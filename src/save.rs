use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: String,
    pub timestamp: String,
    pub action: String,
    pub table_name: String,
    pub status: LogStatus,
    pub details: String,
    pub duration_ms: Option<i32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LogStatus {
    Started,
    Success,
    Failed,
    Retried,
}

impl LogStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogStatus::Started => "started",
            LogStatus::Success => "success",
            LogStatus::Failed => "failed",
            LogStatus::Retried => "retried",
        }
    }
}

pub async fn create_logbook_table(client: &Client) -> Result<()> {
    let query = r#"
        CREATE TABLE IF NOT EXISTS partitioner_logbook (
            id SERIAL PRIMARY KEY,
            timestamp TIMESTAMP NOT NULL,
            action VARCHAR(64) NOT NULL,
            table_name VARCHAR(128) NOT NULL,
            status VARCHAR(16) NOT NULL,
            details TEXT,
            duration_ms INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_partitioner_logbook_timestamp
            ON partitioner_logbook(timestamp DESC);
        CREATE INDEX IF NOT EXISTS idx_partitioner_logbook_table
            ON partitioner_logbook(table_name);
    "#;

    client.batch_execute(query).await?;
    Ok(())
}

pub async fn append_log(client: &Client, entry: &LogEntry) -> Result<()> {
    let query = r#"
        INSERT INTO partitioner_logbook (timestamp, action, table_name, status, details, duration_ms)
        VALUES ($1::timestamp, $2, $3, $4, $5, $6)
    "#;

    client
        .execute(
            query,
            &[
                &entry.timestamp,
                &entry.action,
                &entry.table_name,
                &entry.status.as_str(),
                &entry.details,
                &entry.duration_ms,
            ],
        )
        .await?;

    Ok(())
}

pub async fn get_recent_logs(
    client: &Client,
    table_name: Option<&str>,
    limit: i64,
) -> Result<Vec<LogEntry>> {
    let (query, rows) = if let Some(table) = table_name {
        let q = r#"
            SELECT id, timestamp, action, table_name, status, details, duration_ms
            FROM partitioner_logbook
            WHERE table_name = $1
            ORDER BY timestamp DESC
            LIMIT $2
        "#;
        (
            q,
            client
                .query(q, &[&table, &limit])
                .await?,
        )
    } else {
        let q = r#"
            SELECT id, timestamp, action, table_name, status, details, duration_ms
            FROM partitioner_logbook
            ORDER BY timestamp DESC
            LIMIT $1
        "#;
        let rows = client.query(q, &[&limit]).await?;
        (q, rows)
    };

    let mut entries = Vec::new();
    for row in rows {
        let status_str: String = row.get(4);
        let status = match status_str.as_str() {
            "started" => LogStatus::Started,
            "success" => LogStatus::Success,
            "failed" => LogStatus::Failed,
            "retried" => LogStatus::Retried,
            _ => LogStatus::Started,
        };

        entries.push(LogEntry {
            id: format!("{}", row.get::<_, i32>(0)),
            timestamp: row.get::<_, String>(1),
            action: row.get(2),
            table_name: row.get(3),
            status,
            details: row.get(5),
            duration_ms: row.get(6),
        });
    }

    Ok(entries)
}

pub fn create_log_entry(
    action: String,
    table_name: String,
    status: LogStatus,
    details: String,
) -> LogEntry {
    LogEntry {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        action,
        table_name,
        status,
        details,
        duration_ms: None,
    }
}
