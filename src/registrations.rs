use anyhow::Result;
use chrono::Utc;
use tokio_postgres::Client;
use uuid::Uuid;

use crate::schema;
use crate::types::{PartitionKey, PartitionRegistration, PartitionStrategy, RetentionPolicy};

pub async fn create_registrations_table(client: &Client) -> Result<()> {
    let query = r#"
        CREATE SCHEMA IF NOT EXISTS partitioner;
        CREATE TABLE IF NOT EXISTS partitioner.partitioner_registrations (
            id VARCHAR(36) NOT NULL,
            schema_name VARCHAR(128) NOT NULL,
            table_name VARCHAR(128) NOT NULL,
            strategy VARCHAR(16) NOT NULL,
            partition_key TEXT NOT NULL,
            interval VARCHAR(64) NOT NULL,
            premake_count INTEGER NOT NULL DEFAULT 0,
            -- Stored as text (serialized JSON), not JSONB: tokio-postgres is
            -- built here without the with-serde_json-1 feature, so String's
            -- ToSql impl doesn't accept the jsonb OID; we never query into
            -- this column with SQL JSON operators, so plain text round-trips
            -- fine through serde_json on the Rust side.
            retention_policy TEXT,
            -- Stored as text (RFC3339), not TIMESTAMP: tokio-postgres is built
            -- here without chrono support, so a TIMESTAMP column can't be read
            -- back into a Rust String; registered_at is never used in SQL date
            -- arithmetic, so plain text avoids that mismatch entirely.
            registered_at VARCHAR(64) NOT NULL,
            PRIMARY KEY (schema_name, table_name)
        );
    "#;

    client.batch_execute(query).await?;
    Ok(())
}

pub async fn upsert_registration(client: &Client, registration: &PartitionRegistration) -> Result<()> {
    let partition_key = registration.partition_key.columns.join(",");
    let strategy = registration.strategy.as_registration_str();
    let retention_policy_json = registration
        .retention_policy
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    let query = r#"
        INSERT INTO partitioner.partitioner_registrations
            (id, schema_name, table_name, strategy, partition_key, interval, premake_count, retention_policy, registered_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (schema_name, table_name) DO UPDATE SET
            strategy = EXCLUDED.strategy,
            partition_key = EXCLUDED.partition_key,
            interval = EXCLUDED.interval,
            premake_count = EXCLUDED.premake_count,
            retention_policy = EXCLUDED.retention_policy,
            registered_at = EXCLUDED.registered_at
    "#;

    client
        .execute(
            query,
            &[
                &registration.id,
                &registration.schema_name,
                &registration.table_name,
                &strategy,
                &partition_key,
                &registration.interval,
                &(registration.premake_count as i32),
                &retention_policy_json,
                &registration.registered_at,
            ],
        )
        .await?;

    Ok(())
}

pub async fn get_registration(
    client: &Client,
    schema: &str,
    table: &str,
) -> Result<Option<PartitionRegistration>> {
    if !schema::table_exists(client, "partitioner", "partitioner_registrations").await? {
        return Ok(None);
    }

    let query = r#"
        SELECT id, schema_name, table_name, strategy, partition_key, interval, premake_count, retention_policy, registered_at
        FROM partitioner.partitioner_registrations
        WHERE schema_name = $1 AND table_name = $2
    "#;

    let rows = client.query(query, &[&schema, &table]).await?;
    match rows.first() {
        Some(row) => Ok(Some(row_to_registration(row)?)),
        None => Ok(None),
    }
}

pub async fn list_registrations(client: &Client) -> Result<Vec<PartitionRegistration>> {
    if !schema::table_exists(client, "partitioner", "partitioner_registrations").await? {
        return Ok(Vec::new());
    }

    let query = r#"
        SELECT id, schema_name, table_name, strategy, partition_key, interval, premake_count, retention_policy, registered_at
        FROM partitioner.partitioner_registrations
        ORDER BY schema_name, table_name
    "#;

    let rows = client.query(query, &[]).await?;
    rows.iter().map(row_to_registration).collect()
}

pub async fn delete_registration(client: &Client, schema: &str, table: &str) -> Result<()> {
    client
        .execute(
            "DELETE FROM partitioner.partitioner_registrations WHERE schema_name = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await?;
    Ok(())
}

fn row_to_registration(row: &tokio_postgres::Row) -> Result<PartitionRegistration> {
    let strategy_str: String = row.get(3);
    let partition_key_str: String = row.get(4);
    let premake_count: i32 = row.get(6);
    let retention_policy_json: Option<String> = row.get(7);

    let retention_policy = retention_policy_json
        .map(|json| serde_json::from_str::<RetentionPolicy>(&json))
        .transpose()?;

    Ok(PartitionRegistration {
        id: row.get(0),
        schema_name: row.get(1),
        table_name: row.get(2),
        strategy: PartitionStrategy::from_registration_str(&strategy_str)?,
        partition_key: PartitionKey::new(
            partition_key_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect(),
        ),
        interval: row.get(5),
        premake_count: premake_count as usize,
        retention_policy,
        registered_at: row.get(8),
    })
}

pub fn new_registration(
    schema_name: String,
    table_name: String,
    strategy: PartitionStrategy,
    partition_key: PartitionKey,
    interval: String,
    premake_count: usize,
    retention_policy: Option<RetentionPolicy>,
) -> PartitionRegistration {
    PartitionRegistration {
        id: Uuid::new_v4().to_string(),
        schema_name,
        table_name,
        strategy,
        partition_key,
        interval,
        premake_count,
        retention_policy,
        registered_at: Utc::now().to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_registration_str_round_trip() {
        for strategy in [
            PartitionStrategy::Range,
            PartitionStrategy::List,
            PartitionStrategy::Hash,
        ] {
            let s = strategy.as_registration_str();
            let parsed = PartitionStrategy::from_registration_str(s).unwrap();
            assert_eq!(strategy, parsed);
        }
    }

    #[test]
    fn test_from_registration_str_rejects_unknown() {
        assert!(PartitionStrategy::from_registration_str("bogus").is_err());
    }

    #[test]
    fn test_new_registration_defaults() {
        let registration = new_registration(
            "public".to_string(),
            "events".to_string(),
            PartitionStrategy::Range,
            PartitionKey::single("created_at".to_string()),
            "1 month".to_string(),
            3,
            None,
        );

        assert!(Uuid::parse_str(&registration.id).is_ok());
        assert!(chrono::DateTime::parse_from_rfc3339(&registration.registered_at).is_ok());
        assert_eq!(registration.schema_name, "public");
        assert_eq!(registration.premake_count, 3);
    }
}
