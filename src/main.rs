use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use pg_partitioner::{
    config::ConfigResolver, connection::create_connection, credentials::resolve_password,
    daemon::PartitionerDaemon, explain, inspect, install, logging::LogFormat, plan, apply,
    maintain, export, registrations, types::MigrationConfig, types::PartitionKey,
    types::PartitionStrategy, types::RetentionPolicy, types::RetentionType,
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

    /// Plan a partitioning migration.
    ///
    /// Without --template-table this converts the existing --schema/--table
    /// into a partitioned table (ATTACH-first cutover). With it, --schema/
    /// --table name a *new* table to create from that template instead; the
    /// template is only read, never modified or dropped.
    Plan {
        /// Schema name (of the table to convert, or of the table to create)
        #[arg(long)]
        schema: String,

        /// Table name (of the table to convert, or of the table to create)
        #[arg(long)]
        table: String,

        /// range | list | hash
        #[arg(long)]
        strategy: String,

        /// Partition key column(s), comma-separated for composite keys
        /// (list partitioning accepts exactly one column)
        #[arg(long, value_delimiter = ',', required = true)]
        key: Vec<String>,

        /// Schema of the template table (defaults to --schema)
        #[arg(long, requires = "template_table")]
        template_schema: Option<String>,

        /// Create --schema.--table as a new partitioned table whose columns
        /// are copied from this template table, instead of converting an
        /// existing table
        #[arg(long)]
        template_table: Option<String>,

        /// Range only. Defaults to "1 month"
        #[arg(long)]
        interval: Option<String>,

        /// Range only. Defaults to 3
        #[arg(long)]
        premake: Option<usize>,

        /// Hash only, and required there: how many buckets to create (the
        /// MODULUS every child partition shares). Fixed at creation --
        /// changing it later means recreating every bucket
        #[arg(long)]
        hash_partitions: Option<usize>,

        /// days | months | years | count — must be paired with --retention-value
        #[arg(long)]
        retention_type: Option<String>,

        #[arg(long)]
        retention_value: Option<i32>,

        /// Range cutover only, and required there: minimum date (YYYY-MM-DD)
        /// to start real per-period partitions from; data older than this
        /// lands in one legacy partition instead
        #[arg(long)]
        start_date: Option<String>,

        /// Output plan to file
        #[arg(long)]
        output: Option<PathBuf>,

        /// Output format: json, yaml
        #[arg(long, default_value = "json")]
        format: String,
    },

    /// Add one FOR VALUES IN (...) partition to a list-partitioned table.
    ///
    /// Run once per partition, as many times as needed. The target's
    /// partitioning strategy is read from the live catalog, so this fails
    /// clearly if pointed at a table that isn't list-partitioned.
    AddPartition {
        #[arg(long)]
        schema: String,

        /// The list-partitioned parent table
        #[arg(long)]
        table: String,

        /// Name for the new child partition
        #[arg(long)]
        partition_name: String,

        /// Comma-separated values this partition holds. Pass `NULL` for the
        /// partition that catches null partition-key values
        #[arg(long, value_delimiter = ',', required = true)]
        values: Vec<String>,

        /// Show the plan without executing it
        #[arg(long)]
        dry_run: bool,
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

    /// Pre-check and install pg-partitioner's own metadata tables under the
    /// `partitioner` schema (partitioner_registrations, partitioner_state,
    /// partitioner_logbook)
    Install {
        /// Output format: text, json
        #[arg(long, default_value = "text")]
        format: String,
    },

    /// Declare a table as managed by pg-partitioner (or update its config)
    Register {
        #[arg(long)]
        schema: String,

        #[arg(long)]
        table: String,

        /// range | list | hash
        #[arg(long)]
        strategy: String,

        /// Partition key column(s), comma-separated for composite keys
        #[arg(long, value_delimiter = ',', required = true)]
        key: Vec<String>,

        #[arg(long, default_value = "1 month")]
        interval: String,

        #[arg(long, default_value_t = 3)]
        premake: usize,

        /// days | months | years | count — must be paired with --retention-value
        #[arg(long)]
        retention_type: Option<String>,

        #[arg(long)]
        retention_value: Option<i32>,
    },

    /// Stop managing a table's partitioning configuration
    Unregister {
        #[arg(long)]
        schema: String,

        #[arg(long)]
        table: String,
    },

    /// Run as a long-lived daemon, sweeping premake + retention on a timer
    /// instead of relying on an external scheduler (cron/systemd timer/k8s
    /// CronJob). Intended to run under a supervisor such as systemd — see
    /// docs/daemon.md and packaging/systemd/pg-partitioner.service.
    Daemon {
        /// Seconds between maintenance sweeps
        #[arg(long, env = "PG_PARTITIONER_DAEMON_INTERVAL_SECS")]
        interval_secs: Option<u64>,

        /// Max seconds to wait for a maintenance cycle to finish, including
        /// one already in flight when a shutdown signal arrives
        #[arg(long, env = "PG_PARTITIONER_DAEMON_GRACEFUL_SHUTDOWN_SECS")]
        graceful_shutdown_timeout_secs: Option<u64>,
    },
}

fn build_retention_policy(
    retention_type: Option<String>,
    retention_value: Option<i32>,
) -> Result<Option<RetentionPolicy>> {
    match (retention_type, retention_value) {
        (Some(t), Some(v)) => Ok(Some(RetentionPolicy {
            policy_type: parse_retention_type(&t)?,
            value: v,
        })),
        (None, None) => Ok(None),
        _ => Err(anyhow::anyhow!(
            "--retention-type and --retention-value must both be provided, or neither"
        )),
    }
}

fn parse_retention_type(s: &str) -> Result<RetentionType> {
    match s.to_lowercase().as_str() {
        "days" => Ok(RetentionType::Days),
        "months" => Ok(RetentionType::Months),
        "years" => Ok(RetentionType::Years),
        "count" => Ok(RetentionType::Count),
        other => Err(anyhow::anyhow!(
            "Unknown retention type: {} (expected days, months, years, or count)",
            other
        )),
    }
}

fn parse_start_date(s: &str) -> Result<chrono::NaiveDate> {
    let date = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("Invalid --start-date '{}' (expected YYYY-MM-DD): {}", s, e))?;

    if date > chrono::Utc::now().date_naive() {
        return Err(anyhow::anyhow!(
            "--start-date '{}' is in the future; it must be today or earlier",
            s
        ));
    }

    Ok(date)
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

    // The daemon manages its own (reconnecting) connection internally, so it
    // must not share the eagerly-created client below — that client would
    // open one throwaway connection at startup before the daemon's own
    // retry/backoff logic ever gets a chance to run, and gives it nothing
    // to reconnect with if that one connection later drops.
    if let Some(Commands::Daemon {
        interval_secs,
        graceful_shutdown_timeout_secs,
    }) = cli.command
    {
        let daemon_config =
            config_resolver.resolve_daemon_config(interval_secs, graceful_shutdown_timeout_secs);
        let daemon = PartitionerDaemon::new(daemon_config, connection_config);
        daemon.run().await?;
        return Ok(());
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

        Some(Commands::Plan {
            schema,
            table,
            strategy,
            key,
            template_schema,
            template_table,
            interval,
            premake,
            hash_partitions,
            retention_type,
            retention_value,
            start_date,
            output,
            format,
        }) => {
            let partition_strategy = PartitionStrategy::from_registration_str(&strategy)?;
            let retention_policy = build_retention_policy(retention_type, retention_value)?;

            // `--interval`/`--premake`/`--start-date` describe a time-series
            // window, which only the range cutover flow has. They're rejected
            // rather than ignored in the template flow so a user who expects
            // them to do something finds out immediately.
            let template = template_table.map(|t| {
                format!("{}.{}", template_schema.unwrap_or_else(|| schema.clone()), t)
            });

            // `--hash-partitions` is meaningful only where hash buckets are
            // actually created, which today is the template flow alone.
            if partition_strategy == PartitionStrategy::Hash {
                if template.is_none() {
                    return Err(anyhow::anyhow!(
                        "--strategy hash requires --template-table: a hash-partitioned table \
                         cannot have a DEFAULT partition, so there is no bucket that could hold \
                         an existing table's rows and no way to convert one in place"
                    ));
                }
                match hash_partitions {
                    None => {
                        return Err(anyhow::anyhow!(
                            "--hash-partitions is required with --strategy hash (how many \
                             buckets to create)"
                        ))
                    }
                    Some(0) => {
                        return Err(anyhow::anyhow!("--hash-partitions must be at least 1"))
                    }
                    Some(_) => {}
                }
            } else if hash_partitions.is_some() {
                return Err(anyhow::anyhow!(
                    "--hash-partitions applies only to --strategy hash"
                ));
            }

            let start_date = if template.is_some() {
                for (flag, provided) in [
                    ("--interval", interval.is_some()),
                    ("--premake", premake.is_some()),
                    ("--start-date", start_date.is_some()),
                ] {
                    if provided {
                        return Err(anyhow::anyhow!(
                            "{} does not apply when creating a table from a template",
                            flag
                        ));
                    }
                }
                None
            } else {
                let start_date = start_date.ok_or_else(|| {
                    anyhow::anyhow!(
                        "--start-date is required when converting an existing table \
                         (pass --template-table to create a new one instead)"
                    )
                })?;
                Some(parse_start_date(&start_date)?.to_string())
            };

            let config = MigrationConfig {
                source_table: format!("{}.{}", schema, table),
                partition_strategy,
                partition_key: PartitionKey::new(key),
                interval: interval.unwrap_or_else(|| "1 month".to_string()),
                premake_count: premake.unwrap_or(3),
                use_bulk_copy: false,
                retention_policy,
                start_date,
                template_table: template,
                list_partition_name: None,
                list_partition_values: None,
                hash_modulus: hash_partitions,
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

            if !plan_obj.warnings.is_empty() {
                println!("\nWarnings:");
                for warning in &plan_obj.warnings {
                    println!("  - {}", warning);
                }
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
                // Extract schema and table from the first action (simplified for Phase 1).
                // A plan can legitimately have no actions at all — that's how the
                // template flow reports "the target already exists exactly as
                // requested" — so say so rather than exiting silently.
                match plan_obj.actions.first() {
                    None => println!("Nothing to apply: this plan contains no actions"),
                    Some(first_action) => {
                        let parts: Vec<&str> = first_action.table_name.split('.').collect();
                        if parts.len() != 2 {
                            return Err(anyhow::anyhow!(
                                "Plan action targets '{}'; expected a 'schema.table' name",
                                first_action.table_name
                            ));
                        }
                        let applier = apply::Applier::new(None);
                        applier.apply_plan(&client, parts[0], parts[1], &plan_obj).await?;
                        println!("Plan applied successfully");
                    }
                }
            }
        }

        Some(Commands::AddPartition {
            schema,
            table,
            partition_name,
            values,
            dry_run,
        }) => {
            let plan_obj = plan::Planner::plan_add_list_partition(
                &client,
                &schema,
                &table,
                &partition_name,
                &values,
            )
            .await?;

            for warning in &plan_obj.warnings {
                println!("Warning: {}", warning);
            }

            if dry_run {
                println!("DRY RUN: Would apply {} action(s)", plan_obj.actions.len());
                for action in &plan_obj.actions {
                    println!("  - {}: {}", action.action_type.to_string(), action.description);
                }
            } else {
                let applier = apply::Applier::new(None);
                applier.apply_plan(&client, &schema, &table, &plan_obj).await?;
                println!(
                    "Created list partition {}.{} on {}.{}",
                    schema, partition_name, schema, table
                );
            }
        }

        Some(Commands::Maintain { dry_run }) => {
            if dry_run {
                println!("DRY RUN: Would run maintenance sweep");
            } else {
                let summary = maintain::Maintainer::run_maintenance_sweep(&client).await?;
                println!(
                    "Maintenance complete: {} tables processed, {} partitions created, {} dropped",
                    summary.tables_processed,
                    summary.created_partitions.len(),
                    summary.dropped_partitions.len()
                );
                if !summary.created_partitions.is_empty() {
                    println!("Created:");
                    for partition in &summary.created_partitions {
                        println!("  - {}", partition);
                    }
                }
                if !summary.dropped_partitions.is_empty() {
                    println!("Dropped:");
                    for partition in &summary.dropped_partitions {
                        println!("  - {}", partition);
                    }
                }
                if !summary.failed_tables.is_empty() {
                    println!("Failed tables:");
                    for table in summary.failed_tables {
                        println!("  - {}", table);
                    }
                }
            }
        }

        Some(Commands::Export { output, format }) => {
            let regs = registrations::list_registrations(&client).await?;

            match format.as_str() {
                "yaml" => {
                    export::ConfigExporter::export_to_yaml(&regs, &output)?;
                }
                "json" => {
                    export::ConfigExporter::export_to_json(&regs, &output)?;
                }
                _ => return Err(anyhow::anyhow!("Unknown format: {}", format)),
            }

            println!("Configuration exported to {}", output.display());
        }

        Some(Commands::Install { format }) => {
            let report = install::run_install(&client).await?;

            let output = match format.as_str() {
                "json" => serde_json::to_string_pretty(&report)?,
                _ => install::format_install_report_text(&report),
            };
            println!("{}", output);

            if report.components.iter().any(|c| c.error.is_some()) {
                return Err(anyhow::anyhow!(
                    "One or more components failed to install; see report above"
                ));
            }
        }

        Some(Commands::Register {
            schema,
            table,
            strategy,
            key,
            interval,
            premake,
            retention_type,
            retention_value,
        }) => {
            registrations::create_registrations_table(&client).await?;

            let strategy = PartitionStrategy::from_registration_str(&strategy)?;
            let retention_policy = build_retention_policy(retention_type, retention_value)?;

            let registration = registrations::new_registration(
                schema,
                table,
                strategy,
                PartitionKey::new(key),
                interval,
                premake,
                retention_policy,
            );
            registrations::upsert_registration(&client, &registration).await?;
            println!(
                "Registered {}.{}",
                registration.schema_name, registration.table_name
            );
        }

        Some(Commands::Unregister { schema, table }) => {
            registrations::delete_registration(&client, &schema, &table).await?;
            println!("Unregistered {}.{}", schema, table);
        }

        // Daemon is dispatched earlier (before the eager connection above is
        // created) and always returns before reaching this match.
        Some(Commands::Daemon { .. }) => unreachable!("Daemon is handled before this match"),

        None => {
            println!("pg-partitioner - PostgreSQL partitioning CLI");
            println!("Use 'pg-partitioner --help' for usage information");
        }
    }

    Ok(())
}
