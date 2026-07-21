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
        // Run validation first
        let validation_errors = validation::validate_table_for_partitioning(client, schema, table, config).await?;

        if !validation_errors.is_empty() {
            let error_messages = validation_errors
                .iter()
                .map(|e| format!("{}: {}", e.category, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(anyhow::anyhow!("Validation failed: {}", error_messages));
        }

        // Compute the plan
        let actions = vec![
            PlanAction {
                id: Uuid::new_v4().to_string(),
                action_type: ActionType::CreatePartitionSet,
                table_name: format!("{}.{}", schema, table),
                description: format!("Create partitioned version of table {}.{}", schema, table),
                estimated_duration_secs: Some(5),
            },
            PlanAction {
                id: Uuid::new_v4().to_string(),
                action_type: ActionType::MigrateToPartition,
                table_name: format!("{}.{}", schema, table),
                description: format!("Migrate data to partitioned structure"),
                estimated_duration_secs: Some(30),
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
        };

        assert_eq!(plan.version, "1.0");
        assert_eq!(plan.database, "testdb");
    }
}
