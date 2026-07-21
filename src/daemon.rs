use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::time::interval;
use tokio_postgres::Client;

use crate::fleet::{FleetManager, FleetMaintenanceReport};
use crate::logging;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    pub enabled: bool,
    pub maintenance_interval_secs: u64,
    pub log_level: String,
    pub graceful_shutdown_timeout_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            enabled: false,
            maintenance_interval_secs: 3600, // 1 hour
            log_level: "info".to_string(),
            graceful_shutdown_timeout_secs: 30,
        }
    }
}

pub struct PartitionerDaemon {
    config: DaemonConfig,
}

impl PartitionerDaemon {
    pub fn new(config: DaemonConfig) -> Self {
        PartitionerDaemon { config }
    }

    pub async fn run(&self, client: &Client) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }

        tracing::info!(
            interval_secs = self.config.maintenance_interval_secs,
            "Starting partitioner daemon"
        );

        let mut maintenance_interval = interval(Duration::from_secs(self.config.maintenance_interval_secs));

        loop {
            maintenance_interval.tick().await;

            match self.run_maintenance_cycle(client).await {
                Ok(report) => {
                    tracing::info!(
                        tables_processed = report.tables_processed,
                        successful = report.successful,
                        failed = report.failed,
                        duration_secs = report.duration_secs,
                        "Maintenance cycle completed"
                    );

                    if !report.failures.is_empty() {
                        tracing::warn!(
                            failures = ?report.failures,
                            "Some tables failed maintenance"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "Maintenance cycle failed");
                }
            }
        }
    }

    pub async fn run_once(&self, client: &Client) -> Result<FleetMaintenanceReport> {
        self.run_maintenance_cycle(client).await
    }

    async fn run_maintenance_cycle(&self, client: &Client) -> Result<FleetMaintenanceReport> {
        FleetManager::run_fleet_maintenance(client).await
    }

    pub async fn graceful_shutdown(&self) -> Result<()> {
        tracing::info!(
            timeout_secs = self.config.graceful_shutdown_timeout_secs,
            "Initiating graceful shutdown"
        );

        // Give in-flight maintenance operations time to complete
        tokio::time::sleep(Duration::from_secs(self.config.graceful_shutdown_timeout_secs)).await;

        tracing::info!("Daemon shutdown complete");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_config_default() {
        let config = DaemonConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.maintenance_interval_secs, 3600);
    }

    #[test]
    fn test_daemon_creation() {
        let config = DaemonConfig {
            enabled: true,
            maintenance_interval_secs: 1800,
            log_level: "debug".to_string(),
            graceful_shutdown_timeout_secs: 60,
        };

        let daemon = PartitionerDaemon::new(config);
        assert!(daemon.config.enabled);
    }
}
