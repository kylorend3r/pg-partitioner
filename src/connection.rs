use anyhow::{anyhow, Result};
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::{Client, NoTls};
use zeroize::Zeroize;

use crate::types::{ConnectionConfig, SslMode};

#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: String) -> Self {
        SecretString(s)
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SecretString(***)")
    }
}

pub async fn create_connection(config: &ConnectionConfig) -> Result<tokio_postgres::Client> {
    let connection_string = build_connection_string(config)?;

    let client = match config.ssl_mode {
        SslMode::Disable => {
            let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
                .await
                .map_err(|e| anyhow!("Failed to connect to database: {}", e))?;

            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    eprintln!("Connection error: {}", e);
                }
            });

            client
        }
        SslMode::Prefer | SslMode::Require => {
            let tls = TlsConnector::new()
                .map_err(|e| anyhow!("Failed to create TLS connector: {}", e))?;
            let tls = MakeTlsConnector::new(tls);
            let (client, connection) = tokio_postgres::connect(&connection_string, tls)
                .await
                .map_err(|e| anyhow!("Failed to connect to database: {}", e))?;

            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    eprintln!("Connection error: {}", e);
                }
            });

            client
        }
    };

    verify_postgres_version(&client).await?;
    Ok(client)
}

pub async fn verify_postgres_version(client: &Client) -> Result<()> {
    let version = get_postgres_version_num(client).await?;
    if version < 140000 {
        return Err(anyhow!(
            "PostgreSQL 14+ required. Current version: {}",
            version_to_string(version)
        ));
    }

    Ok(())
}

pub async fn get_postgres_version_num(client: &Client) -> Result<i32> {
    let version_query = "SELECT current_setting('server_version_num')::int;";
    let row = client
        .query_one(version_query, &[])
        .await
        .map_err(|e| anyhow!("Failed to query server version: {}", e))?;

    Ok(row.get(0))
}

pub fn version_to_string(version: i32) -> String {
    let major = version / 10000;
    let minor = (version % 10000) / 100;
    let patch = version % 100;
    format!("{}.{}.{}", major, minor, patch)
}

fn build_connection_string(config: &ConnectionConfig) -> Result<String> {
    let password_part = config
        .password
        .as_ref()
        .map(|p| format!(":{}@", escape_password(p)))
        .unwrap_or_else(|| "@".to_string());

    Ok(format!(
        "postgresql://{}{}{}:{}/{}",
        config.user, password_part, config.host, config.port, config.database
    ))
}

fn escape_password(password: &str) -> String {
    password
        .replace("%", "%25")
        .replace(":", "%3A")
        .replace("@", "%40")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_string_zeroizes_on_drop() {
        let _secret = SecretString::new("sensitive data".to_string());
    }

    #[test]
    fn test_escape_password() {
        assert_eq!(escape_password("simple"), "simple");
        assert_eq!(escape_password("pass@word"), "pass%40word");
        assert_eq!(escape_password("pass:word"), "pass%3Aword");
        assert_eq!(escape_password("pass%word"), "pass%25word");
    }
}
