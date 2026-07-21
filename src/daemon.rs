use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};
use tokio_postgres::Client;

use crate::connection::create_connection;
use crate::fleet::{FleetManager, FleetMaintenanceReport};
use crate::types::ConnectionConfig;

const RECONNECT_INITIAL_BACKOFF_MS: u64 = 1_000;
const RECONNECT_MAX_BACKOFF_MS: u64 = 60_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// How often to run a premake + retention maintenance sweep.
    pub maintenance_interval_secs: u64,
    /// Upper bound on how long a single maintenance cycle may run before
    /// it's logged as overrunning. Also how long a shutdown signal that
    /// arrives mid-cycle will wait for that cycle to finish before the
    /// daemon exits on the next loop check.
    pub graceful_shutdown_timeout_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            maintenance_interval_secs: 3600, // 1 hour
            graceful_shutdown_timeout_secs: 30,
        }
    }
}

pub struct PartitionerDaemon {
    config: DaemonConfig,
    connection_config: ConnectionConfig,
}

impl PartitionerDaemon {
    pub fn new(config: DaemonConfig, connection_config: ConnectionConfig) -> Self {
        PartitionerDaemon {
            config,
            connection_config,
        }
    }

    /// Runs the maintenance loop until a SIGTERM/SIGINT (or Ctrl-C) is
    /// received. Intended to be the entire body of the `daemon` subcommand
    /// and to run under a supervisor (systemd, k8s) that restarts it on
    /// unexpected exit.
    ///
    /// Reconnects automatically if the database connection drops between
    /// cycles, with exponential backoff, rather than exiting — a long-lived
    /// process that dies on the first transient network blip defeats the
    /// point of running it as a daemon instead of a cron entry.
    pub async fn run(&self) -> Result<()> {
        tracing::info!(
            interval_secs = self.config.maintenance_interval_secs,
            graceful_shutdown_timeout_secs = self.config.graceful_shutdown_timeout_secs,
            "Starting partitioner daemon"
        );

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            // Only fails if every receiver has already been dropped, which
            // means the daemon loop has already exited on its own.
            let _ = shutdown_tx.send(true);
        });

        let mut client = match self.connect_with_retry(&mut shutdown_rx).await {
            Ok(client) => client,
            Err(e) => {
                tracing::info!("Daemon stopping before first connection: {}", e);
                return Ok(());
            }
        };

        let mut maintenance_interval =
            interval(Duration::from_secs(self.config.maintenance_interval_secs));
        // A cycle that occasionally overruns the interval shouldn't cause a
        // burst of back-to-back catch-up ticks once it's done.
        maintenance_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                _ = maintenance_interval.tick() => {}
                _ = shutdown_rx.changed() => {
                    tracing::info!("Shutdown signal received while idle, stopping daemon");
                    break;
                }
            }

            if client.is_closed() {
                tracing::warn!("Database connection lost, attempting to reconnect");
                match self.connect_with_retry(&mut shutdown_rx).await {
                    Ok(reconnected) => {
                        tracing::info!("Reconnected to database");
                        client = reconnected;
                    }
                    Err(e) => {
                        tracing::info!("Daemon stopping while reconnecting: {}", e);
                        break;
                    }
                }
            }

            let cycle_timeout = Duration::from_secs(self.config.graceful_shutdown_timeout_secs);
            match tokio::time::timeout(cycle_timeout, self.run_maintenance_cycle(&client)).await {
                Ok(Ok(report)) => {
                    tracing::info!(
                        tables_processed = report.tables_processed,
                        successful = report.successful,
                        failed = report.failed,
                        duration_secs = report.duration_secs,
                        "Maintenance cycle completed"
                    );

                    if !report.failures.is_empty() {
                        tracing::warn!(failures = ?report.failures, "Some tables failed maintenance");
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "Maintenance cycle failed");
                }
                Err(_) => {
                    tracing::error!(
                        timeout_secs = cycle_timeout.as_secs(),
                        "Maintenance cycle exceeded graceful_shutdown_timeout_secs; abandoning \
                         it client-side (it may still be finishing server-side until its own \
                         statement_timeout fires) and continuing to the next tick"
                    );
                }
            }

            if *shutdown_rx.borrow() {
                tracing::info!("Shutdown signal received during last cycle, exiting now that it has finished");
                break;
            }
        }

        tracing::info!("Daemon shutdown complete");
        Ok(())
    }

    /// Runs a single maintenance cycle directly, without the interval loop —
    /// used by tests and by callers that want to trigger one sweep on demand.
    pub async fn run_once(&self, client: &Client) -> Result<FleetMaintenanceReport> {
        self.run_maintenance_cycle(client).await
    }

    async fn run_maintenance_cycle(&self, client: &Client) -> Result<FleetMaintenanceReport> {
        FleetManager::run_fleet_maintenance(client).await
    }

    /// Connects with exponential backoff (1s, 2s, 4s, ... capped at 60s),
    /// retrying indefinitely so a database that's temporarily unreachable
    /// (restart, failover, network blip) doesn't take the daemon down with
    /// it. Aborts early if a shutdown signal arrives while waiting.
    async fn connect_with_retry(&self, shutdown_rx: &mut watch::Receiver<bool>) -> Result<Client> {
        let mut attempt: u32 = 0;

        loop {
            attempt += 1;

            match create_connection(&self.connection_config).await {
                Ok(client) => {
                    if attempt > 1 {
                        tracing::info!(attempt, "Connected to database after retrying");
                    }
                    return Ok(client);
                }
                Err(e) => {
                    let backoff_ms = reconnect_backoff_ms(attempt);
                    tracing::error!(
                        attempt,
                        error = %e,
                        backoff_ms,
                        "Failed to connect to database, retrying"
                    );

                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
                        _ = shutdown_rx.changed() => {
                            return Err(anyhow!("shutdown requested while connecting to database"));
                        }
                    }
                }
            }
        }
    }
}

fn reconnect_backoff_ms(attempt: u32) -> u64 {
    let shift = attempt.saturating_sub(1).min(6); // 2^6 * 1000ms = 64s, already past the cap
    RECONNECT_INITIAL_BACKOFF_MS
        .saturating_mul(1u64 << shift)
        .min(RECONNECT_MAX_BACKOFF_MS)
}

#[cfg(unix)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigterm =
        signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");

    tokio::select! {
        _ = sigterm.recv() => tracing::info!(signal = "SIGTERM", "Shutdown signal received"),
        _ = sigint.recv() => tracing::info!(signal = "SIGINT", "Shutdown signal received"),
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!(signal = "CTRL_C", "Shutdown signal received");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SslMode;

    fn test_connection_config() -> ConnectionConfig {
        ConnectionConfig {
            host: "localhost".to_string(),
            port: 5432,
            database: "postgres".to_string(),
            user: "postgres".to_string(),
            password: None,
            ssl_mode: SslMode::Prefer,
        }
    }

    #[test]
    fn test_daemon_config_default() {
        let config = DaemonConfig::default();
        assert_eq!(config.maintenance_interval_secs, 3600);
        assert_eq!(config.graceful_shutdown_timeout_secs, 30);
    }

    #[test]
    fn test_daemon_creation() {
        let config = DaemonConfig {
            maintenance_interval_secs: 1800,
            graceful_shutdown_timeout_secs: 60,
        };

        let daemon = PartitionerDaemon::new(config, test_connection_config());
        assert_eq!(daemon.config.maintenance_interval_secs, 1800);
    }

    #[test]
    fn test_reconnect_backoff_doubles_and_caps() {
        assert_eq!(reconnect_backoff_ms(1), 1_000);
        assert_eq!(reconnect_backoff_ms(2), 2_000);
        assert_eq!(reconnect_backoff_ms(3), 4_000);
        assert_eq!(reconnect_backoff_ms(4), 8_000);
        assert_eq!(reconnect_backoff_ms(10), 60_000);
        assert_eq!(reconnect_backoff_ms(100), 60_000);
    }
}
