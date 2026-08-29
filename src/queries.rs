use anyhow::Result;
use tokio_postgres::Client;

pub const QUERY_SERVER_VERSION: &str = "SELECT current_setting('server_version_num')::int;";

// Relation-name parameters below are cast `$1::text::regclass`, never bare
// `$1::regclass`. An explicit cast is what Postgres's DESCRIBE step uses to
// infer the parameter's type, so `$1::regclass` reports the parameter as
// `regclass` — and tokio-postgres's `ToSql` for `&str`/`String` accepts only
// text-family OIDs, so binding a Rust string against it fails at runtime.
// Routing through `::text` first pins the parameter to text and lets Postgres
// do the regclass lookup afterwards. Same trap as the `::interval`/`::jsonb`
// cases documented in `migration::compute_partition_boundaries`; for queries
// built with `format!` rather than bound parameters, use
// `quote_regclass_literal` instead.

pub const QUERY_PARTITIONED_TABLES: &str = r#"
    SELECT
        t.oid,
        n.nspname as schema_name,
        t.relname as table_name,
        pt.partstrat::text as strategy,
        string_agg(a.attname, ',' order by a.attnum) as partition_columns
    FROM pg_class t
    JOIN pg_namespace n ON n.oid = t.relnamespace
    JOIN pg_partitioned_table pt ON pt.partrelid = t.oid
    JOIN pg_attribute a ON a.attrelid = t.oid
    WHERE a.attnum = ANY(pt.partattrs)
        AND n.nspname NOT IN ('pg_catalog', 'information_schema')
        AND a.attnum > 0
    GROUP BY t.oid, n.nspname, t.relname, pt.partstrat
    ORDER BY n.nspname, t.relname;
"#;

pub const QUERY_TABLE_SIZE: &str = r#"
    SELECT
        pg_total_relation_size($1::text::regclass)::bigint as size_bytes;
"#;

/// Approximate row count via `pg_stat_user_tables.n_live_tup` rather than
/// `SELECT COUNT(*)`. COUNT(*) forces a full sequential scan — on a large table
/// that can take minutes or never return in the time a status/risk report is
/// willing to wait. n_live_tup is maintained incrementally by the stats
/// collector as DML happens (not just after ANALYZE), so it stays a close,
/// cheap estimate. Used for inspect/risk reporting, none of which need
/// an exact count.
pub const QUERY_TABLE_ROW_COUNT: &str = r#"
    SELECT COALESCE(n_live_tup, 0) as row_count
    FROM pg_stat_user_tables
    WHERE relid = $1::text::regclass;
"#;

pub const QUERY_CHILD_PARTITIONS: &str = r#"
    SELECT
        c.oid,
        c.relname as partition_name,
        pg_total_relation_size(c.oid)::bigint as size_bytes,
        COALESCE(s.n_live_tup, 0) as row_count,
        pg_get_constraintdef(con.oid) as constraint_expr,
        c.relispartition as is_partition
    FROM pg_inherits i
    JOIN pg_class p ON p.oid = i.inhparent
    JOIN pg_class c ON c.oid = i.inhrelid
    LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid
    LEFT JOIN pg_constraint con ON con.conrelid = c.oid
        AND con.contype = 'c'
        AND con.conislocal
    WHERE p.oid = $1::text::regclass
    ORDER BY c.relname;
"#;

pub const QUERY_TABLE_CONSTRAINTS: &str = r#"
    SELECT
        con.oid,
        con.conname as constraint_name,
        con.contype as constraint_type,
        pg_get_constraintdef(con.oid) as definition
    FROM pg_constraint con
    WHERE con.conrelid = $1::text::regclass
        AND con.contype IN ('p', 'u', 'f', 'c', 'x')
    ORDER BY con.contype, con.conname;
"#;

pub const QUERY_TABLE_INDEXES: &str = r#"
    SELECT
        i.oid,
        i.relname as index_name,
        pg_get_indexdef(i.oid) as definition,
        ix.indisunique as is_unique,
        ix.indisprimary as is_primary
    FROM pg_class i
    JOIN pg_index ix ON ix.indexrelid = i.oid
    WHERE ix.indrelid = $1::text::regclass
        AND i.relkind = 'i'
    ORDER BY i.relname;
"#;

pub const QUERY_DEFAULT_PARTITION: &str = r#"
    SELECT
        c.oid,
        c.relname as partition_name
    FROM pg_class p
    JOIN pg_inherits i ON p.oid = i.inhparent
    JOIN pg_class c ON c.oid = i.inhrelid
    WHERE p.oid = $1::text::regclass
        AND i.inhdetachpending = false
        AND pg_get_expr(c.relpartbound, c.oid) = 'DEFAULT'
    LIMIT 1;
"#;

pub const QUERY_PARTITION_STATISTICS: &str = r#"
    SELECT
        schemaname,
        tablename,
        n_live_tup as row_count,
        n_dead_tup as dead_tup_count,
        last_vacuum,
        last_autovacuum
    FROM pg_stat_user_tables
    WHERE relid = $1::text::regclass;
"#;

pub const QUERY_UNIQUE_INDEXES: &str = r#"
    SELECT
        i.oid,
        i.relname as index_name,
        array_agg(a.attname order by a.attnum) as columns
    FROM pg_class i
    JOIN pg_index ix ON ix.indexrelid = i.oid
    JOIN pg_attribute a ON a.attrelid = ix.indrelid
        AND a.attnum = ANY(ix.indkey)
    WHERE ix.indrelid = $1::text::regclass
        AND ix.indisunique
    GROUP BY i.oid, i.relname
    ORDER BY i.relname;
"#;

pub const QUERY_FOREIGN_KEY_DEPENDENTS: &str = r#"
    SELECT DISTINCT
        con.conrelid as dependent_oid,
        c.relname as dependent_table,
        n.nspname as schema_name
    FROM pg_constraint con
    WHERE con.confrelid = $1::text::regclass
        AND con.contype = 'f'
    JOIN pg_class c ON c.oid = con.conrelid
    JOIN pg_namespace n ON n.oid = c.relnamespace;
"#;

pub const QUERY_DEPENDENT_VIEWS: &str = r#"
    SELECT DISTINCT
        d.objid,
        c.relname as view_name,
        n.nspname as schema_name
    FROM pg_depend d
    JOIN pg_class c ON c.oid = d.objid
    JOIN pg_namespace n ON n.oid = c.relnamespace
    WHERE d.refobjid = $1::text::regclass
        AND c.relkind = 'v'
        AND d.deptype = 'n';
"#;

pub const QUERY_UNPARTITIONED_LARGE_TABLES: &str = r#"
    SELECT
        t.oid,
        n.nspname as schema_name,
        t.relname as table_name,
        s.n_live_tup as row_count,
        pg_total_relation_size(t.oid)::bigint as size_bytes
    FROM pg_class t
    JOIN pg_namespace n ON n.oid = t.relnamespace
    LEFT JOIN pg_partitioned_table pt ON pt.partrelid = t.oid
    LEFT JOIN pg_stat_user_tables s ON s.relid = t.oid
    WHERE t.relkind = 'r'
        AND n.nspname NOT IN ('pg_catalog', 'information_schema')
        AND pt.partrelid IS NULL
        AND s.n_live_tup > 10000
    ORDER BY s.n_live_tup DESC;
"#;

pub const QUERY_DATABASE_CHARSET: &str = r#"
    SELECT datcollate as database_collation
    FROM pg_database
    WHERE datname = current_database();
"#;

pub const QUERY_SESSION_TIMEZONE: &str = r#"
    SELECT current_setting('timezone') as tz;
"#;

pub fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace("\"", "\"\""))
}

pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace("'", "''"))
}

/// Renders a possibly schema-qualified relation name as a **string literal**
/// suitable for a `::regclass` cast or a `regclass`-typed argument such as
/// `pg_total_relation_size(…)`.
///
/// This is not interchangeable with `quote_ident`, and reaching for that one
/// here is a silent failure rather than a loud one: `quote_ident("public.events")`
/// yields `"public.events"` — a single double-quoted identifier containing a
/// dot — so `"public.events"::regclass` parses as a cast of a *column
/// reference* and fails with `column "public.events" does not exist`. Callers
/// that wrap such a query in `.unwrap_or_default()` then report a plausible
/// zero instead of an error.
///
/// Each part is quoted as an identifier *inside* the literal, so mixed-case and
/// otherwise-nonstandard names survive regclass's identifier folding:
/// `public.MyTable` becomes `'"public"."MyTable"'`. An unqualified name is
/// left unqualified and resolves through `search_path`, as it would anywhere
/// else.
pub fn quote_regclass_literal(qualified_name: &str) -> String {
    let quoted = match qualified_name.split_once('.') {
        Some((schema, table)) => format!("{}.{}", quote_ident(schema), quote_ident(table)),
        None => quote_ident(qualified_name),
    };

    quote_literal(&quoted)
}

pub async fn get_table_oid(client: &Client, schema: &str, table: &str) -> Result<Option<u32>> {
    let query = r#"
        SELECT oid::int
        FROM pg_class
        WHERE relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = $1)
            AND relname = $2
    "#;

    let rows = client.query(query, &[&schema, &table]).await?;
    Ok(rows.first().map(|r| r.get::<_, i32>(0) as u32))
}

pub async fn verify_table_exists(client: &Client, schema: &str, table: &str) -> Result<bool> {
    let oid = get_table_oid(client, schema, table).await?;
    Ok(oid.is_some())
}

/// Approximate row count for a schema-qualified table via `n_live_tup`. See
/// `QUERY_TABLE_ROW_COUNT` for why this is preferred over `SELECT COUNT(*)`.
/// `table_name` must be schema-qualified (e.g. "public.events") since the
/// lookup casts it to `regclass`, which resolves through `search_path`
/// otherwise.
pub async fn get_approximate_row_count(client: &Client, table_name: &str) -> Result<i64> {
    let row = client.query_opt(QUERY_TABLE_ROW_COUNT, &[&table_name]).await?;
    Ok(row.map(|r| r.get::<_, i64>(0)).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_ident() {
        assert_eq!(quote_ident("simple"), "\"simple\"");
        assert_eq!(quote_ident("with space"), "\"with space\"");
        assert_eq!(quote_ident("with\"quote"), "\"with\"\"quote\"");
    }

    #[test]
    fn test_quote_literal() {
        assert_eq!(quote_literal("simple"), "'simple'");
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    #[test]
    fn test_quote_regclass_literal() {
        // The dot separates identifiers; it must not end up *inside* one.
        assert_eq!(
            quote_regclass_literal("public.events"),
            "'\"public\".\"events\"'"
        );
        // Unqualified names stay unqualified (resolved via search_path).
        assert_eq!(quote_regclass_literal("events"), "'\"events\"'");
        // Quoting each part is what keeps mixed case from being folded away.
        assert_eq!(
            quote_regclass_literal("public.MyTable"),
            "'\"public\".\"MyTable\"'"
        );
        // Embedded quotes are escaped for both layers.
        assert_eq!(
            quote_regclass_literal("public.od'd"),
            "'\"public\".\"od''d\"'"
        );
    }
}
