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
                .map_err(|e| match config.ssl_mode {
                    // Worth spelling out: this path is stricter than libpq's.
                    // native-tls validates the chain and the hostname, so
                    // `require` here behaves like libpq's `verify-full` -- a
                    // self-signed or internal-CA server certificate fails
                    // where `psql "sslmode=require"` would have connected.
                    SslMode::Require => anyhow!(
                        "Failed to connect to database with sslmode=require: {}. Either the \
                         server does not offer TLS (check `SHOW ssl`), or its certificate is \
                         not trusted by this machine's certificate store.",
                        e
                    ),
                    _ => anyhow!("Failed to connect to database: {}", e),
                })?;

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

    // `sslmode` must be in the string, not just implied by the connector we
    // hand to `connect`. Without it tokio-postgres assumes `prefer`, so
    // `--ssl-mode require` against a server with `ssl = off` would negotiate
    // down to cleartext and send the password and every DDL statement in the
    // open -- succeeding, which is the worst way to get this wrong.
    Ok(format!(
        "postgresql://{}{}{}:{}/{}?sslmode={}",
        config.user,
        password_part,
        config.host,
        config.port,
        config.database,
        config.ssl_mode.as_libpq_str()
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

    fn config(ssl_mode: SslMode) -> ConnectionConfig {
        ConnectionConfig {
            host: "db.example.com".to_string(),
            port: 6432,
            database: "app".to_string(),
            user: "deploy".to_string(),
            password: Some("pw".to_string()),
            ssl_mode,
        }
    }

    #[test]
    fn test_connection_string_carries_ssl_mode() {
        for (mode, expected) in [
            (SslMode::Disable, "sslmode=disable"),
            (SslMode::Prefer, "sslmode=prefer"),
            (SslMode::Require, "sslmode=require"),
        ] {
            let s = build_connection_string(&config(mode)).expect("builds");
            assert!(s.ends_with(expected), "{} lacks {}", s, expected);
        }
    }

    #[test]
    fn test_connection_string_shape() {
        assert_eq!(
            build_connection_string(&config(SslMode::Require)).expect("builds"),
            "postgresql://deploy:pw@db.example.com:6432/app?sslmode=require"
        );
    }

    #[test]
    fn test_connection_string_without_password() {
        let mut cfg = config(SslMode::Prefer);
        cfg.password = None;
        assert_eq!(
            build_connection_string(&cfg).expect("builds"),
            "postgresql://deploy@db.example.com:6432/app?sslmode=prefer"
        );
    }

    #[test]
    fn test_escape_password() {
        assert_eq!(escape_password("simple"), "simple");
        assert_eq!(escape_password("pass@word"), "pass%40word");
        assert_eq!(escape_password("pass:word"), "pass%3Aword");
        assert_eq!(escape_password("pass%word"), "pass%25word");
    }
}
