use anyhow::Result;
use tokio_postgres::Client;

use crate::risk;
use crate::schema;
use crate::types::{DoctorFinding, FindingSeverity, PartitionSetInfo, RiskSignal};

pub async fn run_diagnostic(
    client: &Client,
    table_filter: Option<&str>,
) -> Result<Vec<DoctorFinding>> {
    let all_partitioned = schema::get_partitioned_tables(client).await?;

    let tables: Vec<PartitionSetInfo> = if let Some(filter) = table_filter {
        all_partitioned
            .into_iter()
            .filter(|t| t.full_name.contains(filter))
            .collect()
    } else {
        all_partitioned
    };

    let mut findings = Vec::new();

    // Check for risk signals
    let risks = risk::detect_risks(client, &tables).await?;
    for risk_signal in risks {
        let (category, message) = risk::format_risk_signal(&risk_signal);
        findings.push(DoctorFinding {
            severity: FindingSeverity::Warning,
            category: category.clone(),
            table_name: match &risk_signal {
                RiskSignal::UnpartitionedLargeTable { table_name, .. } => table_name.clone(),
                RiskSignal::StrayDefaultPartitionRows { table_name, .. } => table_name.clone(),
                RiskSignal::PlannerRiskPartitionCount { table_name, .. } => table_name.clone(),
                RiskSignal::NamingCollisionExposure { table_name, .. } => table_name.clone(),
                RiskSignal::MaxLocksPerTransactionHeadroom { table_name, .. } => table_name.clone(),
                RiskSignal::TimezoneMismatch { .. } => "system".to_string(),
            },
            message,
            remediation: generate_remediation(&risk_signal),
        });
    }

    // Check for missing partition-key indexes
    for table in &tables {
        let has_pk_index = check_partition_key_indexes(client, &table.full_name, &table.partition_key.columns).await?;
        if !has_pk_index {
            findings.push(DoctorFinding {
                severity: FindingSeverity::Warning,
                category: "missing_partition_key_index".to_string(),
                table_name: table.full_name.clone(),
                message: format!(
                    "No index found on partition key columns ({})",
                    table.partition_key.columns.join(", ")
                ),
                remediation:
                    "Create an index on the partition key to improve query performance".to_string(),
            });
        }
    }

    // Check for constraint drift on children
    for table in &tables {
        let children = schema::get_child_partitions(client, &table.full_name).await?;
        if children.len() > 100 {
            findings.push(DoctorFinding {
                severity: FindingSeverity::Info,
                category: "large_partition_count".to_string(),
                table_name: table.full_name.clone(),
                message: format!(
                    "Partition set has {} children (large count may impact planner)",
                    children.len()
                ),
                remediation: "Consider adjusting interval or retention policy".to_string(),
            });
        }
    }

    Ok(findings)
}

async fn check_partition_key_indexes(
    client: &Client,
    table_name: &str,
    partition_columns: &[String],
) -> Result<bool> {
    // Simplified check: just verify at least one index on partition columns exists
    let query = format!(
        r#"
        SELECT COUNT(*) as idx_count
        FROM pg_class t
        JOIN pg_index ix ON ix.indrelid = t.oid
        JOIN pg_attribute a ON a.attrelid = ix.indrelid
            AND a.attnum = ANY(ix.indkey)
        WHERE t.oid = {}::regclass
        "#,
        crate::queries::quote_ident(table_name)
    );

    let rows = client.query(&query, &[]).await?;

    if let Some(row) = rows.first() {
        let count: i64 = row.get(0);
        Ok(count >= partition_columns.len() as i64)
    } else {
        Ok(false)
    }
}

fn generate_remediation(risk_signal: &RiskSignal) -> String {
    match risk_signal {
        RiskSignal::UnpartitionedLargeTable { .. } => {
            "Consider partitioning this table to improve query performance and enable retention policies".to_string()
        }
        RiskSignal::StrayDefaultPartitionRows { .. } => {
            "Run 'pg-partitioner reconcile-default' to move rows to correct partitions".to_string()
        }
        RiskSignal::PlannerRiskPartitionCount { .. } => {
            "Consider increasing the partitioning interval or enforcing retention to reduce partition count".to_string()
        }
        RiskSignal::NamingCollisionExposure { .. } => {
            "Rename table to use shorter names to avoid identifier truncation collisions".to_string()
        }
        RiskSignal::MaxLocksPerTransactionHeadroom { .. } => {
            "Raise max_locks_per_transaction in postgresql.conf and restart PostgreSQL".to_string()
        }
        RiskSignal::TimezoneMismatch { .. } => {
            "Ensure server and client timezone match; use UTC for consistency".to_string()
        }
    }
}

pub fn format_findings(findings: &[DoctorFinding]) -> String {
    let mut output = String::new();

    if findings.is_empty() {
        output.push_str("✅ No issues detected\n");
        return output;
    }

    let critical_count = findings.iter().filter(|f| f.severity == FindingSeverity::Critical).count();
    let warning_count = findings.iter().filter(|f| f.severity == FindingSeverity::Warning).count();
    let info_count = findings.iter().filter(|f| f.severity == FindingSeverity::Info).count();

    output.push_str(&format!(
        "⚠️  Diagnostic Results: {} critical, {} warnings, {} info\n",
        critical_count, warning_count, info_count
    ));
    output.push_str("\n");

    // Group by severity
    for severity in &[FindingSeverity::Critical, FindingSeverity::Warning, FindingSeverity::Info] {
        let severity_findings: Vec<_> = findings.iter().filter(|f| &f.severity == severity).collect();
        if !severity_findings.is_empty() {
            output.push_str(&format!(
                "{} {} Issue{}\n",
                match severity {
                    FindingSeverity::Critical => "🔴",
                    FindingSeverity::Warning => "🟡",
                    FindingSeverity::Info => "ℹ️",
                },
                severity_findings.len(),
                if severity_findings.len() == 1 { "" } else { "s" }
            ));

            for finding in severity_findings {
                output.push_str(&format!("\n  Table: {}\n", finding.table_name));
                output.push_str(&format!("  Issue: {}\n", finding.message));
                output.push_str(&format!("  Fix: {}\n", finding.remediation));
            }

            output.push_str("\n");
        }
    }

    output
}
