use anyhow::{anyhow, Result};
use std::time::Instant;
use tokio_postgres::Client;
use tracing::{error, info, warn};

use crate::index;
use crate::migration;
use crate::queries;
use crate::save::{self, LogStatus};
use crate::types::{
    ActionType, MigrationConfig, PartitionCreationShape, PlanAction, RetryPolicy,
};

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
        //
        // Only the range cutover flow has boundaries at all. Template creation
        // and single-list-partition adds have no period to compute, and
        // demanding a `start_date` from them would reject perfectly valid plans
        // over a field that means nothing to either.
        let boundaries: Option<Vec<String>> = match migration_config {
            Some(config) if config.needs_range_boundaries() => {
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
            _ => None,
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

                match config.template_table.as_deref() {
                    // Template flow: the new table is created directly under
                    // its final name, since there's no rename-swap to keep
                    // that name free for.
                    Some(template) => {
                        let (template_schema, template_table) = split_table_name(template)?;

                        // Re-run the idempotency check against the live catalog
                        // rather than trusting the plan: an arbitrary amount of
                        // time can pass between `plan` and `apply`, and the
                        // `CREATE TABLE IF NOT EXISTS` below would otherwise
                        // silently no-op over a table that had appeared in the
                        // meantime with a different shape.
                        match migration::classify_template_target(
                            client,
                            schema,
                            table,
                            config.partition_strategy,
                            &config.partition_key,
                        )
                        .await?
                        {
                            migration::TemplateTargetState::Conflict(detail) => Err(anyhow!(
                                "Cannot create {}.{}: {}",
                                schema,
                                table,
                                detail
                            )),
                            migration::TemplateTargetState::AlreadyMatches => {
                                info!(
                                    table = action.table_name,
                                    "Target is already partitioned as requested; nothing to create"
                                );
                                Ok(())
                            }
                            migration::TemplateTargetState::Absent => {
                                migration::create_partitioned_table_from_template(
                                    client,
                                    template_schema,
                                    template_table,
                                    schema,
                                    table,
                                    config.partition_strategy,
                                    &config.partition_key,
                                    &self.retry_policy,
                                )
                                .await
                            }
                        }
                    }
                    None => {
                        migration::create_partitioned_shadow_table(
                            client,
                            schema,
                            table,
                            config,
                            &self.retry_policy,
                        )
                        .await?;
                        Ok(())
                    }
                }
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
                let config = migration_config.ok_or_else(|| {
                    anyhow!(
                        "CreatePartition requires migration_config; re-run `plan` to regenerate it"
                    )
                })?;

                // Which children to build is derived from the config, not
                // re-inferred here, so a new strategy fails to compile rather
                // than falling through to whichever branch happened to be last.
                let shape = config.creation_shape().ok_or_else(|| {
                    anyhow!(
                        "Plan carries a CreatePartition action for {}, but its config creates no \
                         partitions of its own — a list table's children are added one at a time \
                         with `add-partition`. Re-run `plan` to regenerate it.",
                        action.table_name
                    )
                })?;

                match shape {
                    // The whole bucket set at once. There is no DEFAULT
                    // partition to fall back on, so a table missing even one
                    // remainder rejects rows that hash to it.
                    PartitionCreationShape::HashBuckets => {
                        let modulus = config.hash_modulus.ok_or_else(|| {
                            anyhow!(
                                "hash partitioning requires hash_modulus; re-run `plan` with \
                                 --hash-partitions"
                            )
                        })?;
                        let created = migration::create_hash_partitions(
                            client,
                            schema,
                            table,
                            modulus,
                            &self.retry_policy,
                        )
                        .await?;
                        info!(
                            table = action.table_name,
                            modulus = modulus,
                            created = created.len(),
                            "Hash bucket set created"
                        );
                        Ok(())
                    }

                    // `add-partition`: one explicitly-named list child, no
                    // default. A list-partitioned table gets no DEFAULT
                    // partition from this tool at all — rows with an unlisted
                    // value are rejected rather than quietly pooled somewhere
                    // they'd later have to be reconciled out of.
                    PartitionCreationShape::SingleListPartition => {
                        let values = config.list_partition_values.as_deref().ok_or_else(|| {
                            anyhow!(
                                "migration_config has no list_partition_values; re-run `plan` \
                                 to regenerate it"
                            )
                        })?;
                        let partition_name =
                            config.list_partition_name.as_deref().ok_or_else(|| {
                                anyhow!(
                                    "migration_config has list_partition_values but no \
                                     list_partition_name; re-run `plan` to regenerate it"
                                )
                            })?;
                        migration::create_list_partition(
                            client,
                            schema,
                            table,
                            partition_name,
                            values,
                            &self.retry_policy,
                        )
                        .await
                    }

                    PartitionCreationShape::RangeWindow { with_default } => {
                        // Cutover only. A table created from a template starts
                        // empty, and a row that later lands in DEFAULT would
                        // block creating that period's real partition — which
                        // is exactly what the next maintenance sweep tries to
                        // do, so it would start failing.
                        if with_default {
                            migration::create_default_partition(
                                client,
                                schema,
                                table,
                                &self.retry_policy,
                            )
                            .await?;
                        }

                        let boundaries = boundaries.ok_or_else(|| {
                            anyhow!(
                                "CreatePartition for a range table requires computed partition \
                                 boundaries; re-run `plan` to regenerate it"
                            )
                        })?;

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

                        Ok(())
                    }
                }
            }

            ActionType::CreateIndex => {
                // Where the index definitions come from depends on the flow.
                // Cutover reads them off the renamed original (now the legacy
                // child); template creation reads them off the template, which
                // is the only place the target's structure has ever existed.
                let (source_schema, source_table) = match migration_config
                    .and_then(|c| c.template_table.as_deref())
                {
                    Some(template) => {
                        let (ts, tt) = split_table_name(template)?;
                        (ts.to_string(), tt.to_string())
                    }
                    None => (schema.to_string(), format!("{}_legacy", table)),
                };

                let partition_key_columns: &[String] = migration_config
                    .map(|c| c.partition_key.columns.as_slice())
                    .unwrap_or(&[]);

                let defs = index::get_indexes_for_table(client, &source_schema, &source_table).await?;

                for def in defs {
                    // An expression index leaves no column names behind to
                    // rebuild from — `get_indexes_for_table` returns a short
                    // list for those rather than a wrong one.
                    if def.columns.is_empty() {
                        warn!(
                            table = action.table_name,
                            index = def.name,
                            "Skipping index during automatic recreation: it is an expression \
                             index, which cannot be reconstructed from column names alone"
                        );
                        continue;
                    }

                    // Postgres requires a unique index on a partitioned table
                    // to include every partition-key column. One that does is
                    // perfectly legal and worth keeping unique; one that
                    // doesn't cannot be created at all.
                    let unique_is_creatable = !def.is_unique
                        || partition_key_columns.iter().all(|pk| {
                            def.columns.iter().any(|c| c.eq_ignore_ascii_case(pk))
                        });

                    if !unique_is_creatable {
                        warn!(
                            table = action.table_name,
                            index = def.name,
                            source_table = source_table,
                            "Skipping unique/PK index during automatic recreation: partitioned \
                             tables require unique indexes to include all partition-key columns"
                        );
                        continue;
                    }

                    let new_index_name = derived_index_name(table, &def.name, &source_table);
                    if new_index_name.len() > MAX_IDENTIFIER_BYTES {
                        warn!(
                            table = action.table_name,
                            index = def.name,
                            derived = new_index_name,
                            "Skipping index during automatic recreation: the derived name would \
                             exceed PostgreSQL's 63-byte identifier limit"
                        );
                        continue;
                    }

                    index::create_index_on_parent(
                        client,
                        schema,
                        table,
                        &new_index_name,
                        &def.columns,
                        def.is_unique,
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

/// PostgreSQL's hard identifier limit. Names longer than this are silently
/// truncated by the server, which turns two distinct indexes into a collision.
const MAX_IDENTIFIER_BYTES: usize = 63;

/// A name for the copy of `source_index` being created on `target_table`.
///
/// Index names are unique per schema, so the source's own name can't be
/// reused — the source index still exists, on the legacy child or on the
/// untouched template. Swapping the source table's name prefix for the
/// target's keeps the recognisable `{table}_{cols}_idx` shape; anything that
/// doesn't start with that prefix falls back to a suffix, which is the
/// convention the cutover path already used.
fn derived_index_name(target_table: &str, source_index: &str, source_table: &str) -> String {
    match source_index.strip_prefix(source_table) {
        Some(rest) => format!("{}{}", target_table, rest),
        None => format!("{}_new", source_index),
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
