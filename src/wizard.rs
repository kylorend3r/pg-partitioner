use anyhow::Result;
use tokio_postgres::Client;

use crate::types::{MigrationConfig, PartitionKey, PartitionStrategy};

pub struct SetupWizard;

impl SetupWizard {
    /// Not yet interactive: returns a fixed monthly-range config rather than
    /// asking anything, so `_client` has nothing to inspect. A real wizard would
    /// use the connection to suggest a key and interval from the table's own
    /// columns and growth rate.
    pub async fn run_interactive_setup(
        _client: &Client,
        schema: &str,
        table: &str,
    ) -> Result<MigrationConfig> {
        let partition_key = PartitionKey::single("created_at".to_string());

        let config = MigrationConfig {
            source_table: format!("{}.{}", schema, table),
            partition_strategy: PartitionStrategy::Range,
            partition_key,
            interval: "1 month".to_string(),
            premake_count: 3,
            use_bulk_copy: false,
            retention_policy: None,
            start_date: None,
            template_table: None,
            list_partition_name: None,
            list_partition_values: None,
            hash_modulus: None,
        };

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migration_config_defaults() {
        let config = MigrationConfig {
            source_table: "public.events".to_string(),
            partition_strategy: PartitionStrategy::Range,
            partition_key: PartitionKey::single("created_at".to_string()),
            interval: "1 month".to_string(),
            premake_count: 3,
            use_bulk_copy: false,
            retention_policy: None,
            start_date: None,
            template_table: None,
            list_partition_name: None,
            list_partition_values: None,
            hash_modulus: None,
        };

        assert_eq!(config.interval, "1 month");
        assert_eq!(config.premake_count, 3);
        assert!(!config.use_bulk_copy);
        // A plain range cutover config is the one shape that drives the
        // period-boundary machinery.
        assert!(config.needs_range_boundaries());
    }
}
