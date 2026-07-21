use anyhow::Result;
use tokio_postgres::Client;

use crate::queries;
use crate::retry;
use crate::types::{MigrationConfig, RetryPolicy};

pub struct MigrationPlan {
    pub method: MigrationMethod,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum MigrationMethod {
    AttachFirst {
        check_constraint_expr: String,
    },
    BulkCopy {
        batch_size: i32,
        warning: String,
    },
}

pub async fn plan_migration(
    client: &Client,
    schema: &str,
    table: &str,
    config: &MigrationConfig,
) -> Result<MigrationPlan> {
    // Check if table has any data
    let row_count: i64 = crate::schema::get_table_row_count(client, &format!("{}.{}", schema, table))
        .await
        .unwrap_or(0);

    if row_count == 0 {
        return Ok(MigrationPlan {
            method: MigrationMethod::AttachFirst {
                check_constraint_expr: "1=1".to_string(),
            },
            warnings: vec![],
        });
    }

    // For range partitioning, generate a CHECK constraint expression
    let constraint = generate_check_constraint(
        &config.partition_key.columns[0],
        "placeholder_min",
        "placeholder_max",
    );

    // Check for update activity on existing rows
    let has_updates = check_update_activity(client, schema, table).await?;

    let mut warnings = Vec::new();
    if has_updates {
        warnings.push(
            "Table has recent UPDATE activity. If using bulk-copy migration, rows updated \
             during the migration window may not be reflected in the new partitioned structure."
                .to_string(),
        );
    }

    Ok(MigrationPlan {
        method: MigrationMethod::AttachFirst {
            check_constraint_expr: constraint,
        },
        warnings,
    })
}

fn generate_check_constraint(column: &str, min_val: &str, max_val: &str) -> String {
    format!("({} >= '{}' AND {} < '{}')", column, min_val, column, max_val)
}

/// Upper-bound-only CHECK constraint, used for the ATTACH-first cutover path
/// where the legacy chunk's lower bound is `MINVALUE` (unbounded) — a CHECK
/// constraint doesn't need to restate that, only the upper bound the ATTACH
/// itself needs proven.
pub fn generate_upper_bound_constraint(column: &str, upper_val: &str) -> String {
    format!("({} < {})", column, queries::quote_literal(upper_val))
}

/// Computes every period-boundary timestamp from `start_date` through
/// `premake_count` periods past today: `[0]` is `date_trunc(unit, start_date)`
/// — the cutover boundary, above which the one `MINVALUE`-bounded legacy
/// partition catches everything *older* than `start_date` — and `[1..]` are
/// each subsequent real per-period boundary (historical and forward alike;
/// there's no structural distinction between them, both are ordinary
/// `CREATE TABLE ... PARTITION OF ... FOR VALUES FROM/TO` ranges).
///
/// The anchor unit (day/week/month/year) is chosen by a simple substring
/// match on `interval` rather than a full interval parser — this project's
/// own docs and tests only ever illustrate day/week/month/year time-series
/// partitioning. Note this assumes a single-unit interval (`"1 day"`,
/// `"1 month"`, ...): a multi-unit interval like `"2 months"` isn't
/// guaranteed to land the stepped sequence exactly on
/// `date_trunc(unit, now())` — a pre-existing imprecision in the anchor-unit
/// heuristic itself, not new to this function.
pub async fn compute_partition_boundaries(
    client: &Client,
    interval: &str,
    premake_count: usize,
    start_date: &str,
) -> Result<Vec<String>> {
    let anchor_unit = anchor_unit_for_interval(interval);

    let stop_periods = i32::try_from(premake_count + 1)
        .map_err(|_| anyhow::anyhow!("premake_count too large: {}", premake_count))?;

    // Neither `interval` nor `start_date` can be bound parameters here: an
    // explicit `$N::interval`/`$N::timestamptz` cast makes Postgres's
    // DESCRIBE step report that parameter's type as the cast target, and
    // tokio-postgres's `&str`/`String` ToSql only accepts text-family OIDs
    // (the same class of mismatch as binding a String against a
    // `::jsonb`-cast parameter). Embed both as quoted literals instead — the
    // same approach every other DDL-building function in this module already
    // uses for user-influenced string values. `$1` (anchor_unit) has no cast
    // and stays a bound parameter, as before.
    let query = format!(
        r#"
        SELECT (boundary)::text AS boundary
        FROM generate_series(
            date_trunc($1, {start}::timestamptz),
            date_trunc($1, now()) + {stop_periods} * {interval}::interval,
            {interval}::interval
        ) AS boundary
        ORDER BY boundary
        "#,
        start = queries::quote_literal(start_date),
        interval = queries::quote_literal(interval),
        stop_periods = stop_periods,
    );

    let rows = client.query(&query, &[&anchor_unit]).await?;

    Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
}

/// Anchor unit (day/week/month/year) chosen by a simple substring match on
/// `interval` rather than a full interval parser — shared by
/// `compute_partition_boundaries` and `compute_forward_boundaries`.
pub(crate) fn anchor_unit_for_interval(interval: &str) -> &'static str {
    if interval.contains("year") {
        "year"
    } else if interval.contains("month") {
        "month"
    } else if interval.contains("week") {
        "week"
    } else {
        "day"
    }
}

/// Computes `premake_count + 1` period-boundary timestamps anchored on
/// *today*, for ongoing maintenance premake (as opposed to
/// `compute_partition_boundaries`'s one-time historical-to-forward split at
/// initial migration time): `[today's period start, +1 interval, ...,
/// +premake_count intervals]`. Recomputed fresh on every maintenance sweep,
/// so the window naturally slides forward as time passes.
pub async fn compute_forward_boundaries(
    client: &Client,
    interval: &str,
    premake_count: usize,
) -> Result<Vec<String>> {
    let anchor_unit = anchor_unit_for_interval(interval);

    let premake_i32 = i32::try_from(premake_count)
        .map_err(|_| anyhow::anyhow!("premake_count too large: {}", premake_count))?;

    // Same rule as compute_partition_boundaries: `interval` can't be a bound
    // parameter here (an explicit `::interval` cast would make tokio-postgres
    // require a non-text OID for it), so it's embedded as a quoted literal.
    let query = format!(
        r#"
        SELECT (date_trunc($1, now()) + n.i * {interval}::interval)::text AS boundary
        FROM generate_series(0, {premake}) AS n(i)
        ORDER BY n.i
        "#,
        interval = queries::quote_literal(interval),
        premake = premake_i32,
    );

    let rows = client.query(&query, &[&anchor_unit]).await?;

    Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
}

/// Builds a `{table}_{lower_ymd}_{upper_ymd}` partition name (e.g.
/// `products_2026_07_22_2026_07_23`) from two boundary strings. Boundaries
/// returned by `compute_partition_boundaries` are always midnight-truncated
/// timestamps in `"YYYY-MM-DD ..."` form, so the leading 10 bytes are always
/// the date — no full date parsing needed, just a slice and a character swap.
pub fn date_range_partition_name(table: &str, lower: &str, upper: &str) -> String {
    let lower_ymd = lower.get(..10).unwrap_or(lower).replace('-', "_");
    let upper_ymd = upper.get(..10).unwrap_or(upper).replace('-', "_");
    format!("{}_{}_{}", table, lower_ymd, upper_ymd)
}

/// Creates one range partition with an explicit `[lower, upper)` bound —
/// used for the forward-looking partitions created after cutover.
/// `IF NOT EXISTS` makes a retried `apply` (after a partial mid-loop
/// failure) safe to re-run without erroring on partitions that already
/// succeeded.
pub async fn create_range_partition(
    client: &Client,
    schema: &str,
    table: &str,
    partition_name: &str,
    lower: &str,
    upper: &str,
    retry_policy: &RetryPolicy,
) -> Result<()> {
    let query = format!(
        "CREATE TABLE IF NOT EXISTS {}.{} PARTITION OF {}.{} FOR VALUES FROM ({}) TO ({})",
        queries::quote_ident(schema),
        queries::quote_ident(partition_name),
        queries::quote_ident(schema),
        queries::quote_ident(table),
        queries::quote_literal(lower),
        queries::quote_literal(upper),
    );

    retry::execute_batch_with_retry(client, "create_range_partition", &query, retry_policy).await
}

async fn check_update_activity(client: &Client, schema: &str, table: &str) -> Result<bool> {
    let query = r#"
        SELECT n_tup_upd > 0
        FROM pg_stat_user_tables
        WHERE schemaname = $1 AND relname = $2
    "#;

    let rows = client.query(query, &[&schema, &table]).await?;

    if let Some(row) = rows.first() {
        Ok(row.get::<_, bool>(0))
    } else {
        Ok(false)
    }
}

pub async fn create_partitioned_shadow_table(
    client: &Client,
    schema: &str,
    table: &str,
    config: &MigrationConfig,
    retry_policy: &RetryPolicy,
) -> Result<String> {
    let shadow_name = format!("{}_partitioned_new", table);
    let partition_columns = config.partition_key.columns.join(", ");

    let strategy_str = match config.partition_strategy {
        crate::types::PartitionStrategy::Range => "RANGE",
        crate::types::PartitionStrategy::List => "LIST",
        crate::types::PartitionStrategy::Hash => "HASH",
    };

    // `LIKE ... INCLUDING DEFAULTS` (not `INCLUDING ALL`/`INCLUDING
    // CONSTRAINTS`/`INCLUDING INDEXES`): a partitioned table's own column
    // list can't be omitted (a bare `PARTITION BY` with no column list is a
    // syntax error), but pulling in the source's unique/PK constraints or
    // indexes here would fail outright if they don't include the partition
    // key (exactly the case the CreateIndex step already works around by
    // skipping them) — so only column definitions/types/defaults are copied;
    // NOT NULL is copied regardless, as it's inherent to the column
    // definition rather than gated by an INCLUDING option.
    let create_query = format!(
        "CREATE TABLE {}.{} (LIKE {}.{} INCLUDING DEFAULTS) PARTITION BY {} ({})",
        queries::quote_ident(schema),
        queries::quote_ident(&shadow_name),
        queries::quote_ident(schema),
        queries::quote_ident(table),
        strategy_str,
        partition_columns
    );

    retry::execute_batch_with_retry(client, "create_shadow_table", &create_query, retry_policy)
        .await?;

    Ok(shadow_name)
}

pub async fn add_check_constraint(
    client: &Client,
    schema: &str,
    table: &str,
    constraint_expr: &str,
    retry_policy: &RetryPolicy,
) -> Result<()> {
    let constraint_name = format!("{}_partition_check", table);
    let schema_q = queries::quote_ident(schema);
    let table_q = queries::quote_ident(table);
    let constraint_q = queries::quote_ident(&constraint_name);

    // DROP...IF EXISTS first so a retried `apply` (after a later step in the
    // same sequence failed) can safely re-add this constraint rather than
    // erroring on "constraint already exists".
    let query = format!(
        "ALTER TABLE {schema_q}.{table_q} DROP CONSTRAINT IF EXISTS {constraint_q}; \
         ALTER TABLE {schema_q}.{table_q} ADD CONSTRAINT {constraint_q} CHECK {constraint_expr} NOT VALID"
    );

    retry::execute_batch_with_retry(client, "add_check_constraint", &query, retry_policy).await
}

/// Runs as its own batch (not folded into `add_check_constraint`) because
/// `VALIDATE CONSTRAINT` only needs `SHARE UPDATE EXCLUSIVE` — bundling it with
/// the `ADD CONSTRAINT ... NOT VALID` step would hold that statement's stronger
/// lock for the duration of the validation scan instead of releasing it first.
pub async fn validate_constraint(
    client: &Client,
    schema: &str,
    table: &str,
    constraint_name: &str,
    retry_policy: &RetryPolicy,
) -> Result<()> {
    let query = format!(
        "ALTER TABLE {}.{} VALIDATE CONSTRAINT {}",
        queries::quote_ident(schema),
        queries::quote_ident(table),
        queries::quote_ident(constraint_name)
    );

    retry::execute_batch_with_retry(client, "validate_constraint", &query, retry_policy).await
}

/// Performs the rename-rename-ATTACH cutover in one literal transaction, run
/// through the simple query protocol (`batch_execute`) since it's more than one
/// statement — `Client::execute`/`.transaction()` can't run this as a single
/// prepared call.
///
/// `LOCK TABLE ... IN ACCESS EXCLUSIVE MODE` is issued explicitly, right after
/// `BEGIN`, for both the source and shadow table together, even though the
/// `ALTER TABLE ... RENAME` and `ATTACH PARTITION` statements that follow would
/// each acquire that same lock implicitly on their own. Two reasons to take it
/// upfront instead of letting each statement acquire it as it goes:
///
/// 1. **Fail fast, mutate nothing.** Without the explicit lock, it's possible to
///    successfully rename the source table aside and then block waiting for the
///    lock on the shadow table's rename. That's still safe (the whole thing is
///    one transaction — rollback undoes the first rename too), but it means the
///    transaction can sit holding an exclusive lock on the now-renamed source
///    table for up to `lock_timeout` while queued behind unrelated traffic,
///    which is exactly the "hangs and blocks everything behind it" failure mode
///    `locking_and_retries.md` is about. Taking both locks in one `LOCK TABLE`
///    statement means either we get both immediately or we back off and retry
///    the whole attempt before touching either table.
/// 2. **Documents the actual requirement.** A reader of this transaction should
///    not have to know that `RENAME`/`ATTACH PARTITION` imply `ACCESS EXCLUSIVE`
///    to understand what this cutover depends on.
///
/// `SET LOCAL lock_timeout`/`statement_timeout` are embedded in the same batch
/// (right after `BEGIN`, before the lock) rather than set in a prior statement,
/// because `SET LOCAL` only lasts for the transaction it runs in — set outside
/// this literal `BEGIN`/`COMMIT`, it would have no effect on any of it.
pub async fn perform_atomic_cutover(
    client: &Client,
    schema: &str,
    table: &str,
    shadow_name: &str,
    partition_bounds: &str,
    retry_policy: &RetryPolicy,
) -> Result<()> {
    let legacy_name = format!("{}_legacy", table);

    let schema_quoted = queries::quote_ident(schema);
    let table_quoted = queries::quote_ident(table);
    let shadow_quoted = queries::quote_ident(shadow_name);
    let legacy_quoted = queries::quote_ident(&legacy_name);

    let sql = format!(
        "BEGIN; \
        SET LOCAL lock_timeout = '{lock_timeout_ms}ms'; \
        SET LOCAL statement_timeout = '{statement_timeout_ms}ms'; \
        LOCK TABLE {schema}.{table}, {schema}.{shadow} IN ACCESS EXCLUSIVE MODE; \
        ALTER TABLE {schema}.{table} RENAME TO {legacy}; \
        ALTER TABLE {schema}.{shadow} RENAME TO {table}; \
        ALTER TABLE {schema}.{table} ATTACH PARTITION {schema}.{legacy} FOR VALUES {bounds}; \
        COMMIT;",
        lock_timeout_ms = retry_policy.lock_timeout_ms,
        statement_timeout_ms = retry_policy.statement_timeout_ms,
        schema = schema_quoted,
        table = table_quoted,
        shadow = shadow_quoted,
        legacy = legacy_quoted,
        bounds = partition_bounds,
    );

    retry::execute_batch_with_retry(client, "atomic_cutover", &sql, retry_policy).await
}

/// Detaches `partition_name` from `parent_table`, taking `ACCESS EXCLUSIVE`
/// on the parent explicitly before the `DETACH PARTITION` itself, and
/// releasing it via the transaction's own `COMMIT` — same fail-fast-and-
/// document-the-requirement reasoning as `perform_atomic_cutover`'s explicit
/// `LOCK TABLE`: either the lock is acquired immediately (or within
/// `lock_timeout`, with retry/backoff on contention) or nothing happens,
/// rather than the plain `DETACH PARTITION` statement silently queuing
/// behind unrelated traffic while holding no visible intent.
pub async fn detach_partition(
    client: &Client,
    schema: &str,
    parent_table: &str,
    partition_name: &str,
    retry_policy: &RetryPolicy,
) -> Result<()> {
    let schema_quoted = queries::quote_ident(schema);
    let parent_quoted = queries::quote_ident(parent_table);
    let partition_quoted = queries::quote_ident(partition_name);

    let sql = format!(
        "BEGIN; \
        SET LOCAL lock_timeout = '{lock_timeout_ms}ms'; \
        SET LOCAL statement_timeout = '{statement_timeout_ms}ms'; \
        LOCK TABLE {schema}.{parent} IN ACCESS EXCLUSIVE MODE; \
        ALTER TABLE {schema}.{parent} DETACH PARTITION {schema}.{partition}; \
        COMMIT;",
        lock_timeout_ms = retry_policy.lock_timeout_ms,
        statement_timeout_ms = retry_policy.statement_timeout_ms,
        schema = schema_quoted,
        parent = parent_quoted,
        partition = partition_quoted,
    );

    retry::execute_batch_with_retry(client, "detach_partition", &sql, retry_policy).await
}

pub async fn create_default_partition(
    client: &Client,
    schema: &str,
    table: &str,
    retry_policy: &RetryPolicy,
) -> Result<String> {
    let default_name = format!("{}_default", table);

    let query = format!(
        "CREATE TABLE IF NOT EXISTS {}.{} PARTITION OF {}.{} DEFAULT",
        queries::quote_ident(schema),
        queries::quote_ident(&default_name),
        queries::quote_ident(schema),
        queries::quote_ident(table)
    );

    retry::execute_batch_with_retry(client, "create_default_partition", &query, retry_policy)
        .await?;

    Ok(default_name)
}

/// Batch moves also go through the retry/lock-timeout path: the destination
/// child can be concurrently written to (e.g. new rows still arriving during a
/// `reconcile-default` sweep), so this DML is just as capable of blocking on a
/// row lock as the DDL actions are of blocking on a table lock.
///
/// This is a single statement (one `WITH ... INSERT`), so unlike the cutover's
/// multi-statement batch, it runs through `execute_with_retry`'s prepared-
/// statement path instead of `execute_batch_with_retry` — that path properly
/// scopes `SET LOCAL` via a real transaction *and* returns the actual affected
/// row count, which callers need to track backfill/reconciliation progress.
pub async fn atomic_batch_move(
    client: &mut Client,
    source_table: &str,
    destination_table: &str,
    batch_bounds: Option<&str>,
    retry_policy: &RetryPolicy,
) -> Result<u64> {
    let where_clause = batch_bounds
        .map(|b| format!("WHERE {}", b))
        .unwrap_or_default();

    let query = format!(
        "WITH moved AS (\
            DELETE FROM {} {} \
            RETURNING * \
        ) \
        INSERT INTO {} SELECT * FROM moved",
        source_table, where_clause, destination_table
    );

    let action = retry::RetryableAction {
        name: "atomic_batch_move",
        query: &query,
        params: vec![],
    };

    retry::execute_with_retry(client, &action, retry_policy).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anchor_unit_for_interval() {
        assert_eq!(anchor_unit_for_interval("1 year"), "year");
        assert_eq!(anchor_unit_for_interval("1 month"), "month");
        assert_eq!(anchor_unit_for_interval("1 week"), "week");
        assert_eq!(anchor_unit_for_interval("1 day"), "day");
        assert_eq!(anchor_unit_for_interval("7 days"), "day");
    }

    #[test]
    fn test_generate_check_constraint() {
        let constraint = generate_check_constraint("created_at", "2026-01-01", "2026-02-01");
        assert!(constraint.contains("created_at"));
        assert!(constraint.contains("2026-01-01"));
        assert!(constraint.contains("2026-02-01"));
    }

    #[test]
    fn test_generate_upper_bound_constraint() {
        let constraint = generate_upper_bound_constraint("created_at", "2026-08-01 00:00:00");
        assert_eq!(constraint, "(created_at < '2026-08-01 00:00:00')");
    }

    #[test]
    fn test_date_range_partition_name() {
        assert_eq!(
            date_range_partition_name("products", "2026-07-22 00:00:00+00", "2026-07-23 00:00:00+00"),
            "products_2026_07_22_2026_07_23"
        );
        // Also handles bare date strings (no time-of-day component) safely.
        assert_eq!(
            date_range_partition_name("products", "2026-07-22", "2026-07-23"),
            "products_2026_07_22_2026_07_23"
        );
    }

    #[test]
    fn test_boundaries_windows_pairing() {
        // Mirrors how orchestrator.rs turns `compute_partition_boundaries`'
        // output into forward-partition (lower, upper) pairs via `.windows(2)`.
        let boundaries = vec![
            "2026-08-01".to_string(),
            "2026-09-01".to_string(),
            "2026-10-01".to_string(),
            "2026-11-01".to_string(),
        ];

        let pairs: Vec<(&String, &String)> = boundaries
            .windows(2)
            .map(|w| (&w[0], &w[1]))
            .collect();

        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], (&"2026-08-01".to_string(), &"2026-09-01".to_string()));
        assert_eq!(pairs[2], (&"2026-10-01".to_string(), &"2026-11-01".to_string()));
    }
}
