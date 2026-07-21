use anyhow::{anyhow, Result};
use std::time::Instant;
use tokio_postgres::Client;
use tracing::{error, info, warn};

use crate::index;
use crate::migration;
use crate::queries;
use crate::save::{self, LogStatus};
use crate::types::{ActionType, MigrationConfig, PlanAction, RetryPolicy};

pub struct Orchestrator {
    pub retry_policy: RetryPolicy,
}

impl Orchestrator {
    pub fn new(retry_policy: Option<RetryPolicy>) -> Self {
        Orchestrator {
            retry_policy: retry_policy.unwrap_or_default(),
        }
    }

    pub async fn execute_plan(
        &self,
        client: &Client,
        actions: Vec<PlanAction>,
        migration_config: Option<&MigrationConfig>,
    ) -> Result<Vec<ExecutionResult>> {
        // Ensure audit log table exists
        save::create_logbook_table(client).await?;

        // Computed once for the whole plan (not per-action, not at plan time):
        // AddConstraint and AttachPartition must see the identical boundary
        // value for Postgres's ATTACH fast-path (skip-validation-scan) to work.
        let boundaries: Option<Vec<String>> = match migration_config {
            Some(config) => {
                let start_date = config.start_date.as_deref().ok_or_else(|| {
                    anyhow!("migration_config missing start_date; re-run `plan` to regenerate it")
                })?;
                Some(
                    migration::compute_partition_boundaries(
                        client,
                        &config.interval,
                        config.premake_count,
                        start_date,
                    )
                    .await?,
                )
            }
            None => None,
        };

        let mut results = Vec::new();

        for (idx, action) in actions.iter().enumerate() {
            let start_time = Instant::now();

            let result = self
                .execute_action(client, action, migration_config, boundaries.as_deref())
                .await;

            let duration_ms = start_time.elapsed().as_millis() as i32;

            // Log the action
            let (status, details, error_msg) = match &result {
                Ok(_) => (LogStatus::Success, "Action completed successfully".to_string(), None),
                Err(e) => (LogStatus::Failed, format!("Action failed: {}", e), Some(e.to_string())),
            };

            let log_entry = save::create_log_entry(
                action.action_type.to_string(),
                action.table_name.clone(),
                status,
                details,
            );

            let mut log_with_duration = log_entry;
            log_with_duration.duration_ms = Some(duration_ms);

            save::append_log(client, &log_with_duration).await.ok();

            results.push(ExecutionResult {
                action_id: action.id.clone(),
                status: match &result {
                    Ok(_) => ExecutionStatus::Success,
                    Err(_) => ExecutionStatus::Failed,
                },
                duration_ms,
                error: error_msg,
            });

            // Stop on first failure
            if result.is_err() {
                error!(
                    action_index = idx,
                    action_id = action.id,
                    "Stopping due to action failure"
                );
                return Err(result.err().unwrap());
            }

            info!(
                action_index = idx,
                action_id = action.id,
                duration_ms = duration_ms,
                "Action completed"
            );
        }

        Ok(results)
    }

    async fn execute_action(
        &self,
        client: &Client,
        action: &PlanAction,
        migration_config: Option<&MigrationConfig>,
        boundaries: Option<&[String]>,
    ) -> Result<()> {
        let (schema, table) = split_table_name(&action.table_name)?;

        match action.action_type {
            ActionType::AddConstraint => {
                let config = migration_config.ok_or_else(|| {
                    anyhow!("AddConstraint requires migration_config; re-run `plan` to regenerate it")
                })?;
                let boundaries = boundaries
                    .ok_or_else(|| anyhow!("AddConstraint requires computed partition boundaries"))?;
                let column = config.partition_key.columns.first().ok_or_else(|| {
                    anyhow!("migration_config has no partition key columns")
                })?;
                let constraint_expr =
                    migration::generate_upper_bound_constraint(column, &boundaries[0]);
                migration::add_check_constraint(client, schema, table, &constraint_expr, &self.retry_policy)
                    .await
            }

            ActionType::ValidateConstraint => {
                let constraint_name = format!("{}_partition_check", table);
                migration::validate_constraint(client, schema, table, &constraint_name, &self.retry_policy)
                    .await
            }

            ActionType::CreatePartitionSet => {
                let config = migration_config.ok_or_else(|| {
                    anyhow!("CreatePartitionSet requires migration_config; re-run `plan` to regenerate it")
                })?;
                migration::create_partitioned_shadow_table(client, schema, table, config, &self.retry_policy)
                    .await?;
                Ok(())
            }

            ActionType::AttachPartition => {
                let boundaries = boundaries.ok_or_else(|| {
                    anyhow!("AttachPartition requires migration_config/boundaries; re-run `plan` to regenerate it")
                })?;
                let shadow_name = format!("{}_partitioned_new", table);
                let partition_bounds =
                    format!("FROM (MINVALUE) TO ({})", queries::quote_literal(&boundaries[0]));
                migration::perform_atomic_cutover(
                    client,
                    schema,
                    table,
                    &shadow_name,
                    &partition_bounds,
                    &self.retry_policy,
                )
                .await
            }

            ActionType::CreatePartition => {
                migration::create_default_partition(client, schema, table, &self.retry_policy).await?;

                if let Some(boundaries) = boundaries {
                    for pair in boundaries.windows(2) {
                        let partition_name =
                            migration::date_range_partition_name(table, &pair[0], &pair[1]);
                        migration::create_range_partition(
                            client,
                            schema,
                            table,
                            &partition_name,
                            &pair[0],
                            &pair[1],
                            &self.retry_policy,
                        )
                        .await?;
                    }
                }

                Ok(())
            }

            ActionType::CreateIndex => {
                let legacy_table = format!("{}_legacy", table);
                let defs = index::get_indexes_for_table(client, schema, &legacy_table).await?;

                for def in defs {
                    if def.is_unique {
                        warn!(
                            table = action.table_name,
                            index = def.name,
                            legacy_table = legacy_table,
                            "Skipping unique/PK index during automatic recreation: partitioned \
                             tables require unique indexes to include all partition-key columns; \
                             the original constraint remains enforced only on the legacy child"
                        );
                        continue;
                    }

                    let new_index_name = format!("{}_new", def.name);
                    index::create_index_on_parent(
                        client,
                        schema,
                        table,
                        &new_index_name,
                        &def.columns,
                        false,
                        false,
                        &self.retry_policy,
                    )
                    .await?;
                }

                Ok(())
            }

            ActionType::DropPartition => {
                info!(table = action.table_name, "Dropping partition");
                Ok(())
            }
            ActionType::EnforceRetention => {
                info!(table = action.table_name, "Enforcing retention policy");
                Ok(())
            }
            ActionType::MigrateToPartition
            | ActionType::DetachPartition
            | ActionType::ReconcileDefault
            | ActionType::DropIndex
            | ActionType::RepairIndex => {
                info!(
                    table = action.table_name,
                    action_type = action.action_type.to_string(),
                    "No-op stub for this action type"
                );
                Ok(())
            }
        }
    }
}

fn split_table_name(table_name: &str) -> Result<(&str, &str)> {
    match table_name.splitn(2, '.').collect::<Vec<&str>>().as_slice() {
        [schema, table] => Ok((schema, table)),
        _ => Err(anyhow!("Expected 'schema.table', got: {}", table_name)),
    }
}

pub struct ExecutionResult {
    pub action_id: String,
    pub status: ExecutionStatus,
    pub duration_ms: i32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum ExecutionStatus {
    Success,
    Failed,
}

impl ToString for crate::types::ActionType {
    fn to_string(&self) -> String {
        match self {
            crate::types::ActionType::CreatePartitionSet => "create_partition_set",
            crate::types::ActionType::MigrateToPartition => "migrate_to_partition",
            crate::types::ActionType::AttachPartition => "attach_partition",
            crate::types::ActionType::DetachPartition => "detach_partition",
            crate::types::ActionType::CreatePartition => "create_partition",
            crate::types::ActionType::DropPartition => "drop_partition",
            crate::types::ActionType::EnforceRetention => "enforce_retention",
            crate::types::ActionType::ReconcileDefault => "reconcile_default",
            crate::types::ActionType::CreateIndex => "create_index",
            crate::types::ActionType::DropIndex => "drop_index",
            crate::types::ActionType::RepairIndex => "repair_index",
            crate::types::ActionType::ValidateConstraint => "validate_constraint",
            crate::types::ActionType::AddConstraint => "add_constraint",
        }
        .to_string()
    }
}
