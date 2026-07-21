use anyhow::{Context, Result};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::{self, format::FmtSpan};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy)]
pub enum LogFormat {
    Text,
    Json,
}

/// Initializes logging to *both* the terminal and a log file, matching the
/// "dual terminal + file logging" behavior described in project-structure.md
/// — this isn't a either/or choice between the two.
///
/// Returns a `WorkerGuard` that must be kept alive for the life of the
/// process (bind it with `let _guard = init_logging(...)?;` in `main`, not
/// `let _ = ...`) — dropping it early flushes and stops the background
/// writer, silently truncating any log lines written after that point.
///
/// If `log_file` is `None`, a default path is used and its parent directory
/// is created if missing, so a fresh install logs to a real file out of the
/// box instead of only ever going to stdout.
pub fn init_logging(level: &str, format: LogFormat, log_file: Option<&Path>) -> Result<WorkerGuard> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));

    let log_path = match log_file {
        Some(p) => p.to_path_buf(),
        None => default_log_path(),
    };

    let log_dir = log_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let log_file_name = log_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Invalid log file path: {}", log_path.display()))?
        .to_owned();

    fs::create_dir_all(&log_dir)
        .with_context(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    let file_appender = tracing_appender::rolling::never(&log_dir, &log_file_name);
    let (non_blocking_file, guard) = tracing_appender::non_blocking(file_appender);

    let registry = tracing_subscriber::registry().with(env_filter);

    match format {
        LogFormat::Text => {
            let stdout_layer = fmt::layer()
                .with_span_events(FmtSpan::CLOSE)
                .with_target(true)
                .with_level(true)
                .with_writer(io::stdout);
            // File output is always structured JSON regardless of terminal
            // format, since a log file exists to be grepped/parsed later,
            // not read live in a scrolling terminal.
            let file_layer = fmt::layer()
                .json()
                .with_span_events(FmtSpan::CLOSE)
                .with_target(true)
                .with_level(true)
                .with_ansi(false)
                .with_writer(non_blocking_file);
            registry.with(stdout_layer).with(file_layer).init();
        }
        LogFormat::Json => {
            let stdout_layer = fmt::layer()
                .json()
                .with_span_events(FmtSpan::CLOSE)
                .with_target(true)
                .with_level(true)
                .with_writer(io::stdout);
            let file_layer = fmt::layer()
                .json()
                .with_span_events(FmtSpan::CLOSE)
                .with_target(true)
                .with_level(true)
                .with_ansi(false)
                .with_writer(non_blocking_file);
            registry.with(stdout_layer).with(file_layer).init();
        }
    }

    tracing::info!(log_file = %log_path.display(), "Logging initialized (terminal + file)");

    Ok(guard)
}

/// `$XDG_STATE_HOME/pg-partitioner/pg-partitioner.log`, falling back to
/// `~/.local/state/pg-partitioner/pg-partitioner.log`, then to a plain
/// relative file in the current directory if `$HOME` isn't set either (e.g.
/// some container/CI environments).
fn default_log_path() -> PathBuf {
    let base = if let Ok(dir) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(dir)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local").join("state")
    } else {
        return PathBuf::from("pg-partitioner.log");
    };

    base.join("pg-partitioner").join("pg-partitioner.log")
}

#[macro_export]
macro_rules! trace_query {
    ($query:expr, $($param:expr),*) => {
        tracing::trace!(query = $query, params = ?[$($param),*], "Executing query")
    };
}

#[macro_export]
macro_rules! info_action {
    ($action:expr) => {
        tracing::info!(action = $action, "Performing action")
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_log_path_uses_xdg_state_home_when_set() {
        std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-state-test");
        let path = default_log_path();
        std::env::remove_var("XDG_STATE_HOME");

        assert_eq!(
            path,
            PathBuf::from("/tmp/xdg-state-test/pg-partitioner/pg-partitioner.log")
        );
    }
}
