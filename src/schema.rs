use anyhow::Result;
use tokio_postgres::Client;

use crate::queries::{self, quote_ident};
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
        quote_ident(table_name)
    );
    let row = client.query_one(&query, &[]).await?;
    Ok(row.get::<_, i64>(0))
}

pub async fn get_child_partitions(
    client: &Client,
    table_name: &str,
) -> Result<Vec<ChildPartitionInfo>> {
    let query = format!(
        r#"
        SELECT
            c.relname as partition_name,
            COALESCE(s.n_live_tup, 0) as row_count,
            pg_total_relation_size(c.oid)::bigint as size_bytes,
            pg_get_constraintdef(con.oid) as constraint_expr
        FROM pg_inherits i
        JOIN pg_class p ON p.oid = i.inhparent
        JOIN pg_class c ON c.oid = i.inhrelid
        LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid
        LEFT JOIN pg_constraint con ON con.conrelid = c.oid
            AND con.contype = 'c'
            AND con.conislocal
        WHERE p.oid = {}::regclass
        ORDER BY c.relname
        "#,
        quote_ident(table_name)
    );

    let rows = client.query(&query, &[]).await?;

    let mut children = Vec::new();
    for row in rows {
        let name: String = row.get(0);
        let row_count: i64 = row.get(1);
        let size_bytes: i64 = row.get(2);
        let constraint_expr: Option<String> = row.get(3);

        children.push(ChildPartitionInfo {
            name,
            row_count,
            size_bytes,
            constraint_expr,
            is_default: false,
        });
    }

    Ok(children)
}

pub async fn get_default_partition(client: &Client, table_name: &str) -> Result<Option<String>> {
    let query = format!(
        r#"
        SELECT c.relname
        FROM pg_inherits i
        JOIN pg_class p ON p.oid = i.inhparent
        JOIN pg_class c ON c.oid = i.inhrelid
        WHERE p.oid = {}::regclass
            AND NOT EXISTS (
                SELECT 1 FROM pg_constraint con
                WHERE con.conrelid = c.oid
                    AND con.contype = 'c'
            )
        LIMIT 1
        "#,
        quote_ident(table_name)
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

pub async fn get_session_timezone(client: &Client) -> Result<String> {
    let row = client
        .query_one(queries::QUERY_SESSION_TIMEZONE, &[])
        .await?;
    Ok(row.get::<_, String>(0))
}
