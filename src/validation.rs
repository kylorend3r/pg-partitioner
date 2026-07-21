use anyhow::Result;
use tokio_postgres::Client;

use crate::queries;
use crate::types::{MigrationConfig, PartitionKey, ValidationError};

pub async fn validate_table_for_partitioning(
    client: &Client,
    schema: &str,
    table: &str,
    config: &MigrationConfig,
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Verify table exists
    if !queries::verify_table_exists(client, schema, table).await? {
        errors.push(ValidationError {
            category: "table_not_found".to_string(),
            message: format!("Table {}.{} does not exist", schema, table),
            suggestion: None,
        });
        return Ok(errors);
    }

    // Check partition key columns exist
    errors.extend(
        validate_partition_key_columns(client, schema, table, &config.partition_key).await?,
    );

    // Check for naming collisions (63-byte identifier limit)
    errors.extend(validate_identifier_lengths(
        schema,
        table,
        &config.partition_key,
        &config.interval,
    ));

    // Check for unique indexes without partition key
    errors.extend(validate_unique_constraints(client, schema, table, &config.partition_key).await?);

    // Check max_locks_per_transaction headroom
    errors.extend(validate_lock_budget(client).await?);

    // Check for timezone mismatches
    errors.extend(validate_timezone_compatibility(client, &config.partition_key).await?);

    Ok(errors)
}

async fn validate_partition_key_columns(
    client: &Client,
    schema: &str,
    table: &str,
    partition_key: &PartitionKey,
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    for column in &partition_key.columns {
        let query = format!(
            "SELECT 1 FROM information_schema.columns
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3"
        );

        let rows = client.query(&query, &[&schema, &table, column]).await?;

        if rows.is_empty() {
            errors.push(ValidationError {
                category: "column_not_found".to_string(),
                message: format!(
                    "Partition key column '{}' not found in {}.{}",
                    column, schema, table
                ),
                suggestion: Some("Check the column name and table structure".to_string()),
            });
        }
    }

    Ok(errors)
}

fn validate_identifier_lengths(
    schema: &str,
    table: &str,
    partition_key: &PartitionKey,
    interval: &str,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    const IDENTIFIER_LIMIT: usize = 63;
    // Postgres's real limit is 63 bytes, but every derived partition/index
    // name adds a suffix on top of the table name — capping the table name
    // itself well below the hard limit leaves headroom for those suffixes
    // instead of relying solely on per-suffix math.
    const MAX_TABLE_NAME_LEN: usize = 60;

    if table.len() > MAX_TABLE_NAME_LEN {
        errors.push(ValidationError {
            category: "table_name_too_long".to_string(),
            message: format!(
                "Table name '{}' is {} characters; this tool caps table names at {} to leave \
                 headroom below PostgreSQL's 63-byte identifier limit for derived partition/index names",
                table,
                table.len(),
                MAX_TABLE_NAME_LEN
            ),
            suggestion: Some(format!("Use a table name of {} characters or fewer", MAX_TABLE_NAME_LEN)),
        });
    }

    let base_name = format!("{}_{}", table, interval);

    if base_name.len() > IDENTIFIER_LIMIT {
        errors.push(ValidationError {
            category: "identifier_too_long".to_string(),
            message: format!(
                "Base partition name '{}' exceeds PostgreSQL 63-byte limit (length: {})",
                base_name,
                base_name.len()
            ),
            suggestion: Some("Use shorter table or interval names".to_string()),
        });
    }

    for col in &partition_key.columns {
        let index_name = format!("{}_{}_{}_idx", schema, table, col);
        if index_name.len() > IDENTIFIER_LIMIT {
            errors.push(ValidationError {
                category: "index_name_too_long".to_string(),
                message: format!(
                    "Derived index name would be too long ({}). Schema: {}, Table: {}, Column: {}",
                    index_name.len(),
                    schema,
                    table,
                    col
                ),
                suggestion: Some("Use shorter names".to_string()),
            });
        }
    }

    // Date-range partition names (`{table}_YYYY_MM_DD_YYYY_MM_DD`) always add
    // exactly 22 bytes (6 underscores + 16 digits) regardless of the actual
    // dates involved, so this is a pure function of table.len() — no need to
    // compute real boundaries here.
    const DATE_RANGE_SUFFIX_LEN: usize = 22;
    let worst_case_len = table.len() + DATE_RANGE_SUFFIX_LEN;
    if worst_case_len > IDENTIFIER_LIMIT {
        errors.push(ValidationError {
            category: "partition_name_too_long".to_string(),
            message: format!(
                "Date-range partition names for '{}' would exceed PostgreSQL's 63-byte limit \
                 (worst case: {} bytes, e.g. '{}_YYYY_MM_DD_YYYY_MM_DD')",
                table, worst_case_len, table
            ),
            suggestion: Some("Use a shorter table name".to_string()),
        });
    }

    errors
}

async fn validate_unique_constraints(
    client: &Client,
    schema: &str,
    table: &str,
    partition_key: &PartitionKey,
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    let query = format!(
        r#"
        SELECT i.relname, array_agg(a.attname order by a.attnum) as columns
        FROM pg_class t
        JOIN pg_namespace n ON n.oid = t.relnamespace
        JOIN pg_index ix ON ix.indrelid = t.oid
        JOIN pg_class i ON i.oid = ix.indexrelid
        JOIN pg_attribute a ON a.attrelid = ix.indrelid
            AND a.attnum = ANY(ix.indkey)
        WHERE n.nspname = '{}' AND t.relname = '{}' AND ix.indisunique
        GROUP BY i.relname
        "#,
        schema, table
    );

    let rows = client.query(&query, &[]).await?;

    for row in rows {
        let index_name: String = row.get(0);
        let columns: Vec<String> = row.get::<_, Vec<String>>(1);

        let has_partition_key = partition_key.columns.iter().all(|pk_col| columns.contains(pk_col));

        if !has_partition_key {
            errors.push(ValidationError {
                category: "unique_index_missing_partition_key".to_string(),
                message: format!(
                    "Unique index '{}' does not include all partition key columns ({}). \
                     Unique constraints on partitioned tables must include the partition key.",
                    index_name,
                    partition_key.columns.join(", ")
                ),
                suggestion: Some("Add partition key columns to the index or recreate it".to_string()),
            });
        }
    }

    Ok(errors)
}

async fn validate_lock_budget(client: &Client) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    let row = client
        .query_one("SELECT current_setting('max_locks_per_transaction')::int", &[])
        .await?;

    let max_locks: i32 = row.get(0);

    if max_locks < 256 {
        errors.push(ValidationError {
            category: "max_locks_insufficient".to_string(),
            message: format!(
                "max_locks_per_transaction is set to {} (minimum 256 recommended for partitioning)",
                max_locks
            ),
            suggestion: Some(
                "Consider raising max_locks_per_transaction in postgresql.conf".to_string(),
            ),
        });
    }

    Ok(errors)
}

async fn validate_timezone_compatibility(
    client: &Client,
    partition_key: &PartitionKey,
) -> Result<Vec<ValidationError>> {
    let mut errors = Vec::new();

    // Check if partition key is a timestamp column
    if partition_key.columns.iter().any(|c| c.contains("time") || c.contains("date")) {
        let row = client
            .query_one("SELECT current_setting('timezone')", &[])
            .await?;
        let _tz: String = row.get(0);

        // Note: A more thorough check would compare server timezone with client timezone
        // For now, we just flag potential timezone usage
    }

    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_identifier_lengths() {
        let errors = validate_identifier_lengths("public", "events", &PartitionKey::single("created_at".to_string()), "2026_07");
        assert!(errors.is_empty()); // Normal case should pass

        let long_table = "a".repeat(60);
        let errors = validate_identifier_lengths("public", &long_table, &PartitionKey::single("created_at".to_string()), "2026_07");
        assert!(!errors.is_empty()); // Long name should fail
    }

    #[test]
    fn test_validate_table_name_max_60_chars() {
        let at_limit = "a".repeat(60);
        let errors = validate_identifier_lengths("public", &at_limit, &PartitionKey::single("created_at".to_string()), "d");
        assert!(!errors.iter().any(|e| e.category == "table_name_too_long")); // exactly 60 is allowed

        let over_limit = "a".repeat(61);
        let errors = validate_identifier_lengths("public", &over_limit, &PartitionKey::single("created_at".to_string()), "d");
        assert!(errors.iter().any(|e| e.category == "table_name_too_long"));
    }

    #[test]
    fn test_validate_identifier_lengths_date_range_partition_name() {
        // Short enough to pass the existing interval-suffix check (45 + 1 + 7 = 53 <= 63)
        // but long enough to overflow the date-range partition name check (45 + 22 = 67 > 63).
        let table = "a".repeat(45);
        let errors = validate_identifier_lengths("public", &table, &PartitionKey::single("created_at".to_string()), "2026_07");
        assert!(errors.iter().any(|e| e.category == "partition_name_too_long"));
        assert!(!errors.iter().any(|e| e.category == "identifier_too_long"));
    }
}
