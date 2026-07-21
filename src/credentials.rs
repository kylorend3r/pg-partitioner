use anyhow::{anyhow, Result};
use std::env;
use std::fs;
use std::path::PathBuf;

use crate::connection::SecretString;

pub fn resolve_password(
    cli_password: Option<String>,
    user: &str,
    host: &str,
) -> Result<Option<SecretString>> {
    if let Some(pwd) = cli_password {
        return Ok(Some(SecretString::new(pwd)));
    }

    if let Ok(pwd) = env::var("PG_PASSWORD") {
        return Ok(Some(SecretString::new(pwd)));
    }

    if let Some(pwd) = read_pgpass(user, host)? {
        return Ok(Some(SecretString::new(pwd)));
    }

    Ok(None)
}

fn read_pgpass(user: &str, host: &str) -> Result<Option<String>> {
    let pgpass_path = if cfg!(windows) {
        dirs_home()?.join("AppData").join("postgresql").join("pgpass.conf")
    } else {
        dirs_home()?.join(".pgpass")
    };

    if !pgpass_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&pgpass_path)
        .map_err(|e| anyhow!("Failed to read .pgpass: {}", e))?;

    for line in contents.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() != 5 {
            continue;
        }

        let pgpass_host = unescape_pgpass(parts[0]);
        let _pgpass_port = unescape_pgpass(parts[1]);
        let _pgpass_db = unescape_pgpass(parts[2]);
        let pgpass_user = unescape_pgpass(parts[3]);
        let pgpass_password = unescape_pgpass(parts[4]);

        if (pgpass_host == "*" || pgpass_host == host)
            && (pgpass_user == "*" || pgpass_user == user)
        {
            return Ok(Some(pgpass_password));
        }
    }

    Ok(None)
}

fn dirs_home() -> Result<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("Could not determine home directory"))
}

fn unescape_pgpass(s: &str) -> String {
    s.replace("\\\\", "\\").replace("\\:", ":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unescape_pgpass() {
        assert_eq!(unescape_pgpass("simple"), "simple");
        assert_eq!(unescape_pgpass("pass\\:word"), "pass:word");
        assert_eq!(unescape_pgpass("pass\\\\word"), "pass\\word");
    }
}
