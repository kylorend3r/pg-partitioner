use anyhow::Result;
use chrono::Utc;
use serde_json::json;
use tokio_postgres::Client;

use crate::risk;
use crate::schema;
use crate::types::{InspectReport, PartitionSetInfo};

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

    Ok(InspectReport {
        timestamp: Utc::now().to_rfc3339(),
        database: get_database_name(client).await.unwrap_or_default(),
        tables,
        risks,
    })
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

    output
}

pub fn format_inspect_report_json(report: &InspectReport) -> Result<String> {
    let json = json!({
        "timestamp": report.timestamp,
        "database": report.database,
        "tables": report.tables,
        "risks": report.risks
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
        };

        let text = format_inspect_report_text(&report);
        assert!(text.contains("Database: testdb"));
        assert!(text.contains("public.events"));
        assert!(text.contains("OID: 12345"));
        assert!(text.contains("Range"));
        assert!(text.contains("12")); // child count
    }
}
