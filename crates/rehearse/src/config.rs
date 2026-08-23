//! Config + timeout resolution for rehearse.
//!
//! pgcore's `Config`/`Target` resolve `lock_timeout_ms` and
//! `statement_timeout_ms` to fixed defaults (2000ms / 3000ms) with no way to
//! tell "explicitly set" apart from "defaulted", because `Target` only
//! carries the final resolved value. rehearse wants different defaults for
//! both, for opposite reasons:
//!
//! - `lock_timeout_ms`: more conservative (5000ms). A rehearsal against
//!   production must never sit on a lock queue.
//! - `statement_timeout_ms`: unlimited (0) rather than pgcore's 3000ms.
//!   pgcore's default is tuned for its own short privilege/catalog probes;
//!   for rehearse, the statement itself is the migration's own work (a
//!   table rewrite, a backfill, an index build), which can legitimately run
//!   far longer than 3 seconds. The transaction always rolls back
//!   regardless of how long a statement takes, so capping it by default
//!   only produces false failures, not added safety — the lock_timeout
//!   default above is what actually protects production.
//!
//! Since pgcore is frozen, rehearse re-parses the raw TOML here just far
//! enough to check whether each was actually present for the target, and
//! only then falls back to its own default. This is the one pgcore gap this
//! crate had to work around locally.

use std::path::{Path, PathBuf};

use pgcore::config::CONFIG_FILE_NAMES;
use pgcore::{Config, Target};

/// rehearse's own default lock timeout, used only when the target's config
/// does not set `lock_timeout_ms` at all.
pub const DEFAULT_LOCK_TIMEOUT_MS: u64 = 5000;

/// rehearse's own default statement timeout (unlimited), used only when the
/// target's config does not set `statement_timeout_ms` at all.
pub const DEFAULT_STATEMENT_TIMEOUT_MS: u64 = 0;

/// A loaded config plus the raw text it came from (needed to detect whether
/// `lock_timeout_ms` was explicitly set per target).
pub struct LoadedConfig {
    pub config: Config,
    raw: String,
}

impl LoadedConfig {
    /// Load from an explicit path, or discover `rlsnap.toml` / `pgkit.toml`
    /// in the current directory.
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => find_config_path(&std::env::current_dir()?)?,
        };
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("failed to read config file {}: {e}", path.display()))?;
        let config = Config::load(&path)
            .map_err(|e| anyhow::anyhow!("failed to load config file {}: {e}", path.display()))?;
        Ok(LoadedConfig { config, raw })
    }

    pub fn target(&self, name: &str) -> anyhow::Result<&Target> {
        self.config
            .target(name)
            .ok_or_else(|| anyhow::anyhow!("no target named {name:?} in config"))
    }

    /// The `(statement_timeout_ms, lock_timeout_ms)` rehearse should actually
    /// use for `target`: each replaced with rehearse's own default (see the
    /// module docs) when the target's config table did not set it
    /// explicitly.
    pub fn effective_timeouts(&self, target: &Target) -> (u64, u64) {
        let lock_timeout_ms = if target_sets(&self.raw, &target.name, "lock_timeout_ms") {
            target.lock_timeout_ms
        } else {
            DEFAULT_LOCK_TIMEOUT_MS
        };
        let statement_timeout_ms = if target_sets(&self.raw, &target.name, "statement_timeout_ms") {
            target.statement_timeout_ms
        } else {
            DEFAULT_STATEMENT_TIMEOUT_MS
        };
        (statement_timeout_ms, lock_timeout_ms)
    }
}

fn find_config_path(dir: &Path) -> anyhow::Result<PathBuf> {
    for name in CONFIG_FILE_NAMES {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "no config file found (looked for {:?} in {})",
        CONFIG_FILE_NAMES,
        dir.display()
    )
}

/// Whether `[targets.<target_name>]` in `raw_toml` sets `key` explicitly.
fn target_sets(raw_toml: &str, target_name: &str, key: &str) -> bool {
    let Ok(value) = raw_toml.parse::<toml::Value>() else {
        return false;
    };
    value
        .get("targets")
        .and_then(|t| t.get(target_name))
        .and_then(|t| t.get(key))
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_explicit_lock_timeout() {
        let raw = "[targets.a]\nurl_env = \"X\"\nlock_timeout_ms = 500\n";
        assert!(target_sets(raw, "a", "lock_timeout_ms"));
    }

    #[test]
    fn detects_absent_lock_timeout() {
        let raw = "[targets.a]\nurl_env = \"X\"\n";
        assert!(!target_sets(raw, "a", "lock_timeout_ms"));
    }

    #[test]
    fn absent_target_is_not_set() {
        let raw = "[targets.a]\nurl_env = \"X\"\nlock_timeout_ms = 500\n";
        assert!(!target_sets(raw, "b", "lock_timeout_ms"));
    }

    #[test]
    fn detects_explicit_statement_timeout() {
        let raw = "[targets.a]\nurl_env = \"X\"\nstatement_timeout_ms = 9000\n";
        assert!(target_sets(raw, "a", "statement_timeout_ms"));
    }

    #[test]
    fn detects_absent_statement_timeout() {
        let raw = "[targets.a]\nurl_env = \"X\"\n";
        assert!(!target_sets(raw, "a", "statement_timeout_ms"));
    }

    #[test]
    fn effective_timeouts_default_independently_when_unset() {
        let raw = "[targets.a]\nurl_env = \"X\"\n";
        let config = Config::parse(raw).unwrap();
        let loaded = LoadedConfig {
            config,
            raw: raw.to_string(),
        };
        let target = loaded.target("a").unwrap();
        let (statement_timeout_ms, lock_timeout_ms) = loaded.effective_timeouts(target);
        assert_eq!(statement_timeout_ms, DEFAULT_STATEMENT_TIMEOUT_MS);
        assert_eq!(lock_timeout_ms, DEFAULT_LOCK_TIMEOUT_MS);
    }

    #[test]
    fn effective_timeouts_respect_explicit_values() {
        let raw =
            "[targets.a]\nurl_env = \"X\"\nstatement_timeout_ms = 9000\nlock_timeout_ms = 1234\n";
        let config = Config::parse(raw).unwrap();
        let loaded = LoadedConfig {
            config,
            raw: raw.to_string(),
        };
        let target = loaded.target("a").unwrap();
        let (statement_timeout_ms, lock_timeout_ms) = loaded.effective_timeouts(target);
        assert_eq!(statement_timeout_ms, 9000);
        assert_eq!(lock_timeout_ms, 1234);
    }
}
