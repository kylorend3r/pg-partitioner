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

    let create_query = format!(
        "CREATE TABLE {}.{} PARTITION BY {} ({})",
        queries::quote_ident(schema),
        queries::quote_ident(&shadow_name),
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

    let query = format!(
        "ALTER TABLE {}.{} ADD CONSTRAINT {} CHECK {} NOT VALID",
        queries::quote_ident(schema),
        queries::quote_ident(table),
        queries::quote_ident(&constraint_name),
        constraint_expr
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

pub async fn create_default_partition(
    client: &Client,
    schema: &str,
    table: &str,
    retry_policy: &RetryPolicy,
) -> Result<String> {
    let default_name = format!("{}_default", table);

    let query = format!(
        "CREATE TABLE {}.{} PARTITION OF {}.{} DEFAULT",
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
    fn test_generate_check_constraint() {
        let constraint = generate_check_constraint("created_at", "2026-01-01", "2026-02-01");
        assert!(constraint.contains("created_at"));
        assert!(constraint.contains("2026-01-01"));
        assert!(constraint.contains("2026-02-01"));
    }
}
