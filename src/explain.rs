use crate::types::{InspectReport, PartitionStrategy, RiskSignal};

pub fn explain_partition_setup(report: &InspectReport) -> String {
    let mut explanation = String::new();

    explanation.push_str(&format!(
        "Partitioning Analysis for database '{}'\n",
        report.database
    ));
    explanation.push_str(&"=".repeat(80));
    explanation.push_str("\n\n");

    if report.tables.is_empty() {
        explanation.push_str("📊 No partitioned tables found in this database.\n\n");
        explanation.push_str("This database uses single-table storage. Partitioning could help if:\n");
        explanation.push_str("  • You have large tables (>100 MB) with time-series data\n");
        explanation.push_str("  • You need to manage data retention (automatically drop old data)\n");
        explanation.push_str("  • Query performance is affected by full table scans\n");
    } else {
        explanation.push_str(&format!(
            "✅ Found {} partitioned table{}\n\n",
            report.tables.len(),
            if report.tables.len() == 1 { "" } else { "s" }
        ));

        for table in &report.tables {
            explanation.push_str(&format!("📋 Table: {}\n", table.full_name));
            explanation.push_str(&format!("   Strategy: {}\n", strategy_name(table.strategy)));
            explanation.push_str(&format!(
                "   Partition Key: {}\n",
                table.partition_key.columns.join(" + ")
            ));
            explanation.push_str(&format!(
                "   Size: {:.1} MB across {} partitions\n",
                table.size_bytes as f64 / 1024.0 / 1024.0,
                table.child_count
            ));

            match table.strategy {
                PartitionStrategy::Range => {
                    explanation.push_str(
                        "   This table uses range partitioning: data is divided into \
                     ranges (e.g., by time)\n",
                    );
                }
                PartitionStrategy::List => {
                    explanation.push_str(
                        "   This table uses list partitioning: specific values determine \
                     which partition holds rows\n",
                    );
                }
                PartitionStrategy::Hash => {
                    explanation.push_str(
                        "   This table uses hash partitioning: a hash function distributes \
                     rows evenly\n",
                    );
                }
            }

            if let Some(active) = &table.active_child {
                explanation.push_str(&format!(
                    "   Most active partition: {} ({} rows)\n",
                    active, table.row_count
                ));
            }

            explanation.push_str("\n");
        }
    }

    if !report.risks.is_empty() {
        explanation.push_str("⚠️  RISK SIGNALS DETECTED\n");
        explanation.push_str(&"─".repeat(80));
        explanation.push_str("\n\n");

        for (idx, signal) in report.risks.iter().enumerate() {
            explanation.push_str(&format!("{}. ", idx + 1));

            match signal {
                RiskSignal::UnpartitionedLargeTable {
                    table_name,
                    row_count,
                    size_bytes,
                } => {
                    explanation.push_str(&format!(
                        "Large unpartitioned table: {}\n",
                        table_name
                    ));
                    explanation.push_str(&format!(
                        "   {} rows, {:.1} MB\n",
                        row_count,
                        *size_bytes as f64 / 1024.0 / 1024.0
                    ));
                    explanation.push_str(
                        "   Consider partitioning to improve query performance and enable \
                     data retention policies.\n",
                    );
                }

                RiskSignal::StrayDefaultPartitionRows {
                    table_name,
                    row_count,
                } => {
                    explanation.push_str(&format!(
                        "Default partition contains {} out-of-range rows in {}\n",
                        row_count, table_name
                    ));
                    explanation.push_str(
                        "   Rows landed in the default partition instead of their \
                     intended partition.\n",
                    );
                    explanation.push_str(
                        "   This typically means: data arrived outside the premade range, \
                     or a new partition wasn't created on time.\n",
                    );
                }

                RiskSignal::PlannerRiskPartitionCount {
                    table_name,
                    child_count,
                } => {
                    explanation.push_str(&format!(
                        "High partition count in {}: {} partitions\n",
                        table_name, child_count
                    ));
                    explanation.push_str(
                        "   The query planner may become slow with this many partitions.\n",
                    );
                    explanation.push_str(
                        "   Consider increasing the retention or partitioning interval to \
                     consolidate older data.\n",
                    );
                }

                RiskSignal::NamingCollisionExposure {
                    table_name,
                    affected_identifiers,
                } => {
                    explanation.push_str(&format!(
                        "Naming collision risk in {}\n",
                        table_name
                    ));
                    explanation.push_str(&format!(
                        "   Identifiers that may collide (63-char limit): {}\n",
                        affected_identifiers.join(", ")
                    ));
                    explanation.push_str(
                        "   PostgreSQL truncates identifiers to 63 bytes. \
                     Two different names may collide.\n",
                    );
                }

                RiskSignal::MaxLocksPerTransactionHeadroom {
                    table_name,
                    estimated_locks,
                } => {
                    explanation.push_str(&format!(
                        "Lock budget exhaustion risk in {}\n",
                        table_name
                    ));
                    explanation.push_str(&format!(
                        "   Estimated locks needed: {} (default budget: 1600)\n",
                        estimated_locks
                    ));
                    explanation.push_str(
                        "   Operations like subpartitioning may fail. Consider raising \
                     max_locks_per_transaction.\n",
                    );
                }

                RiskSignal::TimezoneMismatch { description } => {
                    explanation.push_str("Timezone mismatch detected\n");
                    explanation.push_str(&format!("   {}\n", description));
                    explanation.push_str(
                        "   This can cause partition boundaries to be calculated incorrectly.\n",
                    );
                }
            }

            explanation.push_str("\n");
        }
    }

    explanation.push_str("For more detailed analysis, run:\n");
    explanation.push_str("  pg-partitioner doctor --table <table_name>\n");

    explanation
}

fn strategy_name(strategy: PartitionStrategy) -> &'static str {
    match strategy {
        PartitionStrategy::Range => "Range (by time, numeric range, etc.)",
        PartitionStrategy::List => "List (by discrete values)",
        PartitionStrategy::Hash => "Hash (distributed evenly)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PartitionKey, PartitionSetInfo};

    #[test]
    fn test_explain_no_partitions() {
        let report = InspectReport {
            timestamp: "2026-07-19T12:00:00Z".to_string(),
            database: "testdb".to_string(),
            tables: vec![],
            risks: vec![],
        };

        let explanation = explain_partition_setup(&report);
        assert!(explanation.contains("No partitioned tables"));
        assert!(explanation.contains("testdb"));
    }

    #[test]
    fn test_explain_with_partitions() {
        let report = InspectReport {
            timestamp: "2026-07-19T12:00:00Z".to_string(),
            database: "testdb".to_string(),
            tables: vec![PartitionSetInfo {
                table_oid: 12345,
                schema_name: "public".to_string(),
                table_name: "events".to_string(),
                full_name: "public.events".to_string(),
                strategy: PartitionStrategy::Range,
                partition_key: PartitionKey::single("created_at".to_string()),
                row_count: 1_000_000,
                size_bytes: 500_000_000,
                child_count: 12,
                active_child: Some("events_2026_07".to_string()),
                default_partition: None,
                premake_count: 0,
            }],
            risks: vec![],
        };

        let explanation = explain_partition_setup(&report);
        assert!(explanation.contains("Found 1 partitioned table"));
        assert!(explanation.contains("public.events"));
        assert!(explanation.contains("Range"));
    }
}
