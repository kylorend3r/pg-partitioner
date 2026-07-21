use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use pg_partitioner::{
    config::ConfigResolver, connection::create_connection, credentials::resolve_password,
    doctor, explain, inspect, logging::LogFormat, plan, apply, maintain, export, types::MigrationConfig,
    types::PartitionKey, types::PartitionStrategy,
};

#[derive(Parser)]
#[command(name = "pg-partitioner")]
#[command(about = "PostgreSQL partitioning lifecycle management CLI")]
#[command(version)]
#[command(author)]
struct Cli {
    /// PostgreSQL host
    #[arg(long, env = "PG_HOST")]
    host: Option<String>,

    /// PostgreSQL port
    #[arg(long, env = "PG_PORT")]
    port: Option<u16>,

    /// PostgreSQL database
    #[arg(long, env = "PG_DATABASE")]
    database: Option<String>,

    /// PostgreSQL user
    #[arg(long, env = "PG_USER")]
    user: Option<String>,

    /// PostgreSQL password
    #[arg(long, env = "PG_PASSWORD")]
    password: Option<String>,

    /// SSL mode: disable, prefer, require
    #[arg(long, env = "PG_SSL_MODE")]
    ssl_mode: Option<String>,

    /// Path to config file
    #[arg(long)]
    config: Option<PathBuf>,

    /// Log level: trace, debug, info, warn, error
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Log format: text, json
    #[arg(long, default_value = "text")]
    log_format: String,

    /// Log file path (defaults to $XDG_STATE_HOME/pg-partitioner/pg-partitioner.log,
    /// or ~/.local/state/pg-partitioner/pg-partitioner.log; always logs to a file
    /// in addition to the terminal)
    #[arg(long)]
    log_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Inspect current partitioning setup
    Inspect {
        /// Filter tables by name pattern
        #[arg(long)]
        table: Option<String>,

        /// Output format: text, json
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// Explain partitioning setup in plain language
    Explain {
        /// Filter tables by name pattern
        #[arg(long)]
        table: Option<String>,
    },

    /// Run health check and get remediation suggestions
    Doctor {
        /// Filter tables by name pattern
        #[arg(long)]
        table: Option<String>,

        /// Output format: text, json
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// Plan a partitioning migration
    Plan {
        /// Schema name
        #[arg(long)]
        schema: String,

        /// Table name
        #[arg(long)]
        table: String,

        /// Output plan to file
        #[arg(long)]
        output: Option<PathBuf>,

        /// Output format: json, yaml
        #[arg(long, default_value = "json")]
        format: String,
    },

    /// Apply a partition migration plan
    Apply {
        /// Path to plan file
        #[arg(long)]
        plan_file: PathBuf,

        /// Dry run (don't execute)
        #[arg(long)]
        dry_run: bool,
    },

    /// Run maintenance sweep (premake + retention)
    Maintain {
        /// Dry run (don't execute)
        #[arg(long)]
        dry_run: bool,
    },

    /// Export partition configuration
    Export {
        /// Output file path
        #[arg(long)]
        output: PathBuf,

        /// Format: yaml, json
        #[arg(long, default_value = "yaml")]
        format: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let log_format = match cli.log_format.as_str() {
        "json" => LogFormat::Json,
        _ => LogFormat::Text,
    };

    let log_file = cli.log_file.as_deref();
    // Held for the lifetime of main: dropping this guard flushes and stops the
    // background file writer, so an early drop would silently truncate the log.
    let _log_guard = pg_partitioner::logging::init_logging(&cli.log_level, log_format, log_file)?;

    // Resolve configuration
    let config_resolver = ConfigResolver::new(cli.config.as_deref())?;
    let mut connection_config = config_resolver.resolve_connection_config(
        cli.host,
        cli.port,
        cli.database,
        cli.user.clone(),
        cli.password,
        cli.ssl_mode,
    )?;

    // Resolve password from credentials chain if not already provided
    if connection_config.password.is_none() {
        let password = resolve_password(None, &connection_config.user, &connection_config.host)?;
        if let Some(pwd) = password {
            connection_config.password = Some(pwd.expose_secret().to_string());
        }
    }

    // Create database connection
    let client = create_connection(&connection_config).await?;

    // Process commands
    match cli.command {
        Some(Commands::Inspect { table, format }) => {
            let report = inspect::inspect_database(&client, table.as_deref()).await?;

            let output = match format.as_str() {
                "json" => inspect::format_inspect_report_json(&report)?,
                _ => inspect::format_inspect_report_text(&report),
            };

            println!("{}", output);
        }

        Some(Commands::Explain { table: _ }) => {
            let report = inspect::inspect_database(&client, None).await?;
            let explanation = explain::explain_partition_setup(&report);
            println!("{}", explanation);
        }

        Some(Commands::Doctor { table, format }) => {
            let findings = doctor::run_diagnostic(&client, table.as_deref()).await?;

            let output = match format.as_str() {
                "json" => serde_json::to_string_pretty(&findings)?,
                _ => doctor::format_findings(&findings),
            };

            println!("{}", output);
        }

        Some(Commands::Plan {
            schema,
            table,
            output,
            format,
        }) => {
            // Create a basic migration config (Phase 1 simplified version)
            let config = MigrationConfig {
                source_table: format!("{}.{}", schema, table),
                partition_strategy: PartitionStrategy::Range,
                partition_key: PartitionKey::single("created_at".to_string()),
                interval: "1 month".to_string(),
                premake_count: 3,
                use_bulk_copy: false,
            };

            let plan_obj = plan::Planner::plan_migration(&client, &schema, &table, &config).await?;

            if let Some(output_path) = output {
                let plan_json = serde_json::to_string_pretty(&plan_obj)?;
                std::fs::write(&output_path, plan_json)?;
                println!("Plan written to {}", output_path.display());
            } else {
                let plan_json = serde_json::to_string_pretty(&plan_obj)?;
                println!("{}", plan_json);
            }
        }

        Some(Commands::Apply { plan_file, dry_run }) => {
            let plan_contents = std::fs::read_to_string(&plan_file)?;
            let plan_obj: pg_partitioner::types::Plan = serde_json::from_str(&plan_contents)?;

            if dry_run {
                println!("DRY RUN: Would apply {} actions", plan_obj.actions.len());
                for action in &plan_obj.actions {
                    println!("  - {}: {}", action.action_type.to_string(), action.description);
                }
            } else {
                let applier = apply::Applier::new(None);
                // Extract schema and table from the first action (simplified for Phase 1)
                if let Some(first_action) = plan_obj.actions.first() {
                    let parts: Vec<&str> = first_action.table_name.split('.').collect();
                    if parts.len() == 2 {
                        applier.apply_plan(&client, parts[0], parts[1], &plan_obj).await?;
                        println!("Plan applied successfully");
                    }
                }
            }
        }

        Some(Commands::Maintain { dry_run }) => {
            if dry_run {
                println!("DRY RUN: Would run maintenance sweep");
            } else {
                let summary = maintain::Maintainer::run_maintenance_sweep(&client).await?;
                println!(
                    "Maintenance complete: {} tables processed, {} partitions created, {} dropped",
                    summary.tables_processed, summary.partitions_created, summary.partitions_dropped
                );
                if !summary.failed_tables.is_empty() {
                    println!("Failed tables:");
                    for table in summary.failed_tables {
                        println!("  - {}", table);
                    }
                }
            }
        }

        Some(Commands::Export { output, format }) => {
            // For Phase 1, export empty registrations (full implementation in Phase 2)
            let registrations: Vec<pg_partitioner::types::PartitionRegistration> = Vec::new();

            match format.as_str() {
                "yaml" => {
                    export::ConfigExporter::export_to_yaml(&registrations, &output)?;
                }
                "json" => {
                    export::ConfigExporter::export_to_json(&registrations, &output)?;
                }
                _ => return Err(anyhow::anyhow!("Unknown format: {}", format)),
            }

            println!("Configuration exported to {}", output.display());
        }

        None => {
            println!("pg-partitioner - PostgreSQL partitioning CLI");
            println!("Use 'pg-partitioner --help' for usage information");
        }
    }

    Ok(())
}
