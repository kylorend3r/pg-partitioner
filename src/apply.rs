use anyhow::{anyhow, Result};
use tokio_postgres::Client;
use tracing::info;

use crate::orchestrator::Orchestrator;
use crate::plan;
use crate::types::{ActionType, Plan, PlanAction, RetryPolicy};

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

        // First, verify that the live schema hasn't drifted. Usually that means
        // the table the actions target, but a template-creation plan checksums
        // its template instead — its target doesn't exist yet, and the template
        // is what the target's structure will be copied from.
        let (checksum_schema, checksum_table) = match plan_obj.checksum_table.as_deref() {
            Some(qualified) => split_qualified_name(qualified)?,
            None => (schema.to_string(), table.to_string()),
        };

        let no_drift = plan::verify_plan_drift(
            client,
            &checksum_schema,
            &checksum_table,
            &plan_obj.schema_checksum,
        )
        .await?;

        if !no_drift {
            return Err(anyhow!(
                "Schema drift detected: live schema of {}.{} does not match plan's checksum. \
                 The table structure has changed since plan was computed. \
                 Please re-run 'plan' to generate a new plan against the current schema.",
                checksum_schema,
                checksum_table
            ));
        }

        info!("Schema checksum matches - proceeding with plan execution");

        // The list order is what actually executes; `sequence` only states it.
        // Checking them against each other is what stops the field becoming a
        // second source of truth — a hand-edited plan whose numbers disagree is
        // refused rather than quietly running in an order it doesn't claim.
        verify_action_order(&plan_obj.actions)?;

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

        // Only a plan that stands up a partition *set* declares a table as
        // managed. An `add-partition` plan touches one child of an
        // already-known parent, so re-registering there would overwrite that
        // parent's real interval/premake/retention settings with the inert
        // placeholders such a plan carries.
        let creates_partition_set = plan_obj
            .actions
            .iter()
            .any(|a| matches!(a.action_type, ActionType::CreatePartitionSet));

        if let (true, Some(config)) = (creates_partition_set, &plan_obj.migration_config) {
            crate::registrations::create_registrations_table(client).await?;
            let registration = crate::registrations::new_registration(
                schema.to_string(),
                table.to_string(),
                config.partition_strategy,
                config.partition_key.clone(),
                config.interval.clone(),
                config.premake_count,
                config.retention_policy.clone(),
                config.hash_modulus,
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

/// Errors when a plan's stated execution order disagrees with the order it
/// would actually run in.
///
/// `sequence == 0` means the field was absent from the plan file — every action
/// in a plan written before `sequence` existed reads that way — so those are
/// skipped rather than treated as a real position.
fn verify_action_order(actions: &[PlanAction]) -> Result<()> {
    for (index, action) in actions.iter().enumerate() {
        let expected = index + 1;
        if action.sequence != 0 && action.sequence != expected {
            return Err(anyhow!(
                "Plan is inconsistent: the action at position {} says it is step {} \
                 ({}: {}). The plan file has been reordered or hand-edited since it was \
                 written; re-run `plan` to regenerate it.",
                expected,
                action.sequence,
                action.action_type.to_string(),
                action.description
            ));
        }
    }

    Ok(())
}

fn split_qualified_name(qualified: &str) -> Result<(String, String)> {
    match qualified.splitn(2, '.').collect::<Vec<&str>>().as_slice() {
        [schema, table] if !schema.is_empty() && !table.is_empty() => {
            Ok((schema.to_string(), table.to_string()))
        }
        _ => Err(anyhow!(
            "Expected a schema-qualified name in the form 'schema.table', got: {}",
            qualified
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_qualified_name() {
        assert_eq!(
            split_qualified_name("public.events").unwrap(),
            ("public".to_string(), "events".to_string())
        );

        // A malformed `checksum_table` must fail loudly rather than silently
        // drift-checking the wrong relation.
        assert!(split_qualified_name("events").is_err());
        assert!(split_qualified_name(".events").is_err());
        assert!(split_qualified_name("public.").is_err());
        assert!(split_qualified_name("").is_err());
    }

    fn action(sequence: usize) -> PlanAction {
        PlanAction {
            id: format!("action-{}", sequence),
            action_type: ActionType::CreatePartition,
            table_name: "public.events".to_string(),
            description: "Create partitions".to_string(),
            estimated_duration_secs: Some(1),
            sequence,
        }
    }

    #[test]
    fn test_verify_action_order_accepts_a_correctly_numbered_plan() {
        assert!(verify_action_order(&[action(1), action(2), action(3)]).is_ok());
        assert!(verify_action_order(&[]).is_ok());
    }

    #[test]
    fn test_verify_action_order_rejects_a_reordered_plan() {
        // Swapping two actions in the file leaves the numbers describing an
        // order the orchestrator would not run.
        let error = verify_action_order(&[action(2), action(1)])
            .unwrap_err()
            .to_string();
        assert!(error.contains("position 1"), "{}", error);
        assert!(error.contains("step 2"), "{}", error);
    }

    #[test]
    fn test_verify_action_order_skips_plans_written_before_sequence_existed() {
        // `sequence` absent from the JSON deserializes as 0 for every action;
        // those plans must still apply.
        assert!(verify_action_order(&[action(0), action(0), action(0)]).is_ok());
    }

    #[test]
    fn test_applier_creation() {
        let applier = Applier::new(None);
        assert_eq!(
            applier.orchestrator.retry_policy.max_attempts,
            5
        );
    }
}
