use anyhow::Result;
use tokio_postgres::Client;

use crate::queries::{self, quote_regclass_literal};
use crate::types::{ChildPartitionInfo, PartitionKey, PartitionSetInfo, PartitionStrategy};

pub async fn get_partitioned_tables(client: &Client) -> Result<Vec<PartitionSetInfo>> {
    let rows = client.query(queries::QUERY_PARTITIONED_TABLES, &[]).await?;

    let mut tables = Vec::new();
    for row in rows {
        let oid: u32 = row.get(0);
        let schema_name: String = row.get(1);
        let table_name: String = row.get(2);
        let strategy_str: &str = row.get(3);
        let partition_columns: String = row.get(4);

        let strategy = match strategy_str {
            "r" => PartitionStrategy::Range,
            "l" => PartitionStrategy::List,
            "h" => PartitionStrategy::Hash,
            _ => PartitionStrategy::Range,
        };

        let columns = partition_columns
            .split(',')
            .map(|s| s.trim().to_string())
            .collect();

        let full_name = format!("{}.{}", schema_name, table_name);
        let row_count = get_table_row_count(client, &full_name).await.unwrap_or(0);
        let size_bytes = get_table_size(client, &full_name).await.unwrap_or(0);

        let children = get_child_partitions(client, &full_name)
            .await
            .unwrap_or_default();

        let default_partition = get_default_partition(client, &full_name)
            .await
            .ok()
            .flatten();

        let active_child = children
            .iter()
            .max_by_key(|c| c.row_count)
            .map(|c| c.name.clone());

        let table_info = PartitionSetInfo {
            table_oid: oid,
            schema_name,
            table_name,
            full_name,
            strategy,
            partition_key: PartitionKey::new(columns),
            row_count,
            size_bytes,
            child_count: children.len(),
            active_child,
            default_partition,
            premake_count: 0,
        };

        tables.push(table_info);
    }

    Ok(tables)
}

/// Approximate row count via `pg_stat_user_tables.n_live_tup` rather than
/// `SELECT COUNT(*)` — see `queries::get_approximate_row_count` for why.
pub async fn get_table_row_count(client: &Client, table_name: &str) -> Result<i64> {
    queries::get_approximate_row_count(client, table_name).await
}

pub async fn get_table_size(client: &Client, table_name: &str) -> Result<i64> {
    let query = format!(
        "SELECT pg_total_relation_size({})::bigint as size_bytes",
        quote_regclass_literal(table_name)
    );
    let row = client.query_one(&query, &[]).await?;
    Ok(row.get::<_, i64>(0))
}

pub async fn get_child_partitions(
    client: &Client,
    table_name: &str,
) -> Result<Vec<ChildPartitionInfo>> {
    // The bound comes from `pg_class.relpartbound`, not `pg_constraint`. A
    // declaratively-partitioned child carries no CHECK constraint at all — its
    // bound lives only in `relpartbound` — so the older `pg_get_constraintdef`
    // join reported NULL for every partition this tool creates, and the
    // `contype = 'c'` test that went with it could not distinguish a DEFAULT
    // partition from any other (see `get_default_partition`).
    let query = format!(
        r#"
        SELECT
            c.relname as partition_name,
            COALESCE(s.n_live_tup, 0) as row_count,
            pg_total_relation_size(c.oid)::bigint as size_bytes,
            pg_get_expr(c.relpartbound, c.oid) as partition_bound
        FROM pg_inherits i
        JOIN pg_class p ON p.oid = i.inhparent
        JOIN pg_class c ON c.oid = i.inhrelid
        LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid
        WHERE p.oid = {}::regclass
        ORDER BY c.relname
        "#,
        quote_regclass_literal(table_name)
    );

    let rows = client.query(&query, &[]).await?;

    let mut children = Vec::new();
    for row in rows {
        let name: String = row.get(0);
        let row_count: i64 = row.get(1);
        let size_bytes: i64 = row.get(2);
        let constraint_expr: Option<String> = row.get(3);
        let is_default = constraint_expr
            .as_deref()
            .is_some_and(|bound| bound.trim().eq_ignore_ascii_case("DEFAULT"));

        children.push(ChildPartitionInfo {
            name,
            row_count,
            size_bytes,
            constraint_expr,
            is_default,
        });
    }

    Ok(children)
}

/// The DEFAULT partition of `table_name`, if it has one.
///
/// Identified by its bound being literally `DEFAULT`, which is the only thing
/// that actually distinguishes it. The previous test — "the child that has no
/// CHECK constraint" — held for inheritance-based partitioning but is simply
/// false under declarative partitioning: *no* declarative child has a CHECK
/// constraint, since bounds live in `pg_class.relpartbound`. It therefore
/// matched the alphabetically-first partition every time, which
/// `risk::detect_default_partition_strays` would then report as holding stray
/// rows — a false alarm naming an ordinary, correctly-populated partition.
pub async fn get_default_partition(client: &Client, table_name: &str) -> Result<Option<String>> {
    let query = format!(
        r#"
        SELECT c.relname
        FROM pg_inherits i
        JOIN pg_class p ON p.oid = i.inhparent
        JOIN pg_class c ON c.oid = i.inhrelid
        WHERE p.oid = {}::regclass
            AND pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT'
        LIMIT 1
        "#,
        quote_regclass_literal(table_name)
    );

    let rows = client.query(&query, &[]).await?;
    Ok(rows.first().map(|r| r.get::<_, String>(0)))
}

/// Same n_live_tup-based estimate as `get_table_row_count`, applied to a default
/// partition. This matters more here, not less: a default partition that has
/// accumulated a large number of stray rows is exactly the case where a COUNT(*)
/// would be slowest, right when a risk-signal check most needs a fast answer.
pub async fn count_default_partition_rows(
    client: &Client,
    parent_table_name: &str,
    default_partition: &str,
) -> Result<i64> {
    let schema = parent_table_name.split('.').next().unwrap_or("public");
    let qualified = format!("{}.{}", schema, default_partition);
    get_table_row_count(client, &qualified).await
}

pub async fn get_unpartitioned_large_tables(client: &Client) -> Result<Vec<PartitionSetInfo>> {
    let rows = client
        .query(queries::QUERY_UNPARTITIONED_LARGE_TABLES, &[])
        .await?;

    let mut tables = Vec::new();
    for row in rows {
        let _oid: u32 = row.get(0);
        let schema_name: String = row.get(1);
        let table_name: String = row.get(2);
        let row_count: i64 = row.get(3);
        let size_bytes: i64 = row.get(4);

        let full_name = format!("{}.{}", schema_name, table_name);

        tables.push(PartitionSetInfo {
            table_oid: _oid,
            schema_name,
            table_name,
            full_name,
            strategy: PartitionStrategy::Range,
            partition_key: PartitionKey::single("created_at".to_string()),
            row_count,
            size_bytes,
            child_count: 0,
            active_child: None,
            default_partition: None,
            premake_count: 0,
        });
    }

    Ok(tables)
}

pub async fn table_exists(client: &Client, schema: &str, table: &str) -> Result<bool> {
    queries::verify_table_exists(client, schema, table).await
}

/// Reads one table's declarative partitioning shape straight from the live
/// catalog. `Ok(None)` means "not a native partitioned table" — which covers
/// both "doesn't exist" and "exists as an ordinary table"; callers that need
/// to tell those two apart pair this with `table_exists`.
///
/// Unlike `QUERY_PARTITIONED_TABLES`, the key columns are ordered by their
/// position in `partattrs` rather than by `attnum`, so a composite key
/// declared out of column order round-trips faithfully. Expression partition
/// keys (`partattrs` entries of `0`, with the expression living in
/// `partexprs`) have no `pg_attribute` row to join to and are silently
/// dropped from the column list — this tool never creates them, and the
/// callers below only compare keys they themselves declared.
pub async fn get_partition_strategy_and_key(
    client: &Client,
    schema: &str,
    table: &str,
) -> Result<Option<(PartitionStrategy, PartitionKey)>> {
    let query = r#"
        SELECT
            pt.partstrat::text AS strategy,
            (
                SELECT COALESCE(array_agg(a.attname ORDER BY k.ord), ARRAY[]::text[])
                FROM unnest(pt.partattrs::int2[]) WITH ORDINALITY AS k(attnum, ord)
                JOIN pg_attribute a
                    ON a.attrelid = t.oid AND a.attnum = k.attnum
            ) AS partition_columns
        FROM pg_class t
        JOIN pg_namespace n ON n.oid = t.relnamespace
        JOIN pg_partitioned_table pt ON pt.partrelid = t.oid
        WHERE n.nspname = $1 AND t.relname = $2
    "#;

    let row = match client.query_opt(query, &[&schema, &table]).await? {
        Some(row) => row,
        None => return Ok(None),
    };

    let strategy = PartitionStrategy::from_partstrat(row.get::<_, &str>(0))?;
    let columns: Vec<String> = row.get(1);

    Ok(Some((strategy, PartitionKey::new(columns))))
}

/// The `FOR VALUES …` bound of `partition_name`, as PostgreSQL itself renders
/// it (`pg_get_expr(relpartbound, …)`), or `None` when that relation isn't a
/// partition of `parent`. Used to tell "this exact partition already exists"
/// apart from "a differently-bounded partition is squatting on that name".
pub async fn get_partition_bound(
    client: &Client,
    schema: &str,
    parent: &str,
    partition_name: &str,
) -> Result<Option<String>> {
    let query = r#"
        SELECT pg_get_expr(c.relpartbound, c.oid)
        FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        JOIN pg_inherits i ON i.inhrelid = c.oid
        JOIN pg_class p ON p.oid = i.inhparent
        JOIN pg_namespace pn ON pn.oid = p.relnamespace
        WHERE n.nspname = $1 AND c.relname = $3
            AND pn.nspname = $1 AND p.relname = $2
    "#;

    let row = client
        .query_opt(query, &[&schema, &parent, &partition_name])
        .await?;
    Ok(row.and_then(|r| r.get::<_, Option<String>>(0)))
}

pub async fn get_session_timezone(client: &Client) -> Result<String> {
    let row = client
        .query_one(queries::QUERY_SESSION_TIMEZONE, &[])
        .await?;
    Ok(row.get::<_, String>(0))
}
