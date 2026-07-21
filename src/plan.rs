use anyhow::Result;
use sha2::{Digest, Sha256};
use tokio_postgres::Client;
use uuid::Uuid;

use crate::types::{MigrationConfig, Plan, PlanAction, ActionType};
use crate::validation;

pub struct Planner;

impl Planner {
    pub async fn plan_migration(
        client: &Client,
        schema: &str,
        table: &str,
        config: &MigrationConfig,
    ) -> Result<Plan> {
        // Run validation first. `unique_index_missing_partition_key` is
        // downgraded to a warning rather than a hard block: the orchestrator's
        // CreateIndex step deliberately skips recreating unique/PK indexes
        // that don't include the partition key (Postgres wouldn't allow it on
        // a partitioned table regardless), logging a warning instead of
        // failing — so this is no longer a reason planning itself can't
        // proceed. Every other validation category still hard-blocks.
        let validation_errors = validation::validate_table_for_partitioning(client, schema, table, config).await?;

        let mut warnings: Vec<String> = Vec::new();
        let blocking_errors: Vec<_> = validation_errors
            .into_iter()
            .filter(|e| {
                if e.category == "unique_index_missing_partition_key" {
                    warnings.push(format!("{}: {}", e.category, e.message));
                    false
                } else {
                    true
                }
            })
            .collect();

        if !blocking_errors.is_empty() {
            let error_messages = blocking_errors
                .iter()
                .map(|e| format!("{}: {}", e.category, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(anyhow::anyhow!("Validation failed: {}", error_messages));
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
            warnings,
        })
    }
}

async fn compute_schema_checksum(client: &Client, schema: &str, table: &str) -> Result<String> {
    // Get table structure (OID, column definitions, constraints)
    let query = format!(
        r#"
        SELECT
            (SELECT array_agg(attname ORDER BY attnum) FROM pg_attribute
             WHERE attrelid = '{}.{}'::regclass AND attnum > 0),
            (SELECT array_agg(conname) FROM pg_constraint
             WHERE conrelid = '{}.{}'::regclass)
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
            warnings: vec![],
        };

        assert_eq!(plan.version, "1.0");
        assert_eq!(plan.database, "testdb");
    }
}
