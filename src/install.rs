use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;

use crate::connection;
use crate::registrations;
use crate::save;
use crate::schema;
use crate::state;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentStatus {
    pub name: String,
    pub already_existed: bool,
    pub created: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallReport {
    pub postgres_version: String,
    pub postgres_version_ok: bool,
    pub components: Vec<ComponentStatus>,
}

/// Pre-checks and provisions every table pg-partitioner owns, under the
/// dedicated `partitioner` schema (created automatically if missing):
/// `partitioner_registrations` (config), `partitioner_state` (checkpoints),
/// and `partitioner_logbook` (audit log). Safe to run repeatedly.
pub async fn run_install(client: &Client) -> Result<InstallReport> {
    let version_num = connection::get_postgres_version_num(client).await?;
    let postgres_version = connection::version_to_string(version_num);
    let postgres_version_ok = version_num >= 140000;

    let mut components = Vec::new();

    let already_existed = schema::table_exists(client, "partitioner", "partitioner_registrations")
        .await
        .unwrap_or(false);
    let result = registrations::create_registrations_table(client).await;
    components.push(ComponentStatus {
        name: "partitioner_registrations".to_string(),
        already_existed,
        created: result.is_ok() && !already_existed,
        error: result.err().map(|e| e.to_string()),
    });

    let already_existed = schema::table_exists(client, "partitioner", "partitioner_state")
        .await
        .unwrap_or(false);
    let result = state::create_checkpoint_table(client).await;
    components.push(ComponentStatus {
        name: "partitioner_state".to_string(),
        already_existed,
        created: result.is_ok() && !already_existed,
        error: result.err().map(|e| e.to_string()),
    });

    let already_existed = schema::table_exists(client, "partitioner", "partitioner_logbook")
        .await
        .unwrap_or(false);
    let result = save::create_logbook_table(client).await;
    components.push(ComponentStatus {
        name: "partitioner_logbook".to_string(),
        already_existed,
        created: result.is_ok() && !already_existed,
        error: result.err().map(|e| e.to_string()),
    });

    Ok(InstallReport {
        postgres_version,
        postgres_version_ok,
        components,
    })
}

pub fn format_install_report_text(report: &InstallReport) -> String {
    let mut output = String::new();

    output.push_str(&format!(
        "PostgreSQL version: {} ({})\n",
        report.postgres_version,
        if report.postgres_version_ok {
            "OK, 14+"
        } else {
            "UNSUPPORTED, 14+ required"
        }
    ));
    output.push_str("\n");
    output.push_str("Components:\n");
    output.push_str(&"─".repeat(80));
    output.push_str("\n");

    for component in &report.components {
        let status = if let Some(err) = &component.error {
            format!("✗ FAILED: {}", err)
        } else if component.created {
            "✓ created".to_string()
        } else if component.already_existed {
            "= already present".to_string()
        } else {
            "? unknown".to_string()
        };
        output.push_str(&format!("  {:<32} {}\n", component.name, status));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_install_report_text_all_created() {
        let report = InstallReport {
            postgres_version: "16.1.0".to_string(),
            postgres_version_ok: true,
            components: vec![ComponentStatus {
                name: "partitioner_registrations".to_string(),
                already_existed: false,
                created: true,
                error: None,
            }],
        };

        let text = format_install_report_text(&report);
        assert!(text.contains("16.1.0"));
        assert!(text.contains("created"));
    }

    #[test]
    fn test_format_install_report_text_reports_error() {
        let report = InstallReport {
            postgres_version: "16.1.0".to_string(),
            postgres_version_ok: true,
            components: vec![ComponentStatus {
                name: "partitioner_state".to_string(),
                already_existed: false,
                created: false,
                error: Some("permission denied".to_string()),
            }],
        };

        let text = format_install_report_text(&report);
        assert!(text.contains("FAILED"));
        assert!(text.contains("permission denied"));
    }
}
