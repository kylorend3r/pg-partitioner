use anyhow::{anyhow, Result};
use std::time::Instant;
use tokio_postgres::Client;
use tracing::{error, info};

use crate::save::{self, LogStatus};
use crate::types::{PlanAction, RetryPolicy};

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
    ) -> Result<Vec<ExecutionResult>> {
        // Ensure audit log table exists
        save::create_logbook_table(client).await?;

        let mut results = Vec::new();

        for (idx, action) in actions.iter().enumerate() {
            let start_time = Instant::now();

            let result = self.execute_action(client, action).await;

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

    async fn execute_action(&self, client: &Client, action: &PlanAction) -> Result<()> {
        match action.action_type {
            crate::types::ActionType::CreatePartitionSet => {
                info!(table = action.table_name, "Creating partition set");
                Ok(())
            }
            crate::types::ActionType::MigrateToPartition => {
                info!(table = action.table_name, "Migrating to partition");
                Ok(())
            }
            crate::types::ActionType::CreatePartition => {
                info!(table = action.table_name, "Creating partition");
                Ok(())
            }
            crate::types::ActionType::DropPartition => {
                info!(table = action.table_name, "Dropping partition");
                Ok(())
            }
            crate::types::ActionType::EnforceRetention => {
                info!(table = action.table_name, "Enforcing retention policy");
                Ok(())
            }
            _ => {
                Err(anyhow!("Unsupported action type: {:?}", action.action_type))
            }
        }
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
