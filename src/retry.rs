use anyhow::{anyhow, Result};
use std::time::Duration;
use tokio::time::sleep;
use tokio_postgres::Client;
use tracing::{error, info, warn};

use crate::types::RetryPolicy;

const DEFAULT_LOCK_TIMEOUT_MS: u32 = 3000;
const DEFAULT_STATEMENT_TIMEOUT_MS: u32 = 30000;
const DEFAULT_MAX_ATTEMPTS: u32 = 5;
const DEFAULT_BACKOFF_BASE_MS: u32 = 200;
const DEFAULT_BACKOFF_MAX_MS: u32 = 10000;

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            lock_timeout_ms: DEFAULT_LOCK_TIMEOUT_MS,
            statement_timeout_ms: DEFAULT_STATEMENT_TIMEOUT_MS,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff_base_ms: DEFAULT_BACKOFF_BASE_MS,
            backoff_max_ms: DEFAULT_BACKOFF_MAX_MS,
            jitter: true,
        }
    }
}

pub struct RetryableAction<'a> {
    pub name: &'a str,
    pub query: &'a str,
    pub params: Vec<&'a (dyn tokio_postgres::types::ToSql + Sync)>,
}

/// Runs a single prepared statement with `SET LOCAL lock_timeout`/`statement_timeout`
/// actually in effect for it, retrying on transient lock contention.
///
/// This requires `&mut Client` because `SET LOCAL` only lasts for the duration of
/// the transaction it runs in. A naive implementation that calls `client.execute()`
/// for the `SET LOCAL` and then again for the real statement runs each as its own
/// implicitly-autocommitted transaction — the setting is gone before the statement
/// that needs it ever runs. Wrapping both in one `client.transaction()` is what
/// makes the timeout actually apply.
pub async fn execute_with_retry(
    client: &mut Client,
    action: &RetryableAction<'_>,
    policy: &RetryPolicy,
) -> Result<u64> {
    let mut attempt = 0;

    loop {
        attempt += 1;

        let txn = client
            .transaction()
            .await
            .map_err(|e| anyhow!("Failed to start transaction for '{}': {}", action.name, e))?;

        if let Err(e) = set_timeouts_txn(&txn, policy).await {
            txn.rollback().await.ok();
            return Err(e);
        }

        let result = execute_query_txn(&txn, action).await;

        match result {
            Ok(rows_affected) => {
                txn.commit()
                    .await
                    .map_err(|e| anyhow!("Failed to commit '{}': {}", action.name, e))?;

                if attempt > 1 {
                    info!(
                        action = action.name,
                        attempts = attempt,
                        "Action succeeded after retries"
                    );
                }
                return Ok(rows_affected);
            }

            Err(e) => {
                // Transaction was never committed, so it rolls back on drop; no
                // rows or timeout settings from this attempt leak into the next one.
                drop(txn);

                let error_code = extract_sqlstate(&e);

                // Lock not available (55P03) - retryable
                if error_code.as_deref() == Some("55P03") {
                    if attempt < policy.max_attempts {
                        let backoff_ms = calculate_backoff(attempt - 1, policy);
                        warn!(
                            action = action.name,
                            attempt = attempt,
                            max_attempts = policy.max_attempts,
                            backoff_ms = backoff_ms,
                            error = %describe_chain(&e),
                            "Lock not acquired, retrying"
                        );
                        sleep(Duration::from_millis(backoff_ms as u64)).await;
                        continue;
                    }
                }

                // Deadlock (40P01) - retryable but log more loudly
                if error_code.as_deref() == Some("40P01") {
                    if attempt < policy.max_attempts {
                        let backoff_ms = calculate_backoff(attempt - 1, policy);
                        error!(
                            action = action.name,
                            attempt = attempt,
                            max_attempts = policy.max_attempts,
                            backoff_ms = backoff_ms,
                            error = %describe_chain(&e),
                            "Deadlock detected, retrying (indicates conflicting lock orders)"
                        );
                        sleep(Duration::from_millis(backoff_ms as u64)).await;
                        continue;
                    }
                }

                // Any other error - fail immediately
                return Err(anyhow!(
                    "Action '{}' failed (attempt {}): {} (SQLSTATE: {})",
                    action.name,
                    attempt,
                    describe_chain(&e),
                    error_code.unwrap_or_else(|| "unknown".to_string())
                ));
            }
        }
    }
}

/// Runs a raw, possibly multi-statement SQL batch (its own `BEGIN`/`COMMIT`,
/// `LOCK TABLE`, etc. embedded in the text) through the simple query protocol,
/// with the same retry/backoff policy as `execute_with_retry`. Needed because
/// tokio-postgres's prepared-statement path (`Client::execute`) rejects strings
/// containing more than one statement — `client.transaction()` can't be used
/// here either, since the caller wants explicit control over statement order
/// (e.g. `LOCK TABLE` before any `ALTER TABLE`) inside its own literal
/// `BEGIN`/`COMMIT` rather than a driver-managed transaction.
pub async fn execute_batch_with_retry(
    client: &Client,
    name: &str,
    sql: &str,
    policy: &RetryPolicy,
) -> Result<()> {
    let mut attempt = 0;

    loop {
        attempt += 1;

        match client.batch_execute(sql).await {
            Ok(_) => {
                if attempt > 1 {
                    info!(action = name, attempts = attempt, "Batch succeeded after retries");
                }
                return Ok(());
            }

            Err(e) => {
                // If the batch failed after its literal BEGIN but before COMMIT,
                // the session is left in an aborted-transaction state; a bare
                // retry would immediately fail with "current transaction is
                // aborted" rather than re-attempting the actual operation. This
                // resets the session regardless of whether one was actually left
                // open (a no-op ROLLBACK outside a transaction is harmless).
                client.batch_execute("ROLLBACK;").await.ok();

                let error_code = e.code().map(|c| c.code().to_string());

                if error_code.as_deref() == Some("55P03") {
                    if attempt < policy.max_attempts {
                        let backoff_ms = calculate_backoff(attempt - 1, policy);
                        warn!(
                            action = name,
                            attempt = attempt,
                            max_attempts = policy.max_attempts,
                            backoff_ms = backoff_ms,
                            error = %describe(&e),
                            "Lock not acquired, retrying batch"
                        );
                        sleep(Duration::from_millis(backoff_ms as u64)).await;
                        continue;
                    }
                }

                if error_code.as_deref() == Some("40P01") {
                    if attempt < policy.max_attempts {
                        let backoff_ms = calculate_backoff(attempt - 1, policy);
                        error!(
                            action = name,
                            attempt = attempt,
                            max_attempts = policy.max_attempts,
                            backoff_ms = backoff_ms,
                            error = %describe(&e),
                            "Deadlock detected, retrying batch (indicates conflicting lock orders)"
                        );
                        sleep(Duration::from_millis(backoff_ms as u64)).await;
                        continue;
                    }
                }

                return Err(anyhow!(
                    "Batch action '{}' failed (attempt {}): {} (SQLSTATE: {})",
                    name,
                    attempt,
                    describe(&e),
                    error_code.unwrap_or_else(|| "unknown".to_string())
                ));
            }
        }
    }
}

async fn set_timeouts_txn(txn: &tokio_postgres::Transaction<'_>, policy: &RetryPolicy) -> Result<()> {
    let lock_timeout_query = format!("SET LOCAL lock_timeout = '{}ms'", policy.lock_timeout_ms);
    txn.execute(&lock_timeout_query, &[]).await?;

    let stmt_timeout_query = format!(
        "SET LOCAL statement_timeout = '{}ms'",
        policy.statement_timeout_ms
    );
    txn.execute(&stmt_timeout_query, &[]).await?;

    Ok(())
}

async fn execute_query_txn(
    txn: &tokio_postgres::Transaction<'_>,
    action: &RetryableAction<'_>,
) -> Result<u64> {
    let stmt = txn.prepare(action.query).await?;
    let rows_affected = txn.execute(&stmt, &action.params).await?;
    Ok(rows_affected)
}

fn calculate_backoff(attempt_index: u32, policy: &RetryPolicy) -> u32 {
    let base_backoff = (policy.backoff_base_ms as f64
        * 2_f64.powf(attempt_index as f64)) as u32;
    let capped_backoff = base_backoff.min(policy.backoff_max_ms);

    if policy.jitter {
        // Add up to 25% jitter
        let jitter_amount = (capped_backoff as f64 * 0.25) as u32;
        let random_jitter = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % (jitter_amount as u32)) as u32;
        capped_backoff + random_jitter
    } else {
        capped_backoff
    }
}

fn extract_sqlstate(error: &anyhow::Error) -> Option<String> {
    db_error_from_chain(error).and_then(|e| e.code().map(|c| c.code().to_string()))
}

/// Finds the driver error inside an `anyhow` chain.
///
/// `execute_with_retry` works with `anyhow::Error` while `execute_batch_with_retry`
/// holds the `tokio_postgres::Error` directly, so everything that wants the
/// server's own reporting has to come back through here first.
fn db_error_from_chain(error: &anyhow::Error) -> Option<&tokio_postgres::error::Error> {
    error
        .chain()
        .find_map(|e| e.downcast_ref::<tokio_postgres::error::Error>())
}

/// Assembles the parts of a server error report into one line.
///
/// Pure on purpose: `DbError` has no public constructor, so the only way to test
/// this rendering is to keep it separate from the field extraction that reads it
/// (CLAUDE.md §4.1). `format_db_error` below is the two-line adapter.
fn render_db_error_parts(
    message: &str,
    detail: Option<&str>,
    hint: Option<&str>,
    relation: Option<&str>,
    constraint: Option<&str>,
) -> String {
    let mut rendered = message.to_string();

    // Ordered the way psql prints them, most specific context last.
    if let Some(relation) = relation {
        match constraint {
            Some(constraint) => rendered.push_str(&format!(
                " [relation {}, constraint {}]",
                relation, constraint
            )),
            None => rendered.push_str(&format!(" [relation {}]", relation)),
        }
    } else if let Some(constraint) = constraint {
        rendered.push_str(&format!(" [constraint {}]", constraint));
    }

    if let Some(detail) = detail {
        rendered.push_str(&format!(" DETAIL: {}", detail));
    }

    if let Some(hint) = hint {
        rendered.push_str(&format!(" HINT: {}", hint));
    }

    rendered
}

/// What the server actually said.
///
/// `tokio_postgres::Error`'s own `Display` is the fixed string "db error" — the
/// message, DETAIL, HINT and the relation or constraint involved all hang off
/// `as_db_error()` and are otherwise dropped. That is why a failed cutover used
/// to report nothing beyond `SQLSTATE: 23514`, leaving an operator to reproduce
/// the DDL by hand to find out what was wrong.
///
/// Note this can carry data values: PostgreSQL puts the offending row into
/// DETAIL for some constraint violations, and that reaches the log file and
/// `partitioner_logbook`. Kept, because it is usually the line that identifies
/// the problem row.
fn format_db_error(error: &tokio_postgres::error::Error) -> Option<String> {
    let db_error = error.as_db_error()?;

    Some(render_db_error_parts(
        db_error.message(),
        db_error.detail(),
        db_error.hint(),
        db_error.table(),
        db_error.constraint(),
    ))
}

/// The driver error rendered for a human, falling back to its `Display` when the
/// failure was not a server-side error at all (a broken connection, say).
fn describe(error: &tokio_postgres::error::Error) -> String {
    format_db_error(error).unwrap_or_else(|| error.to_string())
}

/// Same, for the `anyhow`-wrapped side of the module.
fn describe_chain(error: &anyhow::Error) -> String {
    db_error_from_chain(error)
        .and_then(format_db_error)
        .unwrap_or_else(|| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_db_error_parts_message_only() {
        assert_eq!(
            render_db_error_parts("relation \"events\" does not exist", None, None, None, None),
            "relation \"events\" does not exist"
        );
    }

    #[test]
    fn test_render_db_error_parts_includes_every_field_the_server_sent() {
        // The real shape of the failure this was written for: a violated
        // partition constraint names the relation, and DETAIL identifies the row.
        let rendered = render_db_error_parts(
            "partition constraint is violated by some row",
            Some("Failing row contains (1, null)"),
            Some("Add a NOT NULL constraint"),
            Some("orders_legacy"),
            Some("orders_partition_check"),
        );

        assert!(rendered.starts_with("partition constraint is violated by some row"));
        for expected in [
            "orders_legacy",
            "orders_partition_check",
            "Failing row contains",
            "Add a NOT NULL constraint",
        ] {
            assert!(
                rendered.contains(expected),
                "{} missing from {}",
                expected,
                rendered
            );
        }
    }

    #[test]
    fn test_render_db_error_parts_omits_absent_fields() {
        // A server error with no DETAIL must not render an empty "DETAIL:".
        let rendered = render_db_error_parts("deadlock detected", None, None, Some("events"), None);
        assert!(rendered.contains("events"), "{}", rendered);
        assert!(!rendered.contains("DETAIL"), "{}", rendered);
        assert!(!rendered.contains("HINT"), "{}", rendered);
        assert!(!rendered.contains("constraint"), "{}", rendered);
    }

    #[test]
    fn test_calculate_backoff_no_jitter() {
        let policy = RetryPolicy {
            backoff_base_ms: 100,
            backoff_max_ms: 5000,
            jitter: false,
            ..Default::default()
        };

        assert_eq!(calculate_backoff(0, &policy), 100);
        assert_eq!(calculate_backoff(1, &policy), 200);
        assert_eq!(calculate_backoff(2, &policy), 400);
        assert_eq!(calculate_backoff(3, &policy), 800);
        assert_eq!(calculate_backoff(4, &policy), 1600);
        assert_eq!(calculate_backoff(5, &policy), 3200);
        assert_eq!(calculate_backoff(6, &policy), 5000); // capped
    }

    #[test]
    fn test_calculate_backoff_with_jitter() {
        let policy = RetryPolicy {
            backoff_base_ms: 100,
            backoff_max_ms: 5000,
            jitter: true,
            ..Default::default()
        };

        let backoff = calculate_backoff(0, &policy);
        assert!(backoff >= 100 && backoff <= 125); // 100 + up to 25 jitter
    }
}
