use anyhow::Result;
use tokio_postgres::Client;

use crate::schema;
use crate::types::{PartitionSetInfo, RiskSignal};

const LARGE_TABLE_THRESHOLD: i64 = 100 * 1024 * 1024; // 100 MB
pub(crate) const PLANNER_RISK_PARTITION_COUNT: usize = 1000;
const MAX_LOCKS_BUFFER: usize = 100;

pub async fn detect_risks(
    client: &Client,
    partition_sets: &[PartitionSetInfo],
) -> Result<Vec<RiskSignal>> {
    let mut signals = Vec::new();

    signals.extend(detect_unpartitioned_large_tables(client).await?);
    signals.extend(detect_default_partition_strays(client, partition_sets).await?);
    signals.extend(detect_planner_risks(partition_sets));
    signals.extend(detect_max_locks_headroom(partition_sets));

    Ok(signals)
}

async fn detect_unpartitioned_large_tables(client: &Client) -> Result<Vec<RiskSignal>> {
    let unpartitioned = schema::get_unpartitioned_large_tables(client).await?;

    let mut signals = Vec::new();
    for table in unpartitioned {
        if table.size_bytes > LARGE_TABLE_THRESHOLD {
            signals.push(RiskSignal::UnpartitionedLargeTable {
                table_name: table.full_name,
                row_count: table.row_count,
                size_bytes: table.size_bytes,
            });
        }
    }

    Ok(signals)
}

async fn detect_default_partition_strays(
    client: &Client,
    partition_sets: &[PartitionSetInfo],
) -> Result<Vec<RiskSignal>> {
    let mut signals = Vec::new();

    for table in partition_sets {
        if let Some(default_partition) = &table.default_partition {
            let row_count =
                schema::count_default_partition_rows(client, &table.full_name, default_partition)
                    .await
                    .unwrap_or(0);

            if row_count > 0 {
                signals.push(RiskSignal::StrayDefaultPartitionRows {
                    table_name: table.full_name.clone(),
                    row_count,
                });
            }
        }
    }

    Ok(signals)
}

fn detect_planner_risks(partition_sets: &[PartitionSetInfo]) -> Vec<RiskSignal> {
    let mut signals = Vec::new();

    for table in partition_sets {
        if table.child_count > PLANNER_RISK_PARTITION_COUNT {
            signals.push(RiskSignal::PlannerRiskPartitionCount {
                table_name: table.full_name.clone(),
                child_count: table.child_count,
            });
        }
    }

    signals
}

fn detect_max_locks_headroom(partition_sets: &[PartitionSetInfo]) -> Vec<RiskSignal> {
    let mut signals = Vec::new();

    for table in partition_sets {
        let estimated_locks = table.child_count * 2;
        if estimated_locks > 1600 - MAX_LOCKS_BUFFER {
            signals.push(RiskSignal::MaxLocksPerTransactionHeadroom {
                table_name: table.full_name.clone(),
                estimated_locks,
            });
        }
    }

    signals
}

pub fn format_risk_signal(signal: &RiskSignal) -> (String, String) {
    match signal {
        RiskSignal::UnpartitionedLargeTable {
            table_name,
            row_count,
            size_bytes,
        } => (
            "Unpartitioned Large Table".to_string(),
            format!(
                "{}: {} rows, {} MB",
                table_name,
                row_count,
                size_bytes / 1024 / 1024
            ),
        ),
        RiskSignal::StrayDefaultPartitionRows {
            table_name,
            row_count,
        } => (
            "Rows in Default Partition".to_string(),
            format!("{}: {} rows out-of-range", table_name, row_count),
        ),
        RiskSignal::PlannerRiskPartitionCount {
            table_name,
            child_count,
        } => (
            "High Partition Count".to_string(),
            format!(
                "{}: {} partitions (may impact query planner)",
                table_name, child_count
            ),
        ),
        RiskSignal::NamingCollisionExposure {
            table_name,
            affected_identifiers,
        } => (
            "Naming Collision Risk".to_string(),
            format!(
                "{}: identifiers may collide: {}",
                table_name,
                affected_identifiers.join(", ")
            ),
        ),
        RiskSignal::MaxLocksPerTransactionHeadroom {
            table_name,
            estimated_locks,
        } => (
            "Lock Budget Risk".to_string(),
            format!(
                "{}: estimated {} locks needed (default budget: 1600)",
                table_name, estimated_locks
            ),
        ),
        RiskSignal::TimezoneMismatch { description } => {
            ("Timezone Mismatch".to_string(), description.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_risk_signals() {
        let signal = RiskSignal::UnpartitionedLargeTable {
            table_name: "public.users".to_string(),
            row_count: 5_000_000,
            size_bytes: 1_000_000_000,
        };

        let (category, msg) = format_risk_signal(&signal);
        assert_eq!(category, "Unpartitioned Large Table");
        assert!(msg.contains("public.users"));
        assert!(msg.contains("5000000"));
        assert!(msg.contains("953")); // 1000 MB ≈ 953 MB
    }
}
