use anyhow::{anyhow, Result};
use tokio_postgres::Client;
use tracing::info;

use crate::orchestrator::Orchestrator;
use crate::plan;
use crate::types::{Plan, RetryPolicy};

pub struct Applier {
    pub orchestrator: Orchestrator,
}

impl Applier {
    pub fn new(retry_policy: Option<RetryPolicy>) -> Self {
        Applier {
            orchestrator: Orchestrator::new(retry_policy),
        }
    }

    pub async fn apply_plan(
        &self,
        client: &Client,
        schema: &str,
        table: &str,
        plan_obj: &Plan,
    ) -> Result<()> {
        info!(
            schema = schema,
            table = table,
            action_count = plan_obj.actions.len(),
            "Applying plan"
        );

        // First, verify that the live schema hasn't drifted
        let no_drift = plan::verify_plan_drift(client, schema, table, &plan_obj.schema_checksum).await?;

        if !no_drift {
            return Err(anyhow!(
                "Schema drift detected: live schema does not match plan's checksum. \
                 The table structure has changed since plan was computed. \
                 Please re-run 'plan' to generate a new plan against the current schema."
            ));
        }

        info!("Schema checksum matches - proceeding with plan execution");

        // Execute all actions in order through the orchestrator
        let results = self
            .orchestrator
            .execute_plan(client, plan_obj.actions.clone(), plan_obj.migration_config.as_ref())
            .await?;

        // Log summary
        let success_count = results.iter().filter(|r| matches!(r.status, crate::orchestrator::ExecutionStatus::Success)).count();
        let total_duration_ms: i32 = results.iter().map(|r| r.duration_ms).sum();

        info!(
            actions_completed = success_count,
            total_duration_ms = total_duration_ms,
            "Plan execution completed"
        );

        if let Some(config) = &plan_obj.migration_config {
            crate::registrations::create_registrations_table(client).await?;
            let registration = crate::registrations::new_registration(
                schema.to_string(),
                table.to_string(),
                config.partition_strategy,
                config.partition_key.clone(),
                config.interval.clone(),
                config.premake_count,
                config.retention_policy.clone(),
            );
            crate::registrations::upsert_registration(client, &registration).await?;
            info!(
                table = format!("{}.{}", schema, table),
                "Auto-registered table in partitioner_registrations"
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_applier_creation() {
        let applier = Applier::new(None);
        assert_eq!(
            applier.orchestrator.retry_policy.max_attempts,
            5
        );
    }
}
