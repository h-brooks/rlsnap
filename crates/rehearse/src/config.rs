//! Config + timeout resolution for rehearse.
//!
//! pgcore's `Config`/`Target` resolve `lock_timeout_ms` to a default of
//! 2000ms with no way to tell "explicitly set to 2000" apart from
//! "defaulted", because `Target` only carries the final resolved value.
//! rehearse wants a different, more conservative default (5000ms: a
//! rehearsal against production must never sit on a lock queue). Since
//! pgcore is frozen, rehearse re-parses the raw TOML here just far enough to
//! check whether `lock_timeout_ms` was actually present for the target, and
//! only then falls back to its own default. This is the one pgcore gap this
//! crate had to work around locally.

use std::path::{Path, PathBuf};

use pgcore::config::CONFIG_FILE_NAMES;
use pgcore::{Config, Target};

/// rehearse's own default lock timeout, used only when the target's config
/// does not set `lock_timeout_ms` at all.
pub const DEFAULT_LOCK_TIMEOUT_MS: u64 = 5000;

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
    /// use for `target`: pgcore's resolved `statement_timeout_ms` unchanged,
    /// and `lock_timeout_ms` replaced with rehearse's own default when the
    /// target's config table did not set it explicitly.
    pub fn effective_timeouts(&self, target: &Target) -> (u64, u64) {
        let lock_timeout_ms = if target_sets_lock_timeout(&self.raw, &target.name) {
            target.lock_timeout_ms
        } else {
            DEFAULT_LOCK_TIMEOUT_MS
        };
        (target.statement_timeout_ms, lock_timeout_ms)
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

fn target_sets_lock_timeout(raw_toml: &str, target_name: &str) -> bool {
    let Ok(value) = raw_toml.parse::<toml::Value>() else {
        return false;
    };
    value
        .get("targets")
        .and_then(|t| t.get(target_name))
        .and_then(|t| t.get("lock_timeout_ms"))
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_explicit_lock_timeout() {
        let raw = "[targets.a]\nurl_env = \"X\"\nlock_timeout_ms = 500\n";
        assert!(target_sets_lock_timeout(raw, "a"));
    }

    #[test]
    fn detects_absent_lock_timeout() {
        let raw = "[targets.a]\nurl_env = \"X\"\n";
        assert!(!target_sets_lock_timeout(raw, "a"));
    }

    #[test]
    fn absent_target_is_not_set() {
        let raw = "[targets.a]\nurl_env = \"X\"\nlock_timeout_ms = 500\n";
        assert!(!target_sets_lock_timeout(raw, "b"));
    }
}
