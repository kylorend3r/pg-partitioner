use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::types::{ConnectionConfig, SslMode};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    pub database: Option<DatabaseConfig>,
    pub logging: Option<LoggingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub ssl_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: Option<String>,
    pub format: Option<String>,
    pub file: Option<String>,
}

impl ConfigFile {
    pub fn load(config_path: Option<&Path>) -> Result<Self> {
        let path = if let Some(p) = config_path {
            p.to_path_buf()
        } else {
            Self::default_config_path()?
        };

        if !path.exists() {
            if config_path.is_some() {
                return Err(anyhow!("Config file not found: {}", path.display()));
            }
            return Ok(ConfigFile::default());
        }

        let contents = fs::read_to_string(&path)
            .map_err(|e| anyhow!("Failed to read config file: {}", e))?;

        toml::from_str(&contents).map_err(|e| anyhow!("Failed to parse config file: {}", e))
    }

    fn default_config_path() -> Result<PathBuf> {
        let config_dir = if let Ok(val) = std::env::var("XDG_CONFIG_HOME") {
            PathBuf::from(val)
        } else if let Ok(home) = std::env::var("HOME") {
            PathBuf::from(home).join(".config")
        } else {
            return Ok(PathBuf::from("config.toml"));
        };

        Ok(config_dir.join("pg-partitioner").join("config.toml"))
    }
}

impl Default for ConfigFile {
    fn default() -> Self {
        ConfigFile {
            database: None,
            logging: None,
        }
    }
}

pub struct ConfigResolver {
    file_config: ConfigFile,
}

impl ConfigResolver {
    pub fn new(config_path: Option<&Path>) -> Result<Self> {
        let file_config = ConfigFile::load(config_path)?;
        Ok(ConfigResolver { file_config })
    }

    pub fn resolve_connection_config(
        &self,
        cli_host: Option<String>,
        cli_port: Option<u16>,
        cli_database: Option<String>,
        cli_user: Option<String>,
        cli_password: Option<String>,
        cli_ssl: Option<String>,
    ) -> Result<ConnectionConfig> {
        let host = cli_host
            .or_else(|| {
                self.file_config
                    .database
                    .as_ref()
                    .and_then(|db| db.host.clone())
            })
            .or_else(|| std::env::var("PG_HOST").ok())
            .unwrap_or_else(|| "localhost".to_string());

        let port = cli_port
            .or_else(|| {
                self.file_config
                    .database
                    .as_ref()
                    .and_then(|db| db.port)
            })
            .or_else(|| std::env::var("PG_PORT").ok().and_then(|p| p.parse().ok()))
            .unwrap_or(5432);

        let database = cli_database
            .or_else(|| {
                self.file_config
                    .database
                    .as_ref()
                    .and_then(|db| db.database.clone())
            })
            .or_else(|| std::env::var("PG_DATABASE").ok())
            .unwrap_or_else(|| "postgres".to_string());

        let user = cli_user
            .or_else(|| {
                self.file_config
                    .database
                    .as_ref()
                    .and_then(|db| db.user.clone())
            })
            .or_else(|| std::env::var("PG_USER").ok())
            .ok_or_else(|| anyhow!("Database user must be specified"))?;

        let password = cli_password.or_else(|| {
            self.file_config
                .database
                .as_ref()
                .and_then(|db| db.password.clone())
        });

        let ssl_mode_str = cli_ssl
            .or_else(|| {
                self.file_config
                    .database
                    .as_ref()
                    .and_then(|db| db.ssl_mode.clone())
            })
            .or_else(|| std::env::var("PG_SSL_MODE").ok())
            .unwrap_or_else(|| "prefer".to_string());

        let ssl_mode = match ssl_mode_str.to_lowercase().as_str() {
            "disable" => SslMode::Disable,
            "require" => SslMode::Require,
            "prefer" | _ => SslMode::Prefer,
        };

        Ok(ConnectionConfig {
            host,
            port,
            database,
            user,
            password,
            ssl_mode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = ConfigFile::default();
        assert!(config.database.is_none());
        assert!(config.logging.is_none());
    }
}
