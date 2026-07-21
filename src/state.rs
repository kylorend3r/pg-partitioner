use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub operation: String,
    pub table_name: String,
    pub progress: CheckpointProgress,
    pub created_at: String,
    pub last_updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CheckpointProgress {
    Started,
    Backfilling {
        batches_completed: i32,
        rows_moved: i64,
        high_water_mark: String,
    },
    ConversionComplete,
    Failed {
        reason: String,
    },
}

pub async fn create_checkpoint_table(client: &Client) -> Result<()> {
    let query = r#"
        CREATE SCHEMA IF NOT EXISTS partitioner;
        CREATE TABLE IF NOT EXISTS partitioner.partitioner_state (
            id VARCHAR(36) PRIMARY KEY,
            operation VARCHAR(64) NOT NULL,
            table_name VARCHAR(128) NOT NULL,
            progress JSONB NOT NULL,
            created_at TIMESTAMP NOT NULL,
            last_updated TIMESTAMP NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_partitioner_state_table
            ON partitioner.partitioner_state(table_name);
    "#;

    client.batch_execute(query).await?;
    Ok(())
}

pub async fn save_checkpoint(client: &Client, checkpoint: &Checkpoint) -> Result<()> {
    let progress_json = serde_json::to_string(&checkpoint.progress)?;

    let query = r#"
        INSERT INTO partitioner.partitioner_state (id, operation, table_name, progress, created_at, last_updated)
        VALUES ($1, $2, $3, $4::jsonb, $5::timestamp, $6::timestamp)
        ON CONFLICT (id) DO UPDATE SET
            progress = EXCLUDED.progress,
            last_updated = EXCLUDED.last_updated
    "#;

    client
        .execute(
            query,
            &[
                &checkpoint.id,
                &checkpoint.operation,
                &checkpoint.table_name,
                &progress_json,
                &checkpoint.created_at,
                &checkpoint.last_updated,
            ],
        )
        .await?;

    Ok(())
}

pub async fn get_checkpoint(client: &Client, id: &str) -> Result<Option<Checkpoint>> {
    let query = r#"
        SELECT id, operation, table_name, progress, created_at, last_updated
        FROM partitioner.partitioner_state
        WHERE id = $1
    "#;

    let rows = client.query(query, &[&id]).await?;

    if let Some(row) = rows.first() {
        let progress_json: String = row.get(3);
        let progress: CheckpointProgress = serde_json::from_str(&progress_json)?;

        Ok(Some(Checkpoint {
            id: row.get(0),
            operation: row.get(1),
            table_name: row.get(2),
            progress,
            created_at: row.get(4),
            last_updated: row.get(5),
        }))
    } else {
        Ok(None)
    }
}

pub async fn list_active_checkpoints(client: &Client, table_name: &str) -> Result<Vec<Checkpoint>> {
    let query = r#"
        SELECT id, operation, table_name, progress, created_at, last_updated
        FROM partitioner.partitioner_state
        WHERE table_name = $1 AND progress ->> 'Started' IS NOT NULL
        ORDER BY created_at DESC
    "#;

    let rows = client.query(query, &[&table_name]).await?;

    let mut checkpoints = Vec::new();
    for row in rows {
        let progress_json: String = row.get(3);
        let progress: CheckpointProgress = serde_json::from_str(&progress_json)?;

        checkpoints.push(Checkpoint {
            id: row.get(0),
            operation: row.get(1),
            table_name: row.get(2),
            progress,
            created_at: row.get(4),
            last_updated: row.get(5),
        });
    }

    Ok(checkpoints)
}

pub async fn delete_checkpoint(client: &Client, id: &str) -> Result<()> {
    client
        .execute("DELETE FROM partitioner.partitioner_state WHERE id = $1", &[&id])
        .await?;
    Ok(())
}

pub fn new_checkpoint(
    operation: String,
    table_name: String,
) -> Checkpoint {
    let now = Utc::now().to_rfc3339();
    Checkpoint {
        id: Uuid::new_v4().to_string(),
        operation,
        table_name,
        progress: CheckpointProgress::Started,
        created_at: now.clone(),
        last_updated: now,
    }
}
