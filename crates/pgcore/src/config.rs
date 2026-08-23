//! Config loading: `rlsnap.toml` (also accepted as `pgkit.toml`).
//!
//! Connection strings are supplied only via a named environment variable.
//! A literal `url = "..."` key in a target is a hard configuration error.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

/// The two candidate config file names, checked in this order.
pub const CONFIG_FILE_NAMES: [&str; 2] = ["rlsnap.toml", "pgkit.toml"];

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("no config file found (looked for {0:?} in {1})")]
    NotFound(&'static [&'static str], String),

    #[error("failed to read config file {0}: {1}")]
    Read(String, std::io::Error),

    #[error("failed to parse config file {0}: {1}")]
    Parse(String, toml::de::Error),

    #[error("target {0:?} has a literal `url` key; connection strings must come from `url_env` (an environment variable name), never from config")]
    LiteralUrl(String),

    #[error("target {0:?}: missing required `url_env`")]
    MissingUrlEnv(String),

    #[error("target {0:?}: invalid `mode` {1:?} (expected \"catalog\" or \"behavioural\")")]
    InvalidMode(String, String),

    #[error("target {0:?}: environment variable {1:?} (from url_env) is not set")]
    EnvVarMissing(String, String),

    #[error("target {0:?}: failed to connect: {1}")]
    Connect(String, tokio_postgres::Error),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// Probe mode for a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Mode {
    /// Read-only: privilege functions and catalog reads only, no DML.
    Catalog,
    /// Full behavioural probe set (SELECT/INSERT/UPDATE/DELETE), always rolled back.
    Behavioural,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "catalog" => Ok(Mode::Catalog),
            "behavioural" | "behavioral" => Ok(Mode::Behavioural),
            other => Err(other.to_string()),
        }
    }
}

/// Raw, on-disk representation of a `[targets.<name>]` table.
#[derive(Debug, Deserialize)]
struct RawTarget {
    url_env: Option<String>,
    url: Option<String>,
    mode: Option<String>,
    max_rows: Option<u32>,
    statement_timeout_ms: Option<u64>,
    lock_timeout_ms: Option<u64>,
    insert_probes: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    targets: BTreeMap<String, RawTarget>,
}

/// A named connection target, fully resolved (defaults applied, validated).
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub url_env: String,
    pub mode: Mode,
    pub max_rows: u32,
    pub statement_timeout_ms: u64,
    pub lock_timeout_ms: u64,
    pub insert_probes: bool,
}

pub const DEFAULT_MAX_ROWS: u32 = 1000;
pub const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 3000;
pub const DEFAULT_LOCK_TIMEOUT_MS: u64 = 2000;

impl Target {
    fn from_raw(name: &str, raw: RawTarget) -> Result<Self> {
        if raw.url.is_some() {
            return Err(ConfigError::LiteralUrl(name.to_string()));
        }
        let url_env = raw
            .url_env
            .ok_or_else(|| ConfigError::MissingUrlEnv(name.to_string()))?;
        let mode = match raw.mode {
            None => Mode::Catalog,
            Some(m) => m
                .parse::<Mode>()
                .map_err(|bad| ConfigError::InvalidMode(name.to_string(), bad))?,
        };
        Ok(Target {
            name: name.to_string(),
            url_env,
            mode,
            max_rows: raw.max_rows.unwrap_or(DEFAULT_MAX_ROWS),
            statement_timeout_ms: raw
                .statement_timeout_ms
                .unwrap_or(DEFAULT_STATEMENT_TIMEOUT_MS),
            lock_timeout_ms: raw.lock_timeout_ms.unwrap_or(DEFAULT_LOCK_TIMEOUT_MS),
            insert_probes: raw.insert_probes.unwrap_or(true),
        })
    }

    /// Connect to this target. The connection string is read from the
    /// environment variable named by `url_env` — never from config.
    ///
    /// TLS is not implemented in v1; this connects in plaintext. Do not point
    /// this at a database that requires `sslmode=require` without a local
    /// trusted proxy (e.g. `supabase db` local dev, or a docker-compose
    /// service on a private network).
    pub async fn connect(&self) -> Result<tokio_postgres::Client> {
        let url = std::env::var(&self.url_env)
            .map_err(|_| ConfigError::EnvVarMissing(self.name.clone(), self.url_env.clone()))?;
        let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
            .await
            .map_err(|e| ConfigError::Connect(self.name.clone(), e))?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("pgcore: connection task error: {e}");
            }
        });
        Ok(client)
    }
}

/// A fully loaded and validated config file.
#[derive(Debug, Clone)]
pub struct Config {
    pub targets: BTreeMap<String, Target>,
}

impl Config {
    /// Parse config text directly (bypasses file discovery). Useful for tests.
    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawConfig =
            toml::from_str(text).map_err(|e| ConfigError::Parse("<string>".to_string(), e))?;
        let mut targets = BTreeMap::new();
        for (name, raw_target) in raw.targets {
            targets.insert(name.clone(), Target::from_raw(&name, raw_target)?);
        }
        Ok(Config { targets })
    }

    /// Load a specific config file path.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(path.display().to_string(), e))?;
        let raw: RawConfig =
            toml::from_str(&text).map_err(|e| ConfigError::Parse(path.display().to_string(), e))?;
        let mut targets = BTreeMap::new();
        for (name, raw_target) in raw.targets {
            targets.insert(name.clone(), Target::from_raw(&name, raw_target)?);
        }
        Ok(Config { targets })
    }

    /// Look for `rlsnap.toml`, then `pgkit.toml`, in `dir`.
    pub fn find_and_load(dir: &Path) -> Result<Self> {
        for name in CONFIG_FILE_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Self::load(&candidate);
            }
        }
        Err(ConfigError::NotFound(
            &CONFIG_FILE_NAMES,
            dir.display().to_string(),
        ))
    }

    pub fn target(&self, name: &str) -> Option<&Target> {
        self.targets.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_target() {
        let cfg = Config::parse(
            r#"
            [targets.local]
            url_env = "LOCAL_DB_URL"
            mode = "behavioural"
            max_rows = 500
        "#,
        )
        .unwrap();
        let t = cfg.target("local").unwrap();
        assert_eq!(t.url_env, "LOCAL_DB_URL");
        assert_eq!(t.mode, Mode::Behavioural);
        assert_eq!(t.max_rows, 500);
        assert_eq!(t.statement_timeout_ms, DEFAULT_STATEMENT_TIMEOUT_MS);
        assert!(t.insert_probes);
    }

    #[test]
    fn defaults_mode_to_catalog() {
        let cfg = Config::parse(
            r#"
            [targets.prod]
            url_env = "PROD_DB_URL"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.target("prod").unwrap().mode, Mode::Catalog);
    }

    #[test]
    fn rejects_literal_url() {
        let err = Config::parse(
            r#"
            [targets.oops]
            url = "postgres://x/y"
        "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::LiteralUrl(name) if name == "oops"));
    }

    #[test]
    fn rejects_missing_url_env() {
        let err = Config::parse(
            r#"
            [targets.oops]
            mode = "catalog"
        "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::MissingUrlEnv(name) if name == "oops"));
    }

    #[test]
    fn rejects_invalid_mode() {
        let err = Config::parse(
            r#"
            [targets.oops]
            url_env = "X"
            mode = "yolo"
        "#,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidMode(name, m) if name == "oops" && m == "yolo"));
    }

    #[test]
    fn finds_pgkit_toml_alias() {
        let dir = std::env::temp_dir().join(format!("pgcore-cfgtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pgkit.toml"), "[targets.t]\nurl_env = \"T_URL\"\n").unwrap();
        let cfg = Config::find_and_load(&dir).unwrap();
        assert!(cfg.target("t").is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
