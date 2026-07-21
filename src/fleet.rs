use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio_postgres::Client;

use crate::maintain::{MaintenanceSummary, Maintainer};
use crate::types::PartitionRegistration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FleetStatus {
    pub database: String,
    pub timestamp: String,
    pub total_tables: usize,
    pub healthy_tables: usize,
    pub tables_needing_attention: usize,
    pub maintenance_summary: Option<MaintenanceSummary>,
    pub table_statuses: Vec<TableFleetStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableFleetStatus {
    pub table_name: String,
    pub status: TableHealthStatus,
    pub rows: i64,
    pub size_mb: i64,
    pub partitions: usize,
    pub last_maintenance: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TableHealthStatus {
    Healthy,
    Warning,
    Critical,
}

pub struct FleetManager;

impl FleetManager {
    pub async fn get_fleet_status(
        client: &Client,
        database_name: &str,
    ) -> Result<FleetStatus> {
        let registrations = Maintainer::load_registrations(client).await?;

        let mut table_statuses = Vec::new();
        let mut healthy_count = 0;
        let mut warning_count = 0;

        for reg in &registrations {
            let status = Self::get_table_status(client, &reg).await?;

            match status.status {
                TableHealthStatus::Healthy => healthy_count += 1,
                TableHealthStatus::Warning => warning_count += 1,
                TableHealthStatus::Critical => {}
            }

            table_statuses.push(status);
        }

        Ok(FleetStatus {
            database: database_name.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            total_tables: registrations.len(),
            healthy_tables: healthy_count,
            tables_needing_attention: warning_count,
            maintenance_summary: None,
            table_statuses,
        })
    }

    async fn get_table_status(
        client: &Client,
        registration: &PartitionRegistration,
    ) -> Result<TableFleetStatus> {
        // Determine health status based on registration
        let status = if registration.premake_count < 1 {
            TableHealthStatus::Warning
        } else {
            TableHealthStatus::Healthy
        };

        Ok(TableFleetStatus {
            table_name: format!("{}.{}", registration.schema_name, registration.table_name),
            status,
            rows: 0, // Placeholder
            size_mb: 0, // Placeholder
            partitions: 0, // Placeholder
            last_maintenance: None,
        })
    }

    pub async fn run_fleet_maintenance(
        client: &Client,
    ) -> Result<FleetMaintenanceReport> {
        let start_time = std::time::Instant::now();
        let registrations = Maintainer::load_registrations(client).await?;
        let registration_count = registrations.len();

        let mut successful = 0;
        let mut failed = 0;
        let mut failures = Vec::new();

        for reg in registrations {
            let table_name = format!("{}.{}", reg.schema_name, reg.table_name);

            // Premake future partitions
            match Maintainer::premake_future_partitions(client, &reg).await {
                Ok(_) => successful += 1,
                Err(e) => {
                    failed += 1;
                    failures.push(format!("{}: {}", table_name, e));
                }
            }

            // Enforce retention
            match Maintainer::enforce_retention_policy(client, &reg).await {
                Ok(_) => {}
                Err(e) => {
                    if !failures.iter().any(|f| f.starts_with(&table_name)) {
                        failures.push(format!("{}: {}", table_name, e));
                    }
                }
            }
        }

        let duration_secs = start_time.elapsed().as_secs();

        Ok(FleetMaintenanceReport {
            tables_processed: registration_count,
            successful,
            failed,
            duration_secs,
            failures,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FleetMaintenanceReport {
    pub tables_processed: usize,
    pub successful: usize,
    pub failed: usize,
    pub duration_secs: u64,
    pub failures: Vec<String>,
}

impl std::fmt::Display for FleetStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Fleet Status for {}: {} tables ({} healthy, {} need attention)",
            self.database, self.total_tables, self.healthy_tables, self.tables_needing_attention
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_health_status() {
        assert_eq!(TableHealthStatus::Healthy, TableHealthStatus::Healthy);
        assert_ne!(TableHealthStatus::Healthy, TableHealthStatus::Warning);
    }

    #[test]
    fn test_fleet_status_creation() {
        let status = FleetStatus {
            database: "testdb".to_string(),
            timestamp: "2026-07-19T12:00:00Z".to_string(),
            total_tables: 5,
            healthy_tables: 4,
            tables_needing_attention: 1,
            maintenance_summary: None,
            table_statuses: vec![],
        };

        assert_eq!(status.total_tables, 5);
        assert_eq!(status.healthy_tables, 4);
    }
}
