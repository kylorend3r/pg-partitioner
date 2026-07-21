use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use std::collections::HashMap;
use tokio_postgres::Client;

use crate::registrations;
use crate::risk;
use crate::schema;
use crate::types::{
    InspectReport, PartitionRegistration, PartitionSetInfo, ReconciliationEntry,
    ReconciliationStatus, ReconciliationSummary,
};

pub async fn inspect_database(
    client: &Client,
    table_filter: Option<&str>,
) -> Result<InspectReport> {
    let all_partitioned = schema::get_partitioned_tables(client).await?;

    let tables: Vec<PartitionSetInfo> = if let Some(filter) = table_filter {
        all_partitioned
            .into_iter()
            .filter(|t| t.full_name.contains(filter))
            .collect()
    } else {
        all_partitioned
    };

    let risks = risk::detect_risks(client, &tables).await?;

    let registrations_table_exists =
        schema::table_exists(client, "partitioner", "partitioner_registrations")
            .await
            .unwrap_or(false);
    let all_registrations = registrations::list_registrations(client).await?;
    let registrations: Vec<PartitionRegistration> = if let Some(filter) = table_filter {
        all_registrations
            .into_iter()
            .filter(|r| format!("{}.{}", r.schema_name, r.table_name).contains(filter))
            .collect()
    } else {
        all_registrations
    };

    let reconciliation = reconcile(&tables, &registrations, registrations_table_exists);

    Ok(InspectReport {
        timestamp: Utc::now().to_rfc3339(),
        database: get_database_name(client).await.unwrap_or_default(),
        tables,
        risks,
        registrations,
        reconciliation,
    })
}

fn reconcile(
    tables: &[PartitionSetInfo],
    registrations: &[PartitionRegistration],
    registrations_table_exists: bool,
) -> ReconciliationSummary {
    let live_by_key: HashMap<(&str, &str), &PartitionSetInfo> = tables
        .iter()
        .map(|t| ((t.schema_name.as_str(), t.table_name.as_str()), t))
        .collect();
    let registered_keys: std::collections::HashSet<(&str, &str)> = registrations
        .iter()
        .map(|r| (r.schema_name.as_str(), r.table_name.as_str()))
        .collect();

    let mut entries = Vec::new();

    for registration in registrations {
        let key = (
            registration.schema_name.as_str(),
            registration.table_name.as_str(),
        );
        match live_by_key.get(&key) {
            None => entries.push(ReconciliationEntry {
                schema_name: registration.schema_name.clone(),
                table_name: registration.table_name.clone(),
                status: ReconciliationStatus::DriftMissing,
                detail: "Registered but no matching partitioned table found in the live catalog"
                    .to_string(),
            }),
            Some(live) => {
                let mut mismatches = Vec::new();
                if live.strategy != registration.strategy {
                    mismatches.push(format!(
                        "strategy: registered={:?} actual={:?}",
                        registration.strategy, live.strategy
                    ));
                }
                if live.partition_key.columns != registration.partition_key.columns {
                    mismatches.push(format!(
                        "partition key: registered=[{}] actual=[{}]",
                        registration.partition_key.columns.join(","),
                        live.partition_key.columns.join(",")
                    ));
                }

                if mismatches.is_empty() {
                    entries.push(ReconciliationEntry {
                        schema_name: registration.schema_name.clone(),
                        table_name: registration.table_name.clone(),
                        status: ReconciliationStatus::Healthy,
                        detail: "Registered configuration matches the live catalog".to_string(),
                    });
                } else {
                    entries.push(ReconciliationEntry {
                        schema_name: registration.schema_name.clone(),
                        table_name: registration.table_name.clone(),
                        status: ReconciliationStatus::DriftMismatch,
                        detail: mismatches.join("; "),
                    });
                }
            }
        }
    }

    for table in tables {
        let key = (table.schema_name.as_str(), table.table_name.as_str());
        if !registered_keys.contains(&key) {
            entries.push(ReconciliationEntry {
                schema_name: table.schema_name.clone(),
                table_name: table.table_name.clone(),
                status: ReconciliationStatus::Unmanaged,
                detail: "Partitioned in the live catalog but not registered".to_string(),
            });
        }
    }

    let note = if !registrations_table_exists {
        Some(
            "partitioner_registrations table not found; run `pg-partitioner install` to create it"
                .to_string(),
        )
    } else if registrations.is_empty() {
        Some(
            "No tables are registered yet; run `pg-partitioner register` to declare which tables to manage"
                .to_string(),
        )
    } else {
        None
    };

    ReconciliationSummary { entries, note }
}

async fn get_database_name(client: &Client) -> Result<String> {
    let row = client
        .query_one("SELECT current_database()", &[])
        .await?;
    Ok(row.get::<_, String>(0))
}

pub fn format_inspect_report_text(report: &InspectReport) -> String {
    let mut output = String::new();

    output.push_str(&format!("Database: {}\n", report.database));
    output.push_str(&format!("Timestamp: {}\n", report.timestamp));
    output.push_str("\n");

    if report.tables.is_empty() {
        output.push_str("No partitioned tables found.\n");
    } else {
        output.push_str(&format!("Partitioned Tables ({}):\n", report.tables.len()));
        output.push_str(&"─".repeat(80));
        output.push_str("\n");

        for table in &report.tables {
            output.push_str(&format!(
                "Table: {} (OID: {})\n",
                table.full_name, table.table_oid
            ));
            output.push_str(&format!("  Strategy: {:?}\n", table.strategy));
            output.push_str(&format!(
                "  Partition Key: {}\n",
                table.partition_key.columns.join(", ")
            ));
            output.push_str(&format!("  Children: {}\n", table.child_count));
            output.push_str(&format!("  Rows: {}\n", table.row_count));
            output.push_str(&format!(
                "  Size: {:.2} MB\n",
                table.size_bytes as f64 / 1024.0 / 1024.0
            ));

            if let Some(active) = &table.active_child {
                output.push_str(&format!("  Most Active Child: {}\n", active));
            }
            if let Some(default) = &table.default_partition {
                output.push_str(&format!("  Default Partition: {}\n", default));
            }
            output.push_str("\n");
        }
    }

    if !report.risks.is_empty() {
        output.push_str(&format!("Risk Signals ({}):\n", report.risks.len()));
        output.push_str(&"─".repeat(80));
        output.push_str("\n");

        for signal in &report.risks {
            let (category, message) = risk::format_risk_signal(signal);
            output.push_str(&format!("⚠  {}: {}\n", category, message));
        }
        output.push_str("\n");
    }

    if report.registrations.is_empty() {
        output.push_str("Registered Configuration: none\n\n");
    } else {
        output.push_str(&format!(
            "Registered Configuration ({}):\n",
            report.registrations.len()
        ));
        output.push_str(&"─".repeat(80));
        output.push_str("\n");

        for registration in &report.registrations {
            output.push_str(&format!(
                "Table: {}.{}\n",
                registration.schema_name, registration.table_name
            ));
            output.push_str(&format!(
                "  Strategy: {}\n",
                registration.strategy.as_registration_str()
            ));
            output.push_str(&format!(
                "  Partition Key: {}\n",
                registration.partition_key.columns.join(", ")
            ));
            output.push_str(&format!("  Interval: {}\n", registration.interval));
            output.push_str(&format!("  Premake: {}\n", registration.premake_count));
            if let Some(policy) = &registration.retention_policy {
                output.push_str(&format!(
                    "  Retention: {:?} ({})\n",
                    policy.policy_type, policy.value
                ));
            }
            output.push_str("\n");
        }
    }

    output.push_str("Configuration Reconciliation:\n");
    output.push_str(&"─".repeat(80));
    output.push_str("\n");

    if report.reconciliation.entries.is_empty() {
        output.push_str("No registered or unmanaged tables to reconcile.\n");
    } else {
        for entry in &report.reconciliation.entries {
            let symbol = match entry.status {
                crate::types::ReconciliationStatus::Healthy => "✓",
                crate::types::ReconciliationStatus::DriftMissing
                | crate::types::ReconciliationStatus::DriftMismatch => "✗",
                crate::types::ReconciliationStatus::Unmanaged => "⚠",
            };
            output.push_str(&format!(
                "{} {}.{} [{:?}]: {}\n",
                symbol, entry.schema_name, entry.table_name, entry.status, entry.detail
            ));
        }
    }

    if let Some(note) = &report.reconciliation.note {
        output.push_str(&format!("\nNote: {}\n", note));
    }

    output
}

pub fn format_inspect_report_json(report: &InspectReport) -> Result<String> {
    let json = json!({
        "timestamp": report.timestamp,
        "database": report.database,
        "tables": report.tables,
        "risks": report.risks,
        "registrations": report.registrations,
        "reconciliation": report.reconciliation
    });

    Ok(serde_json::to_string_pretty(&json)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PartitionKey, PartitionStrategy};

    #[test]
    fn test_format_inspect_report_text() {
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
            registrations: vec![],
            reconciliation: crate::types::ReconciliationSummary {
                entries: vec![],
                note: None,
            },
        };

        let text = format_inspect_report_text(&report);
        assert!(text.contains("Database: testdb"));
        assert!(text.contains("public.events"));
        assert!(text.contains("OID: 12345"));
        assert!(text.contains("Range"));
        assert!(text.contains("12")); // child count
    }
}
