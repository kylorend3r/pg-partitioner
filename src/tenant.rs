use anyhow::Result;
use tokio_postgres::Client;

use crate::queries;
use crate::types::PartitionKey;

pub struct TenantPartitioningConfig {
    pub table_name: String,
    pub tenant_column: String,
    pub partition_key: PartitionKey,
    pub archive_on_churn: bool,
}

pub async fn create_tenant_partition(
    client: &Client,
    table_name: &str,
    tenant_column: &str,
    tenant_id: &str,
) -> Result<String> {
    let partition_name = format!("{}_{}", table_name, tenant_id);

    let query = format!(
        "CREATE TABLE {}.{} PARTITION OF {} FOR VALUES IN ({})",
        queries::quote_ident("public"),
        queries::quote_ident(&partition_name),
        queries::quote_ident(table_name),
        queries::quote_literal(tenant_id)
    );

    client.execute(&query, &[]).await?;

    Ok(partition_name)
}

pub async fn archive_tenant_partition(
    client: &Client,
    table_name: &str,
    tenant_id: &str,
    archive_schema: &str,
) -> Result<()> {
    let partition_name = format!("{}_{}", table_name, tenant_id);

    // Detach partition concurrently (PG14+)
    let detach_query = format!(
        "ALTER TABLE {} DETACH PARTITION {} CONCURRENTLY",
        queries::quote_ident(table_name),
        queries::quote_ident(&partition_name)
    );

    client.execute(&detach_query, &[]).await?;

    // Move to archive schema
    let move_query = format!(
        "ALTER TABLE {} SET SCHEMA {}",
        queries::quote_ident(&partition_name),
        queries::quote_ident(archive_schema)
    );

    client.execute(&move_query, &[]).await?;

    Ok(())
}

pub async fn drop_tenant_partition(
    client: &Client,
    table_name: &str,
    tenant_id: &str,
) -> Result<()> {
    let partition_name = format!("{}_{}", table_name, tenant_id);

    // Detach first
    let detach_query = format!(
        "ALTER TABLE {} DETACH PARTITION {} CONCURRENTLY",
        queries::quote_ident(table_name),
        queries::quote_ident(&partition_name)
    );

    client.execute(&detach_query, &[]).await?;

    // Then drop
    let drop_query = format!("DROP TABLE {}", queries::quote_ident(&partition_name));

    client.execute(&drop_query, &[]).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tenant_config() {
        let config = TenantPartitioningConfig {
            table_name: "tenants_data".to_string(),
            tenant_column: "tenant_id".to_string(),
            partition_key: PartitionKey::single("tenant_id".to_string()),
            archive_on_churn: true,
        };

        assert_eq!(config.table_name, "tenants_data");
        assert_eq!(config.tenant_column, "tenant_id");
    }
}
