//! Config loading: `rlsnap.toml` (also accepted as `pgkit.toml`).
//!
//! Connection strings are supplied only via a named environment variable.
//! A literal `url = "..."` key in a target is a hard configuration error.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
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

    #[error("target {0:?}: invalid connection URL: {1}")]
    InvalidUrl(String, tokio_postgres::Error),

    #[error("target {0:?}: failed to set up TLS: {1}")]
    Tls(String, String),
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
    /// `application_name` is set on the connection (the binary passes its
    /// own name, e.g. `"rlsnap"` or `"rehearse"`), and a 10-second
    /// `connect_timeout` is applied.
    ///
    /// TLS is negotiated via rustls, honouring the URL's `sslmode` (absent
    /// defaults to `prefer`: try TLS, fall back to plaintext if the server
    /// doesn't support it). See [`SslMode`] for exactly what each mode
    /// verifies.
    pub async fn connect(&self, application_name: &str) -> Result<tokio_postgres::Client> {
        let url = std::env::var(&self.url_env)
            .map_err(|_| ConfigError::EnvVarMissing(self.name.clone(), self.url_env.clone()))?;

        let ssl_mode = SslMode::from_url(&url);
        let stripped_url = strip_sslmode_param(&url);

        let mut pg_config = tokio_postgres::Config::from_str(&stripped_url)
            .map_err(|e| ConfigError::InvalidUrl(self.name.clone(), e))?;
        pg_config
            .ssl_mode(ssl_mode.tokio_postgres_mode())
            .application_name(application_name)
            .connect_timeout(Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS));

        let tls_config =
            tls_client_config(ssl_mode).map_err(|e| ConfigError::Tls(self.name.clone(), e))?;
        let tls = tokio_postgres_rustls::MakeRustlsConnect::new(tls_config);

        let (client, connection) = pg_config
            .connect(tls)
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

/// Default `connect_timeout` applied to every connection.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;

/// TLS verification mode, parsed from the connection URL's `sslmode` query
/// parameter (absent = [`SslMode::Prefer`]).
///
/// `verify-ca` and `verify-full` are folded into one mode
/// ([`SslMode::VerifyFull`]): both perform full certificate-chain-and-
/// hostname verification against the native root store. This crate does
/// not implement `verify-ca`'s weaker semantics (trusting any certificate
/// issued by a trusted CA without checking the hostname) as a separate,
/// distinct mode.
///
/// `disable`/`prefer`/`require` provide encryption without authenticating
/// the server (matching standard libpq semantics: only `verify-ca` and
/// `verify-full` verify who is actually being talked to) -- they differ
/// only in whether TLS is attempted at all, and whether a server that
/// declines TLS is tolerated:
/// - `disable`: never attempt TLS.
/// - `prefer`: attempt TLS; fall back to plaintext if the server doesn't
///   support it.
/// - `require`: attempt TLS; refuse to fall back if the server doesn't
///   support it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyFull,
}

impl SslMode {
    fn from_url(url: &str) -> Self {
        match sslmode_param(url).as_deref() {
            Some("disable") => SslMode::Disable,
            Some("prefer") => SslMode::Prefer,
            Some("require") => SslMode::Require,
            Some("verify-ca") | Some("verify-full") => SslMode::VerifyFull,
            _ => SslMode::Prefer,
        }
    }

    /// How this mode maps onto `tokio_postgres`'s own three-way `SslMode`,
    /// which only governs whether TLS is attempted and whether a server
    /// that declines it is tolerated. The actual verification strictness
    /// (none, vs full chain-and-hostname) is entirely down to which rustls
    /// `ClientConfig` is handed to the connector (see [`tls_client_config`]),
    /// not to this value.
    fn tokio_postgres_mode(self) -> tokio_postgres::config::SslMode {
        match self {
            SslMode::Disable => tokio_postgres::config::SslMode::Disable,
            SslMode::Prefer => tokio_postgres::config::SslMode::Prefer,
            SslMode::Require | SslMode::VerifyFull => tokio_postgres::config::SslMode::Require,
        }
    }
}

/// Extract the raw `sslmode` query-parameter value (lower-cased) from a
/// Postgres connection URL, if present. A small manual parser rather than a
/// URL crate dependency: the format here is constrained to `key=value`
/// pairs separated by `&` after a single `?`, which is all a Postgres
/// connection URL ever has.
fn sslmode_param(url: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        k.eq_ignore_ascii_case("sslmode")
            .then(|| v.to_ascii_lowercase())
    })
}

/// Remove every `sslmode=...` pair from `url`'s query string. `sslmode` is
/// parsed by this module directly (via [`sslmode_param`]) and handled
/// entirely by [`SslMode`]/[`tls_client_config`]; it must not also reach
/// `tokio_postgres::Config`'s own parser, which recognises only
/// `disable`/`prefer`/`require` and errors on `verify-ca`/`verify-full`.
fn strip_sslmode_param(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            !pair
                .split_once('=')
                .is_some_and(|(k, _)| k.eq_ignore_ascii_case("sslmode"))
        })
        .collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

/// Build the rustls `ClientConfig` for `mode`. Only `VerifyFull` performs
/// real certificate verification (against the native root store);
/// `Disable`/`Prefer`/`Require` use a verifier that accepts any certificate,
/// since those modes provide encryption without authentication (see
/// [`SslMode`]'s doc comment). `Disable`'s config is built but never
/// actually used: `tokio_postgres` never invokes the TLS connector at all
/// when its own `ssl_mode` is `Disable`.
fn tls_client_config(mode: SslMode) -> std::result::Result<rustls::ClientConfig, String> {
    match mode {
        SslMode::VerifyFull => {
            let result = rustls_native_certs::load_native_certs();
            if result.certs.is_empty() {
                return Err(format!(
                    "no native root certificates could be loaded: {:?}",
                    result.errors
                ));
            }
            let mut roots = rustls::RootCertStore::empty();
            let (_added, _ignored) = roots.add_parsable_certificates(result.certs);
            if roots.is_empty() {
                return Err(format!(
                    "no native root certificates could be parsed: {:?}",
                    result.errors
                ));
            }
            Ok(rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth())
        }
        SslMode::Disable | SslMode::Prefer | SslMode::Require => {
            Ok(rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert))
                .with_no_client_auth())
        }
    }
}

/// Accepts any server certificate without verification (see [`SslMode`]'s
/// doc comment for why `disable`/`prefer`/`require` deliberately do this).
#[derive(Debug)]
struct AcceptAnyServerCert;

impl ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
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

    #[test]
    fn sslmode_absent_defaults_to_prefer() {
        assert_eq!(SslMode::from_url("postgres://u:p@host/db"), SslMode::Prefer);
    }

    #[test]
    fn sslmode_disable_is_parsed() {
        assert_eq!(
            SslMode::from_url("postgres://u:p@host/db?sslmode=disable"),
            SslMode::Disable
        );
    }

    #[test]
    fn sslmode_prefer_is_parsed() {
        assert_eq!(
            SslMode::from_url("postgres://u:p@host/db?sslmode=prefer"),
            SslMode::Prefer
        );
    }

    #[test]
    fn sslmode_require_is_parsed() {
        assert_eq!(
            SslMode::from_url("postgres://u:p@host/db?sslmode=require"),
            SslMode::Require
        );
    }

    #[test]
    fn sslmode_verify_ca_and_verify_full_both_map_to_verify_full() {
        assert_eq!(
            SslMode::from_url("postgres://u:p@host/db?sslmode=verify-ca"),
            SslMode::VerifyFull
        );
        assert_eq!(
            SslMode::from_url("postgres://u:p@host/db?sslmode=verify-full"),
            SslMode::VerifyFull
        );
    }

    #[test]
    fn sslmode_is_case_insensitive() {
        assert_eq!(
            SslMode::from_url("postgres://u:p@host/db?sslmode=REQUIRE"),
            SslMode::Require
        );
        assert_eq!(
            SslMode::from_url("postgres://u:p@host/db?SSLMODE=require"),
            SslMode::Require
        );
    }

    #[test]
    fn sslmode_is_read_alongside_other_params() {
        assert_eq!(
            SslMode::from_url("postgres://u:p@host/db?connect_timeout=5&sslmode=require&x=1"),
            SslMode::Require
        );
    }

    #[test]
    fn unknown_sslmode_value_defaults_to_prefer() {
        assert_eq!(
            SslMode::from_url("postgres://u:p@host/db?sslmode=bogus"),
            SslMode::Prefer
        );
    }

    #[test]
    fn strip_sslmode_param_removes_only_that_key() {
        assert_eq!(
            strip_sslmode_param("postgres://u:p@host/db?sslmode=require&x=1"),
            "postgres://u:p@host/db?x=1"
        );
        assert_eq!(
            strip_sslmode_param("postgres://u:p@host/db?x=1&sslmode=require"),
            "postgres://u:p@host/db?x=1"
        );
        assert_eq!(
            strip_sslmode_param("postgres://u:p@host/db?sslmode=require"),
            "postgres://u:p@host/db"
        );
        assert_eq!(
            strip_sslmode_param("postgres://u:p@host/db"),
            "postgres://u:p@host/db"
        );
    }
}
