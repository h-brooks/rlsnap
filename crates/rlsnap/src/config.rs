//! rlsnap-specific config sections layered on top of `pgcore::Config`'s
//! `[targets.*]` parsing: schemas/excludes, personas, and asserts.
//!
//! The same config text is parsed twice: once by `pgcore::Config` (which
//! only looks at `[targets.*]`) and once here (which looks at everything
//! else). Unknown top-level keys are ignored by each side.

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use pgcore::config::CONFIG_FILE_NAMES;
use pgcore::Persona;

pub const DEFAULT_SNAPSHOT_PATH: &str = "rlsnap.snap.json";
pub const DEFAULT_POOL_SIZE: usize = 4;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(transparent)]
    Core(#[from] pgcore::config::ConfigError),
    #[error("failed to read config file {0}: {1}")]
    Read(String, std::io::Error),
    #[error("failed to parse config file {0}: {1}")]
    Parse(String, toml::de::Error),
    #[error("no config file found (looked for {0:?} in {1})")]
    NotFound(&'static [&'static str], String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

fn default_claims() -> Value {
    Value::Object(Default::default())
}

#[derive(Debug, Deserialize)]
struct RawPersona {
    name: String,
    role: String,
    #[serde(default = "default_claims")]
    claims: Value,
    setup_sql: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AssertCfg {
    pub name: String,
    pub sql: String,
    pub per_persona: bool,
}

#[derive(Debug, Deserialize, Default)]
struct RawRoot {
    #[serde(default)]
    schemas: Vec<String>,
    #[serde(default)]
    excludes: Vec<String>,
    #[serde(default)]
    personas: Vec<RawPersona>,
    #[serde(default)]
    asserts: Vec<AssertCfg>,
    snapshot_path: Option<String>,
    pool_size: Option<usize>,
}

/// A fully loaded rlsnap config: `pgcore`'s targets, plus rlsnap's own
/// sections.
#[derive(Debug, Clone)]
pub struct RlsnapConfig {
    pub core: pgcore::Config,
    pub schemas: Vec<String>,
    pub excludes: Vec<String>,
    pub personas: Vec<Persona>,
    pub asserts: Vec<AssertCfg>,
    pub snapshot_path: String,
    pub pool_size: usize,
}

/// The three Supabase-default personas, always present unless a config
/// persona of the same name overrides them.
fn built_in_personas() -> Vec<Persona> {
    vec![
        Persona::anon(),
        Persona::authenticated(),
        Persona::service_role(),
    ]
}

fn merge_personas(raw: Vec<RawPersona>) -> Vec<Persona> {
    let mut by_name = std::collections::BTreeMap::new();
    for p in built_in_personas() {
        by_name.insert(p.name.clone(), p);
    }
    for r in raw {
        let mut persona = Persona::new(r.name.clone(), r.role, r.claims);
        if let Some(sql) = r.setup_sql {
            persona = persona.with_setup_sql(sql);
        }
        by_name.insert(r.name, persona);
    }
    by_name.into_values().collect()
}

impl RlsnapConfig {
    fn from_text(text: &str, source: &str) -> Result<Self> {
        let core = pgcore::Config::parse(text)?;
        let raw: RawRoot =
            toml::from_str(text).map_err(|e| ConfigError::Parse(source.to_string(), e))?;
        let schemas = if raw.schemas.is_empty() {
            vec!["public".to_string()]
        } else {
            raw.schemas
        };
        Ok(RlsnapConfig {
            core,
            schemas,
            excludes: raw.excludes,
            personas: merge_personas(raw.personas),
            asserts: raw.asserts,
            snapshot_path: raw
                .snapshot_path
                .unwrap_or_else(|| DEFAULT_SNAPSHOT_PATH.to_string()),
            pool_size: raw.pool_size.unwrap_or(DEFAULT_POOL_SIZE),
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(path.display().to_string(), e))?;
        Self::from_text(&text, &path.display().to_string())
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_schemas_to_public() {
        let cfg = RlsnapConfig::from_text("[targets.local]\nurl_env = \"X\"\n", "<t>").unwrap();
        assert_eq!(cfg.schemas, vec!["public".to_string()]);
    }

    #[test]
    fn built_in_personas_always_present() {
        let cfg = RlsnapConfig::from_text("[targets.local]\nurl_env = \"X\"\n", "<t>").unwrap();
        let names: Vec<&str> = cfg.personas.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"anon"));
        assert!(names.contains(&"authenticated"));
        assert!(names.contains(&"service_role"));
    }

    #[test]
    fn config_persona_overrides_built_in_by_name() {
        let cfg = RlsnapConfig::from_text(
            r#"
            [targets.local]
            url_env = "X"

            [[personas]]
            name = "anon"
            role = "anon"
            claims = { extra = "yes" }
            "#,
            "<t>",
        )
        .unwrap();
        let anon = cfg.personas.iter().find(|p| p.name == "anon").unwrap();
        assert_eq!(anon.claims["extra"], "yes");
    }

    #[test]
    fn parses_asserts_and_extra_personas() {
        let cfg = RlsnapConfig::from_text(
            r#"
            schemas = ["public", "app"]
            excludes = ["storage.*"]

            [targets.local]
            url_env = "X"

            [[personas]]
            name = "staff"
            role = "authenticated"
            claims = { sub = "staff-1" }

            [[asserts]]
            name = "tenant_disjointness"
            sql = "SELECT 1"
            per_persona = false
            "#,
            "<t>",
        )
        .unwrap();
        assert_eq!(cfg.schemas, vec!["public", "app"]);
        assert_eq!(cfg.excludes, vec!["storage.*"]);
        assert!(cfg.personas.iter().any(|p| p.name == "staff"));
        assert_eq!(cfg.asserts.len(), 1);
        assert_eq!(cfg.asserts[0].name, "tenant_disjointness");
    }

    /// Guards against the schemas/excludes lines drifting back below a
    /// `[targets.*]` header, where TOML would silently fold them into that
    /// target's table instead of the top level.
    #[test]
    fn starter_toml_schemas_and_excludes_are_top_level() {
        let cfg = RlsnapConfig::from_text(crate::init::STARTER_TOML, "<starter>").unwrap();
        assert_eq!(cfg.schemas, vec!["public".to_string()]);
        assert!(cfg.excludes.iter().any(|e| e == "auth.*"));
        assert!(cfg.core.target("local").is_some());
        assert!(cfg.core.target("prod").is_some());
    }
}
