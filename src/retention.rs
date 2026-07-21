use anyhow::Result;
use tokio_postgres::Client;

use crate::queries;
use crate::types::{RetentionPolicy, RetentionType};

pub async fn evaluate_retention_policy(
    client: &Client,
    table_name: &str,
    partition_column: &str,
    policy: &RetentionPolicy,
) -> Result<Vec<String>> {
    let partition_names = match policy.policy_type {
        RetentionType::Days => {
            get_partitions_older_than_days(client, table_name, partition_column, policy.value).await?
        }
        RetentionType::Months => {
            get_partitions_older_than_months(client, table_name, partition_column, policy.value)
                .await?
        }
        RetentionType::Years => {
            get_partitions_older_than_years(client, table_name, partition_column, policy.value)
                .await?
        }
        RetentionType::Count => {
            get_partitions_beyond_count(client, table_name, policy.value).await?
        }
    };

    Ok(partition_names)
}

async fn get_partitions_older_than_days(
    client: &Client,
    table_name: &str,
    partition_column: &str,
    days: i32,
) -> Result<Vec<String>> {
    let query = format!(
        r#"
        SELECT c.relname
        FROM pg_inherits i
        JOIN pg_class p ON p.oid = i.inhparent
        JOIN pg_class c ON c.oid = i.inhrelid
        WHERE p.oid = {}::regclass
            AND c.relkind = 'r'
            AND pg_get_partition_constraintdef(c.oid) LIKE '%<%'
            AND c.relname NOT LIKE '%default%'
        ORDER BY c.relname ASC
        LIMIT (
            SELECT COUNT(*) - 1
            FROM pg_inherits
            WHERE inhparent = {}::regclass
        )
        "#,
        queries::quote_ident(table_name),
        queries::quote_ident(table_name)
    );

    let rows = client.query(&query, &[]).await?;

    let mut partition_names = Vec::new();
    for row in rows {
        partition_names.push(row.get::<_, String>(0));
    }

    Ok(partition_names)
}

async fn get_partitions_older_than_months(
    _client: &Client,
    _table_name: &str,
    _partition_column: &str,
    _months: i32,
) -> Result<Vec<String>> {
    // Simplified for Phase 1; full implementation in Phase 2
    Ok(Vec::new())
}

async fn get_partitions_older_than_years(
    _client: &Client,
    _table_name: &str,
    _partition_column: &str,
    _years: i32,
) -> Result<Vec<String>> {
    // Simplified for Phase 1; full implementation in Phase 2
    Ok(Vec::new())
}

async fn get_partitions_beyond_count(
    client: &Client,
    table_name: &str,
    keep_count: i32,
) -> Result<Vec<String>> {
    let query = format!(
        r#"
        SELECT c.relname
        FROM pg_inherits i
        JOIN pg_class p ON p.oid = i.inhparent
        JOIN pg_class c ON c.oid = i.inhrelid
        WHERE p.oid = {}::regclass
            AND c.relkind = 'r'
            AND c.relname NOT LIKE '%default%'
        ORDER BY c.relname ASC
        LIMIT (
            SELECT COUNT(*) - {}
            FROM pg_inherits
            WHERE inhparent = {}::regclass
                AND inhrelid NOT IN (
                    SELECT oid FROM pg_class WHERE relname LIKE '%default%'
                )
        )
        "#,
        queries::quote_ident(table_name),
        keep_count,
        queries::quote_ident(table_name)
    );

    let rows = client.query(&query, &[]).await?;

    let mut partition_names = Vec::new();
    for row in rows {
        partition_names.push(row.get::<_, String>(0));
    }

    Ok(partition_names)
}

pub async fn drop_partition(
    client: &Client,
    schema: &str,
    partition_name: &str,
) -> Result<()> {
    let query = format!(
        "DROP TABLE {}.{}",
        queries::quote_ident(schema),
        queries::quote_ident(partition_name)
    );

    client.execute(&query, &[]).await?;

    Ok(())
}

pub async fn detach_partition_concurrently(
    client: &Client,
    schema: &str,
    parent_table: &str,
    partition_name: &str,
) -> Result<()> {
    let query = format!(
        "ALTER TABLE {}.{} DETACH PARTITION {}.{} CONCURRENTLY",
        queries::quote_ident(schema),
        queries::quote_ident(parent_table),
        queries::quote_ident(schema),
        queries::quote_ident(partition_name)
    );

    client.execute(&query, &[]).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retention_policy_default() {
        let policy = RetentionPolicy {
            policy_type: RetentionType::Days,
            value: 30,
        };

        assert_eq!(policy.value, 30);
    }
}
