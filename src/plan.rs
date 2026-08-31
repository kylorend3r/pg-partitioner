use anyhow::Result;
use sha2::{Digest, Sha256};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::migration::TemplateTargetState;
use crate::types::{ActionType, MigrationConfig, PartitionStrategy, Plan, PlanAction};
use crate::validation;

pub struct Planner;

impl Planner {
    /// Entry point for both planning flows. `config.template_table` is the
    /// switch: with it, a brand-new partitioned table is created from a
    /// separate structure source and nothing existing is touched; without it,
    /// this is the original ATTACH-first cutover of an existing table, entirely
    /// unchanged.
    pub async fn plan_migration(
        client: &Client,
        schema: &str,
        table: &str,
        config: &MigrationConfig,
    ) -> Result<Plan> {
        if config.template_table.is_some() {
            return Self::plan_template_creation(client, schema, table, config).await;
        }

        Self::plan_cutover(client, schema, table, config).await
    }

    async fn plan_cutover(
        client: &Client,
        schema: &str,
        table: &str,
        config: &MigrationConfig,
    ) -> Result<Plan> {
        // Enforced here rather than only in the CLI so the library path is
        // guarded too. Without this, a list cutover produced a perfectly
        // well-formed plan whose AttachPartition action carried a
        // `FOR VALUES FROM (MINVALUE) TO (…)` bound — meaningless to a LIST
        // parent — and it failed at *apply* time, after the bounding CHECK had
        // already been added to and validated on the real table.
        match config.partition_strategy {
            PartitionStrategy::Range => {}
            PartitionStrategy::Hash => {
                return Err(anyhow::anyhow!(
                    "Converting an existing table is supported for range only (got hash). A \
                     hash-partitioned table cannot have a DEFAULT partition, so there is no \
                     bucket that could hold the existing rows — create a new table with \
                     --template-table instead."
                ))
            }
            PartitionStrategy::List => {
                return Err(anyhow::anyhow!(
                    "Converting an existing table is supported for range only (got list). \
                     Converting to LIST would mean attaching the existing table as the DEFAULT \
                     partition and draining it before any FOR VALUES IN (…) partition could be \
                     created — use --template-table to create a new list-partitioned table, \
                     then `pg-partitioner add-partition`."
                ))
            }
        }

        // Run validation first. `unique_index_missing_partition_key` is
        // downgraded to a warning rather than a hard block: the orchestrator's
        // CreateIndex step deliberately skips recreating unique/PK indexes
        // that don't include the partition key (Postgres wouldn't allow it on
        // a partitioned table regardless), logging a warning instead of
        // failing — so this is no longer a reason planning itself can't
        // proceed. Every other validation category still hard-blocks.
        let validation_errors = validation::validate_table_for_partitioning(client, schema, table, config).await?;

        let (blocking_errors, mut warnings) = split_blocking_errors(validation_errors);

        if !blocking_errors.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation failed: {}",
                blocking_errors.join("; ")
            ));
        }

        // Decide the migration method and collect any advisory warnings (e.g.
        // update activity detected on the source table). The method itself is
        // intentionally unused below: `migration::plan_migration` never
        // constructs `MigrationMethod::BulkCopy` today, so this planner always
        // emits the ATTACH-first sequence and self-recomputes bounds/names at
        // execution time rather than trusting that function's placeholder
        // min/max constraint.
        let crate::migration::MigrationPlan { warnings: migration_warnings, .. } =
            crate::migration::plan_migration(client, schema, table, config).await?;
        warnings.extend(migration_warnings);

        // Transient count-only check: computed here purely to warn if the
        // start_date + interval combination would create a very large number
        // of partitions. Not stored anywhere — orchestrator.rs recomputes the
        // authoritative set fresh at apply time, same as every other bound.
        let start_date = config
            .start_date
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("migration_config missing start_date"))?;
        let boundary_count =
            crate::migration::compute_partition_boundaries(client, &config.interval, config.premake_count, start_date)
                .await?
                .len();
        if boundary_count > crate::risk::PLANNER_RISK_PARTITION_COUNT {
            warnings.push(format!(
                "large_partition_count: this start date + interval would create {} partitions \
                 (including the legacy bucket), above the {}-partition planner-risk threshold",
                boundary_count.saturating_sub(1),
                crate::risk::PLANNER_RISK_PARTITION_COUNT
            ));
        }

        let table_name = format!("{}.{}", schema, table);
        let actions = vec![
            PlanAction {
                id: Uuid::new_v4().to_string(),
                action_type: ActionType::AddConstraint,
                table_name: table_name.clone(),
                description: format!(
                    "Add NOT VALID bounding CHECK constraint on {}.{}",
                    schema, table
                ),
                estimated_duration_secs: Some(1),
            },
            PlanAction {
                id: Uuid::new_v4().to_string(),
                action_type: ActionType::ValidateConstraint,
                table_name: table_name.clone(),
                description: format!("Validate bounding CHECK constraint on {}.{}", schema, table),
                estimated_duration_secs: Some(5),
            },
            PlanAction {
                id: Uuid::new_v4().to_string(),
                action_type: ActionType::CreatePartitionSet,
                table_name: table_name.clone(),
                description: format!("Create partitioned shadow table for {}.{}", schema, table),
                estimated_duration_secs: Some(1),
            },
            PlanAction {
                id: Uuid::new_v4().to_string(),
                action_type: ActionType::AttachPartition,
                table_name: table_name.clone(),
                description: format!(
                    "Atomic rename-rename-attach cutover for {}.{}",
                    schema, table
                ),
                estimated_duration_secs: Some(1),
            },
            PlanAction {
                id: Uuid::new_v4().to_string(),
                action_type: ActionType::CreatePartition,
                table_name: table_name.clone(),
                description: format!(
                    "Create default partition + {} forward-looking partition(s) for {}.{}",
                    config.premake_count, schema, table
                ),
                estimated_duration_secs: Some(1),
            },
            PlanAction {
                id: Uuid::new_v4().to_string(),
                action_type: ActionType::CreateIndex,
                table_name: table_name.clone(),
                description: format!(
                    "Recreate non-unique indexes on the new parent for {}.{}",
                    schema, table
                ),
                estimated_duration_secs: Some(1),
            },
        ];

        // Compute schema checksum
        let schema_checksum = compute_schema_checksum(client, schema, table).await?;

        Ok(Plan {
            version: "1.0".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            database: get_database_name(client).await.unwrap_or_default(),
            schema_checksum,
            actions,
            migration_config: Some(config.clone()),
            checksum_table: None,
            warnings,
        })
    }

    /// Plans creation of a new partitioned table from a template.
    ///
    /// All three strategies, differing only in their middle step:
    ///
    /// - **List** creates no children at all — they arrive one at a time via
    ///   `add-partition`.
    /// - **Hash** creates its entire bucket set immediately, because a hash
    ///   table is only usable once every remainder is covered and it can have
    ///   no DEFAULT partition to catch what's missing.
    /// - **Range** creates the period window from `start_date` forward, and
    ///   deliberately *no* DEFAULT: the table starts empty, and a row landing
    ///   in DEFAULT would block creating that period's real partition, which
    ///   is what the next maintenance sweep would try to do.
    async fn plan_template_creation(
        client: &Client,
        schema: &str,
        table: &str,
        config: &MigrationConfig,
    ) -> Result<Plan> {
        let template = config
            .template_table
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("plan_template_creation called without a template"))?;
        let (template_schema, template_table) = split_qualified_name(template)?;

        if config.partition_strategy == PartitionStrategy::Hash && config.hash_modulus.is_none() {
            return Err(anyhow::anyhow!(
                "migration_config missing hash_modulus; re-run `plan` with --hash-partitions"
            ));
        }

        let validation_errors = validation::validate_template_creation(
            client,
            template_schema,
            template_table,
            schema,
            table,
            config,
        )
        .await?;

        let (blocking_errors, mut warnings) = split_blocking_errors(validation_errors);
        if !blocking_errors.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation failed: {}",
                blocking_errors.join("; ")
            ));
        }

        let table_name = format!("{}.{}", schema, table);

        // Range needs its period window resolved before the actions are built,
        // both to describe the plan honestly and to warn about a start date
        // that would produce an unreasonable number of partitions. Computed the
        // same way the cutover path computes it, and — like there — not stored:
        // the orchestrator recomputes the authoritative set at apply time.
        let (start_date, range_boundary_count) =
            if config.partition_strategy == PartitionStrategy::Range {
                let start_date = config.start_date.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "migration_config missing start_date; re-run `plan` with --start-date"
                    )
                })?;
                let count = crate::migration::compute_partition_boundaries(
                    client,
                    &config.interval,
                    config.premake_count,
                    &start_date,
                )
                .await?
                .len();

                if count > crate::risk::PLANNER_RISK_PARTITION_COUNT {
                    warnings.push(format!(
                        "large_partition_count: this start date + interval would create {} \
                         partitions, above the {}-partition planner-risk threshold",
                        count.saturating_sub(1),
                        crate::risk::PLANNER_RISK_PARTITION_COUNT
                    ));
                }

                (start_date, count)
            } else {
                (String::new(), 0)
            };

        let actions = match crate::migration::classify_template_target(
            client,
            schema,
            table,
            config.partition_strategy,
            &config.partition_key,
        )
        .await?
        {
            TemplateTargetState::Conflict(detail) => {
                return Err(anyhow::anyhow!(
                    "Cannot create {}: {}. Pick a different target name, or drop the \
                     existing object first.",
                    table_name,
                    detail
                ));
            }
            // An empty action list is the honest representation of "nothing to
            // do": the plan still applies cleanly (as a no-op), so a pipeline
            // that runs plan-then-apply unconditionally stays correct.
            TemplateTargetState::AlreadyMatches => {
                warnings.push(format!(
                    "already_present: {} is already partitioned BY {} ({}); nothing to create",
                    table_name,
                    config.partition_strategy.as_sql_keyword(),
                    config.partition_key.columns.join(", ")
                ));
                Vec::new()
            }
            TemplateTargetState::Absent => {
                let mut actions = vec![PlanAction {
                    id: Uuid::new_v4().to_string(),
                    action_type: ActionType::CreatePartitionSet,
                    table_name: table_name.clone(),
                    description: format!(
                        "Create {} as PARTITION BY {} ({}) from template {}",
                        table_name,
                        config.partition_strategy.as_sql_keyword(),
                        config.partition_key.columns.join(", "),
                        template
                    ),
                    estimated_duration_secs: Some(1),
                }];

                match config.partition_strategy {
                    PartitionStrategy::Hash => {
                        let modulus = config.hash_modulus.unwrap_or_default();
                        actions.push(PlanAction {
                            id: Uuid::new_v4().to_string(),
                            action_type: ActionType::CreatePartition,
                            table_name: table_name.clone(),
                            description: format!(
                                "Create {} hash bucket(s) for {} (MODULUS {})",
                                modulus, table_name, modulus
                            ),
                            estimated_duration_secs: Some(1),
                        });
                    }
                    PartitionStrategy::Range => {
                        actions.push(PlanAction {
                            id: Uuid::new_v4().to_string(),
                            action_type: ActionType::CreatePartition,
                            table_name: table_name.clone(),
                            description: format!(
                                "Create {} range partition(s) for {} from {}, every {} \
                                 (no DEFAULT partition)",
                                range_boundary_count.saturating_sub(1),
                                table_name,
                                start_date,
                                config.interval
                            ),
                            estimated_duration_secs: Some(1),
                        });
                    }
                    // Children arrive later, via `add-partition`.
                    PartitionStrategy::List => {}
                }

                actions.push(PlanAction {
                    id: Uuid::new_v4().to_string(),
                    action_type: ActionType::CreateIndex,
                    table_name: table_name.clone(),
                    description: format!(
                        "Recreate {}'s non-unique indexes on {}",
                        template, table_name
                    ),
                    estimated_duration_secs: Some(1),
                });

                actions
            }
        };

        if config.partition_strategy == PartitionStrategy::List {
            // List creates no children here by design: the parent lands empty
            // and `add-partition` fills it in one value set at a time. An index
            // created on the parent now still applies to every partition added
            // later, so there's nothing to defer on that front.
            warnings.push(format!(
                "no_partitions_created: {} will have no child partitions (not even a DEFAULT); \
                 add them with `pg-partitioner add-partition`",
                table_name
            ));
        }

        // The target doesn't exist yet, so the checksum covers the template —
        // the structure the target is about to be copied from.
        let schema_checksum =
            compute_schema_checksum(client, template_schema, template_table).await?;

        Ok(Plan {
            version: "1.0".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            database: get_database_name(client).await.unwrap_or_default(),
            schema_checksum,
            actions,
            migration_config: Some(config.clone()),
            checksum_table: Some(template.to_string()),
            warnings,
        })
    }

    /// Plans a single `FOR VALUES IN (…)` child for an already-list-partitioned
    /// table. Deliberately routed through the same Plan → Apply → orchestrator
    /// path as everything else rather than executed one-shot, so the DDL lands
    /// in `partitioner_logbook` like every other change this tool makes.
    pub async fn plan_add_list_partition(
        client: &Client,
        schema: &str,
        table: &str,
        partition_name: &str,
        values: &[String],
    ) -> Result<Plan> {
        let validation_errors =
            validation::validate_add_list_partition(client, schema, table, partition_name, values)
                .await?;

        let (blocking_errors, warnings) = split_blocking_errors(validation_errors);
        if !blocking_errors.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation failed: {}",
                blocking_errors.join("; ")
            ));
        }

        // Validation has already established the target is list-partitioned.
        let (_, partition_key) = crate::schema::get_partition_strategy_and_key(client, schema, table)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("{}.{} is no longer a partitioned table", schema, table)
            })?;

        let table_name = format!("{}.{}", schema, table);

        // `interval`/`premake_count` are structurally required by
        // `MigrationConfig` but carry no meaning for a list partition. They stay
        // at their inert defaults and are never persisted: `apply` only
        // registers a table when the plan creates a partition *set*, which this
        // plan does not.
        let config = MigrationConfig {
            source_table: table_name.clone(),
            partition_strategy: PartitionStrategy::List,
            partition_key,
            interval: "1 month".to_string(),
            premake_count: 0,
            use_bulk_copy: false,
            retention_policy: None,
            start_date: None,
            template_table: None,
            list_partition_name: Some(partition_name.to_string()),
            list_partition_values: Some(values.to_vec()),
            hash_modulus: None,
        };

        let actions = vec![PlanAction {
            id: Uuid::new_v4().to_string(),
            action_type: ActionType::CreatePartition,
            table_name: table_name.clone(),
            description: format!(
                "Create list partition {}.{} of {} FOR VALUES IN ({})",
                schema,
                partition_name,
                table_name,
                crate::migration::format_list_values(values)
            ),
            estimated_duration_secs: Some(1),
        }];

        let schema_checksum = compute_schema_checksum(client, schema, table).await?;

        Ok(Plan {
            version: "1.0".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            database: get_database_name(client).await.unwrap_or_default(),
            schema_checksum,
            actions,
            migration_config: Some(config),
            checksum_table: None,
            warnings,
        })
    }
}

/// Splits validation output into hard blockers and advisory warnings.
///
/// `unique_index_missing_partition_key` is the one category that doesn't block:
/// the `CreateIndex` step already skips unique indexes that omit the partition
/// key (PostgreSQL wouldn't accept them on a partitioned table), logging a
/// warning rather than failing, so there's nothing left for planning to refuse
/// over. Every other category still blocks.
fn split_blocking_errors(
    errors: Vec<crate::types::ValidationError>,
) -> (Vec<String>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut blocking = Vec::new();

    for error in errors {
        let rendered = format!("{}: {}", error.category, error.message);
        if error.category == "unique_index_missing_partition_key" {
            warnings.push(rendered);
        } else {
            blocking.push(rendered);
        }
    }

    (blocking, warnings)
}

fn split_qualified_name(qualified: &str) -> Result<(&str, &str)> {
    match qualified.splitn(2, '.').collect::<Vec<&str>>().as_slice() {
        [schema, table] if !schema.is_empty() && !table.is_empty() => Ok((schema, table)),
        _ => Err(anyhow::anyhow!(
            "Expected a schema-qualified name in the form 'schema.table', got: {}",
            qualified
        )),
    }
}

async fn compute_schema_checksum(client: &Client, schema: &str, table: &str) -> Result<String> {
    // Get table structure (OID, column definitions, constraints).
    //
    // Both aggregates are COALESCEd to an empty array because `array_agg` over
    // zero rows returns NULL, not `{}` — and a NULL column deserializes into
    // `Vec<String>` by panicking. A table carrying no `pg_constraint` rows at
    // all is entirely ordinary (no PK, no unique, no CHECK; NOT NULL lives in
    // `pg_attribute`, not here), and is exactly the shape a bare template
    // table tends to have.
    let query = format!(
        r#"
        SELECT
            COALESCE(
                (SELECT array_agg(attname ORDER BY attnum) FROM pg_attribute
                 WHERE attrelid = '{}.{}'::regclass AND attnum > 0),
                ARRAY[]::name[]
            ),
            COALESCE(
                (SELECT array_agg(conname) FROM pg_constraint
                 WHERE conrelid = '{}.{}'::regclass),
                ARRAY[]::name[]
            )
        "#,
        schema, table, schema, table
    );

    let rows = client.query(&query, &[]).await?;

    if let Some(row) = rows.first() {
        let columns: Vec<String> = row.get::<_, Vec<String>>(0);
        let constraints: Vec<String> = row.get::<_, Vec<String>>(1);

        let combined = format!("{:?}:{:?}", columns, constraints);

        let mut hasher = Sha256::new();
        hasher.update(combined.as_bytes());
        let hash = hasher.finalize();

        Ok(format!("{:x}", hash))
    } else {
        Err(anyhow::anyhow!("Table not found: {}.{}", schema, table))
    }
}

pub async fn verify_plan_drift(
    client: &Client,
    schema: &str,
    table: &str,
    expected_checksum: &str,
) -> Result<bool> {
    let current_checksum = compute_schema_checksum(client, schema, table).await?;
    Ok(current_checksum == expected_checksum)
}

async fn get_database_name(client: &Client) -> Result<String> {
    let row = client
        .query_one("SELECT current_database()", &[])
        .await?;
    Ok(row.get::<_, String>(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_creation() {
        let plan = Plan {
            version: "1.0".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            database: "testdb".to_string(),
            schema_checksum: "abc123".to_string(),
            actions: vec![],
            migration_config: None,
            checksum_table: None,
            warnings: vec![],
        };

        assert_eq!(plan.version, "1.0");
        assert_eq!(plan.database, "testdb");
    }

    #[test]
    fn test_split_qualified_name() {
        assert_eq!(split_qualified_name("public.events").unwrap(), ("public", "events"));
        assert!(split_qualified_name("events").is_err());
        assert!(split_qualified_name(".events").is_err());
        assert!(split_qualified_name("public.").is_err());
    }

    #[test]
    fn test_split_blocking_errors_downgrades_unique_index_only() {
        let errors = vec![
            crate::types::ValidationError {
                category: "unique_index_missing_partition_key".to_string(),
                message: "pk omits region".to_string(),
                suggestion: None,
            },
            crate::types::ValidationError {
                category: "list_key_must_be_single_column".to_string(),
                message: "two columns".to_string(),
                suggestion: None,
            },
        ];

        let (blocking, warnings) = split_blocking_errors(errors);
        assert_eq!(warnings.len(), 1);
        assert_eq!(blocking.len(), 1);
        assert!(blocking[0].starts_with("list_key_must_be_single_column"));
    }
}
