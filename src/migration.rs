use anyhow::Result;
use tokio_postgres::Client;

use crate::queries;
use crate::retry;
use crate::types::{MigrationConfig, PartitionKey, PartitionStrategy, RetryPolicy};

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

    let strategy_str = config.partition_strategy.as_sql_keyword();

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

/// What a template-creation run would find waiting for it at the target name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateTargetState {
    /// Nothing occupies the target name — create it.
    Absent,
    /// The target is already a partitioned table with exactly the requested
    /// strategy and partition key — nothing to do.
    AlreadyMatches,
    /// Something else already owns the name. The string explains what.
    Conflict(String),
}

fn keys_equal(left: &PartitionKey, right: &PartitionKey) -> bool {
    left.columns.len() == right.columns.len()
        && left
            .columns
            .iter()
            .zip(right.columns.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// Implements the template flow's "create if not exists" rule against the live
/// catalog: absent → create, already-matching → no-op, anything else → hard
/// error. Called twice per run — once at plan time so a conflict is reported
/// before a plan file is even written, and again by the orchestrator at apply
/// time, since an arbitrary amount of time (and other people's DDL) can pass
/// between the two.
pub async fn classify_template_target(
    client: &Client,
    schema: &str,
    table: &str,
    strategy: PartitionStrategy,
    partition_key: &PartitionKey,
) -> Result<TemplateTargetState> {
    match crate::schema::get_partition_strategy_and_key(client, schema, table).await? {
        Some((live_strategy, live_key)) => {
            if live_strategy != strategy {
                return Ok(TemplateTargetState::Conflict(format!(
                    "{}.{} already exists and is partitioned BY {}, not BY {}",
                    schema,
                    table,
                    live_strategy.as_sql_keyword(),
                    strategy.as_sql_keyword()
                )));
            }
            if !keys_equal(&live_key, partition_key) {
                return Ok(TemplateTargetState::Conflict(format!(
                    "{}.{} already exists and is partitioned BY {} ({}), not ({})",
                    schema,
                    table,
                    strategy.as_sql_keyword(),
                    live_key.columns.join(", "),
                    partition_key.columns.join(", ")
                )));
            }
            Ok(TemplateTargetState::AlreadyMatches)
        }
        None => {
            if crate::schema::table_exists(client, schema, table).await? {
                Ok(TemplateTargetState::Conflict(format!(
                    "{}.{} already exists and is not a partitioned table",
                    schema, table
                )))
            } else {
                Ok(TemplateTargetState::Absent)
            }
        }
    }
}

/// Creates a brand-new partitioned table whose column definitions come from a
/// separate template table.
///
/// This is the additive counterpart to `create_partitioned_shadow_table`: no
/// shadow name and no later rename, because nothing is being swapped — the
/// table is created directly under its final name. The template is only ever
/// read (`LIKE`), never locked exclusively, altered, or dropped.
///
/// `INCLUDING DEFAULTS` and nothing more, for the same reason the cutover path
/// gives: pulling in the template's unique/PK constraints or indexes would
/// fail outright whenever they don't include the partition key, which is the
/// normal case for a surrogate-keyed table.
pub async fn create_partitioned_table_from_template(
    client: &Client,
    template_schema: &str,
    template_table: &str,
    target_schema: &str,
    target_table: &str,
    strategy: PartitionStrategy,
    partition_key: &PartitionKey,
    retry_policy: &RetryPolicy,
) -> Result<()> {
    let partition_columns = partition_key
        .columns
        .iter()
        .map(|c| queries::quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");

    let query = format!(
        "CREATE TABLE IF NOT EXISTS {}.{} (LIKE {}.{} INCLUDING DEFAULTS) PARTITION BY {} ({})",
        queries::quote_ident(target_schema),
        queries::quote_ident(target_table),
        queries::quote_ident(template_schema),
        queries::quote_ident(template_table),
        strategy.as_sql_keyword(),
        partition_columns,
    );

    retry::execute_batch_with_retry(
        client,
        "create_partitioned_table_from_template",
        &query,
        retry_policy,
    )
    .await
}

/// Renders values for a `FOR VALUES IN (…)` bound. A value of `NULL` (in any
/// case) is emitted as the SQL null keyword rather than the four-character
/// string — that's the only way to declare the partition that catches null
/// partition-key values, and there is no other way to spell it on a command
/// line. Everything else is quoted as a string literal and left for PostgreSQL
/// to coerce to the partition key's own type, which it does for numeric,
/// uuid, enum, and date/time keys alike.
pub fn format_list_values(values: &[String]) -> String {
    values
        .iter()
        .map(|v| {
            if v.eq_ignore_ascii_case("null") {
                "NULL".to_string()
            } else {
                queries::quote_literal(v)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Adds one `FOR VALUES IN (…)` child to an existing list-partitioned table.
///
/// Neither value-set overlap with a sibling partition nor a type mismatch
/// against the partition key is checked ahead of time: PostgreSQL raises a
/// clear, specific error for both at DDL time, and re-deriving those checks
/// here would only duplicate a constraint the server already enforces
/// authoritatively.
///
/// `IF NOT EXISTS` matches `create_range_partition`/`create_default_partition`,
/// making a re-run after a partial failure safe. Whether the partition that
/// already exists under that name is the *same* partition is settled before we
/// get here, by `validation::validate_add_list_partition`.
pub async fn create_list_partition(
    client: &Client,
    schema: &str,
    table: &str,
    partition_name: &str,
    values: &[String],
    retry_policy: &RetryPolicy,
) -> Result<()> {
    if values.is_empty() {
        return Err(anyhow::anyhow!(
            "cannot create list partition {}.{} with an empty value set",
            schema,
            partition_name
        ));
    }

    let query = format!(
        "CREATE TABLE IF NOT EXISTS {}.{} PARTITION OF {}.{} FOR VALUES IN ({})",
        queries::quote_ident(schema),
        queries::quote_ident(partition_name),
        queries::quote_ident(schema),
        queries::quote_ident(table),
        format_list_values(values),
    );

    retry::execute_batch_with_retry(client, "create_list_partition", &query, retry_policy).await
}

/// Pulls the value set back out of a bound as PostgreSQL renders it —
/// `FOR VALUES IN ('a', 'b')`, `FOR VALUES IN (1, 2)`,
/// `FOR VALUES IN ('shipped'::order_state)` — so an existing partition can be
/// compared against a requested one. Returns `None` for anything that isn't a
/// list bound at all (a range bound, `DEFAULT`, or an unterminated literal).
///
/// Quoting is dropped and any trailing `::type` cast ignored, which leaves one
/// blind spot: an unquoted SQL `NULL` normalizes to `"NULL"`, so a partition
/// holding the *text* value `'NULL'` compares equal to the null partition. That
/// costs a spurious "already present" no-op in a case no real schema hits.
pub fn parse_list_bound_values(bound_expr: &str) -> Option<Vec<String>> {
    const MARKER: &str = "FOR VALUES IN";

    let trimmed = bound_expr.trim();
    let marker_at = trimmed.to_ascii_uppercase().find(MARKER)?;
    let after = trimmed[marker_at + MARKER.len()..].trim_start();
    let inner = after.strip_prefix('(')?;
    let inner = &inner[..inner.rfind(')')?];

    let chars: Vec<char> = inner.chars().collect();
    let mut idx = 0usize;
    let mut values = Vec::new();

    while idx < chars.len() {
        while idx < chars.len() && chars[idx].is_whitespace() {
            idx += 1;
        }
        if idx >= chars.len() {
            break;
        }

        let value = if chars[idx] == '\'' {
            idx += 1;
            let mut literal = String::new();
            loop {
                if idx >= chars.len() {
                    return None; // unterminated literal — not a bound we understand
                }
                if chars[idx] == '\'' {
                    // Doubled quote is an escaped quote, not the terminator.
                    if chars.get(idx + 1) == Some(&'\'') {
                        literal.push('\'');
                        idx += 2;
                    } else {
                        idx += 1;
                        break;
                    }
                } else {
                    literal.push(chars[idx]);
                    idx += 1;
                }
            }
            literal
        } else {
            let mut raw = String::new();
            let mut depth = 0usize;
            while idx < chars.len() {
                match chars[idx] {
                    '(' => depth += 1,
                    ')' => depth = depth.saturating_sub(1),
                    ',' if depth == 0 => break,
                    _ => {}
                }
                raw.push(chars[idx]);
                idx += 1;
            }
            let raw = raw.trim().to_string();
            if raw.eq_ignore_ascii_case("null") {
                "NULL".to_string()
            } else {
                raw
            }
        };

        // Skip whatever trails the value up to the next top-level comma: a
        // `::type` cast, which may itself carry parenthesised, comma-bearing
        // modifiers such as `::numeric(10,2)`.
        let mut depth = 0usize;
        while idx < chars.len() {
            match chars[idx] {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    idx += 1;
                    break;
                }
                _ => {}
            }
            idx += 1;
        }

        values.push(value);
    }

    Some(values)
}

/// Whether an existing partition's rendered bound describes the same value set
/// as `values`. Order-insensitive and duplicate-insensitive, because
/// `FOR VALUES IN ('b', 'a')` and `FOR VALUES IN ('a', 'b')` are the same
/// partition.
pub fn list_bound_matches(existing_bound: &str, values: &[String]) -> bool {
    // Only the requested side needs folding: `parse_list_bound_values` has
    // already mapped an unquoted SQL `NULL` to `"NULL"`, and deliberately left
    // a *quoted* `'null'` alone so the two don't collapse into each other.
    let requested: std::collections::BTreeSet<String> = values
        .iter()
        .map(|v| {
            if v.eq_ignore_ascii_case("null") {
                "NULL".to_string()
            } else {
                v.clone()
            }
        })
        .collect();

    match parse_list_bound_values(existing_bound) {
        Some(existing) => existing.into_iter().collect::<std::collections::BTreeSet<_>>() == requested,
        None => false,
    }
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
    fn test_format_list_values() {
        assert_eq!(
            format_list_values(&["eu-west".to_string(), "us-east".to_string()]),
            "'eu-west', 'us-east'"
        );
        // Numeric/uuid values stay quoted: PostgreSQL coerces the literal to
        // the partition key's own type.
        assert_eq!(format_list_values(&["1".to_string()]), "'1'");
        assert_eq!(format_list_values(&["it's".to_string()]), "'it''s'");
        // `NULL` in any case is the SQL keyword, not the 4-character string.
        assert_eq!(
            format_list_values(&["null".to_string(), "a".to_string()]),
            "NULL, 'a'"
        );
    }

    #[test]
    fn test_parse_list_bound_values() {
        assert_eq!(
            parse_list_bound_values("FOR VALUES IN ('eu-west', 'us-east')"),
            Some(vec!["eu-west".to_string(), "us-east".to_string()])
        );
        // Integer keys render unquoted.
        assert_eq!(
            parse_list_bound_values("FOR VALUES IN (1, 2, 3)"),
            Some(vec!["1".to_string(), "2".to_string(), "3".to_string()])
        );
        // Escaped quotes, and casts that Postgres appends for enum/domain keys.
        assert_eq!(
            parse_list_bound_values("FOR VALUES IN ('it''s')"),
            Some(vec!["it's".to_string()])
        );
        assert_eq!(
            parse_list_bound_values("FOR VALUES IN ('shipped'::order_state, 'held'::order_state)"),
            Some(vec!["shipped".to_string(), "held".to_string()])
        );
        // A cast carrying its own comma-bearing modifier must not split.
        assert_eq!(
            parse_list_bound_values("FOR VALUES IN ('1.0'::numeric(10,2))"),
            Some(vec!["1.0".to_string()])
        );
        // Unquoted NULL normalizes to the keyword.
        assert_eq!(
            parse_list_bound_values("FOR VALUES IN (NULL)"),
            Some(vec!["NULL".to_string()])
        );
        // Non-list bounds aren't list bounds.
        assert_eq!(
            parse_list_bound_values("FOR VALUES FROM ('2026-01-01') TO ('2026-02-01')"),
            None
        );
        assert_eq!(parse_list_bound_values("DEFAULT"), None);
    }

    #[test]
    fn test_list_bound_matches() {
        let values = vec!["eu-west".to_string(), "us-east".to_string()];

        assert!(list_bound_matches(
            "FOR VALUES IN ('eu-west', 'us-east')",
            &values
        ));
        // Order and duplicates don't define a different partition.
        assert!(list_bound_matches(
            "FOR VALUES IN ('us-east', 'eu-west')",
            &values
        ));
        assert!(!list_bound_matches("FOR VALUES IN ('eu-west')", &values));
        assert!(!list_bound_matches("DEFAULT", &values));

        // Numeric values survive the quoted-in/unquoted-out round trip.
        assert!(list_bound_matches(
            "FOR VALUES IN (1, 2)",
            &["1".to_string(), "2".to_string()]
        ));

        // A quoted 'null' is the text value, not the null partition.
        assert!(list_bound_matches("FOR VALUES IN (NULL)", &["null".to_string()]));
        assert!(!list_bound_matches(
            "FOR VALUES IN ('null')",
            &["null".to_string()]
        ));
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
