use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;
use tracing::info;

use crate::migration;
use crate::schema;
use crate::types::{PartitionRegistration, PartitionStrategy, RetryPolicy};

/// Whether a scheduled sweep can do anything for a registered table.
///
/// Only range has an answer to "what partition comes next": its children are a
/// sequence of periods, so the window can be extended forward without touching
/// a single existing row. The other two are recorded and reported, never
/// maintained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceEligibility {
    Maintainable,
    Informational { reason: &'static str },
}

/// Pure by design (no `&Client`), so the rule that decides whether a table is
/// ever touched by automation is testable on its own — see CLAUDE.md §4.1.
pub fn maintenance_eligibility(strategy: PartitionStrategy) -> MaintenanceEligibility {
    match strategy {
        PartitionStrategy::Range => MaintenanceEligibility::Maintainable,
        // The reason this is structural rather than unimplemented: a hash
        // child's contents are decided by `hash(key) % modulus`. Adding a
        // bucket changes the modulus, which changes that result for every row
        // already stored — so it is not a partition creation at all, it is a
        // full redistribution of the table. Nothing a maintenance sweep can
        // do unattended.
        PartitionStrategy::Hash => MaintenanceEligibility::Informational {
            reason: "hash buckets are fixed at creation; adding one changes the modulus, \
                     so every existing row would have to be rehashed and redistributed",
        },
        PartitionStrategy::List => MaintenanceEligibility::Informational {
            reason: "list partitions are added deliberately, one value set at a time, \
                     with `pg-partitioner add-partition`",
        },
    }
}

pub struct Maintainer;

impl Maintainer {
    pub async fn load_registrations(client: &Client) -> Result<Vec<PartitionRegistration>> {
        crate::registrations::list_registrations(client).await
    }

    /// Ensures the next `premake_count` periods ahead of today exist as real
    /// partitions on `registration`'s table, creating whatever's missing —
    /// including gaps from a manually-dropped partition, not just extending
    /// the tail, since the target window is recomputed fresh every call.
    ///
    /// Range only. Premake is a time-series idea: it extends a `FOR VALUES
    /// FROM/TO` sequence into the future. List has no "next period" to
    /// premake — its partitions are added deliberately, one value set at a
    /// time, via `add-partition` — and hash's buckets are all fixed at
    /// creation. Running the range DDL against either would fail on every
    /// sweep, so both are skipped outright.
    pub async fn premake_future_partitions(
        client: &Client,
        registration: &PartitionRegistration,
    ) -> Result<Vec<String>> {
        // Defence in depth: the sweep already filters these out, but this is
        // also a public entry point.
        if let MaintenanceEligibility::Informational { reason } =
            maintenance_eligibility(registration.strategy)
        {
            info!(
                table = format!("{}.{}", registration.schema_name, registration.table_name),
                strategy = registration.strategy.as_registration_str(),
                reason = reason,
                "Skipping premake: only range-partitioned tables have a forward window"
            );
            return Ok(Vec::new());
        }

        let retry_policy = RetryPolicy::default();
        let boundaries = migration::compute_forward_boundaries(
            client,
            &registration.interval,
            registration.premake_count,
        )
        .await?;

        let mut created = Vec::new();
        for pair in boundaries.windows(2) {
            let partition_name =
                migration::date_range_partition_name(&registration.table_name, &pair[0], &pair[1]);

            let already_exists =
                schema::table_exists(client, &registration.schema_name, &partition_name)
                    .await
                    .unwrap_or(false);

            // Skip the DDL call entirely when it's already there — not just
            // to avoid the noisy "relation ... already exists, skipping"
            // NOTICE that `CREATE TABLE IF NOT EXISTS` would otherwise emit
            // on every no-op run, but because there's nothing to do; a
            // concurrent creator racing us is still safe, since
            // `create_range_partition` keeps its own `IF NOT EXISTS`.
            if already_exists {
                continue;
            }

            migration::create_range_partition(
                client,
                &registration.schema_name,
                &registration.table_name,
                &partition_name,
                &pair[0],
                &pair[1],
                &retry_policy,
            )
            .await?;

            created.push(partition_name);
        }

        info!(
            table = format!("{}.{}", registration.schema_name, registration.table_name),
            premake_target = registration.premake_count,
            created = created.len(),
            "Premake window checked"
        );

        Ok(created)
    }

    pub async fn enforce_retention_policy(
        client: &Client,
        registration: &PartitionRegistration,
    ) -> Result<Vec<String>> {
        if let Some(policy) = &registration.retention_policy {
            info!(
                table = format!("{}.{}", registration.schema_name, registration.table_name),
                policy_type = format!("{:?}", policy.policy_type),
                "Enforcing retention policy"
            );

            // Placeholder for Phase 1
            Ok(Vec::new())
        } else {
            Ok(Vec::new())
        }
    }

    pub async fn run_maintenance_sweep(client: &Client) -> Result<MaintenanceSummary> {
        let registrations = Self::load_registrations(client).await?;

        let mut created_partitions = Vec::new();
        let mut dropped_partitions = Vec::new();
        let mut failed_tables = Vec::new();
        let mut informational_tables = Vec::new();
        let mut tables_processed = 0;

        for reg in registrations {
            let table_name = format!("{}.{}", reg.schema_name, reg.table_name);

            // Counted separately rather than folded into `tables_processed`:
            // reporting a hash table as "processed" overstates what the sweep
            // did, and hides the fact that it can never do anything there.
            if let MaintenanceEligibility::Informational { reason } =
                maintenance_eligibility(reg.strategy)
            {
                informational_tables.push(format!(
                    "{} ({}): {}",
                    table_name,
                    reg.strategy.as_registration_str(),
                    reason
                ));
                continue;
            }

            tables_processed += 1;

            // Premake future partitions
            match Self::premake_future_partitions(client, &reg).await {
                Ok(created) => {
                    created_partitions.extend(created);
                }
                Err(e) => {
                    failed_tables.push(format!("{}: {}", table_name, e));
                }
            }

            // Enforce retention
            match Self::enforce_retention_policy(client, &reg).await {
                Ok(dropped) => {
                    dropped_partitions.extend(dropped);
                }
                Err(e) => {
                    failed_tables.push(format!("{}: {}", table_name, e));
                }
            }
        }

        Ok(MaintenanceSummary {
            tables_processed,
            created_partitions,
            dropped_partitions,
            failed_tables,
            informational_tables,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceSummary {
    /// Registered tables the sweep could actually act on — range only.
    /// Informational registrations are excluded rather than counted here.
    pub tables_processed: usize,
    pub created_partitions: Vec<String>,
    pub dropped_partitions: Vec<String>,
    pub failed_tables: Vec<String>,
    /// `"schema.table (strategy): reason"` for every registration the sweep
    /// deliberately left alone. `#[serde(default)]` so a summary serialized by
    /// an older binary still deserializes.
    #[serde(default)]
    pub informational_tables: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_maintenance_summary_creation() {
        let summary = MaintenanceSummary {
            tables_processed: 5,
            created_partitions: vec!["events_2026_07_22_2026_07_23".to_string()],
            dropped_partitions: vec![],
            failed_tables: vec![],
            informational_tables: vec![],
        };

        assert_eq!(summary.tables_processed, 5);
        assert_eq!(summary.created_partitions.len(), 1);
    }

    #[test]
    fn test_only_range_is_maintainable() {
        assert_eq!(
            maintenance_eligibility(PartitionStrategy::Range),
            MaintenanceEligibility::Maintainable
        );

        // Both non-range strategies are informational, and each says why —
        // the reasons differ, so a single shared message would be wrong.
        for strategy in [PartitionStrategy::List, PartitionStrategy::Hash] {
            match maintenance_eligibility(strategy) {
                MaintenanceEligibility::Informational { reason } => {
                    assert!(!reason.is_empty(), "{:?} needs a reason", strategy)
                }
                MaintenanceEligibility::Maintainable => {
                    panic!("{:?} must not be maintainable", strategy)
                }
            }
        }

        assert_ne!(
            maintenance_eligibility(PartitionStrategy::List),
            maintenance_eligibility(PartitionStrategy::Hash)
        );
    }
}
