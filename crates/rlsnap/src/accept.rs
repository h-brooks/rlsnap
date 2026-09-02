//! `rlsnap accept --only <pattern>`: merge a freshly probed snapshot into
//! an existing baseline, touching only the entries named by `--only`.
//!
//! This exists for the case a wholesale `accept` can't be run cleanly: the
//! probe environment carries unrelated drift (a shared dev database, say),
//! so re-baselining everything would also bake in that drift. `--only`
//! probes normally, but writes back just the matched entries, leaving every
//! other entry in the snapshot file byte-identical to the old baseline --
//! no hand-editing the JSON, and no risk of getting an untouched persona's
//! outcome wrong by typing it in by hand.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::glob::only_match;
use crate::snapshot::Snapshot;

#[derive(Debug, Error)]
pub enum AcceptError {
    #[error(
        "--only pattern {0:?} matched nothing in either the baseline or the freshly probed \
         snapshot (functions, function_defs, privileges, and policies were all checked)"
    )]
    NoMatch(String),
    #[error(
        "cannot scope an accept across snapshot format versions ({a} vs {b}) -- run a full \
         `accept` (without --only) to re-baseline on the current format first"
    )]
    FormatMismatch { a: u32, b: u32 },
    #[error(
        "cannot scope an accept across a {a:?}-mode baseline and a {b:?}-mode probe -- run \
         `accept --target <target>` (without --only) against a matching target"
    )]
    ModeMismatch { a: String, b: String },
}

/// The object identifiers actually touched by a scoped accept, one list per
/// section, in the order printed. Empty means no `--only` pattern matched
/// anything in that section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OnlyReport {
    pub functions: Vec<String>,
    pub function_defs: Vec<String>,
    pub privileges: Vec<String>,
    pub policies: Vec<String>,
}

/// Object keys across a persona-major section (`persona -> object -> V`,
/// e.g. `functions` or `privileges`): the union of every object name seen
/// under any persona in either snapshot, filtered to those `patterns`
/// match.
fn matched_persona_major_keys<V>(
    baseline: &BTreeMap<String, BTreeMap<String, V>>,
    current: &BTreeMap<String, BTreeMap<String, V>>,
    patterns: &[String],
) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for objs in baseline.values().chain(current.values()) {
        keys.extend(objs.keys().cloned());
    }
    keys.retain(|k| only_match(patterns, k));
    keys
}

/// Object keys across a flat section (`object -> V`, e.g. `policies` or
/// `function_defs`), filtered to those `patterns` match.
fn matched_flat_keys<V>(
    baseline: &BTreeMap<String, V>,
    current: &BTreeMap<String, V>,
    patterns: &[String],
) -> BTreeSet<String> {
    let mut keys: BTreeSet<String> = baseline.keys().chain(current.keys()).cloned().collect();
    keys.retain(|k| only_match(patterns, k));
    keys
}

/// Replace `matched_keys` in every persona of `baseline` with `current`'s
/// value for that persona/key, dropping the key entirely for a persona
/// where `current` no longer has it. Every key not in `matched_keys`, and
/// every persona not touched by a matched key, is untouched.
fn merge_persona_major<V: Clone>(
    baseline: &BTreeMap<String, BTreeMap<String, V>>,
    current: &BTreeMap<String, BTreeMap<String, V>>,
    matched_keys: &BTreeSet<String>,
) -> BTreeMap<String, BTreeMap<String, V>> {
    let mut merged = baseline.clone();
    for objs in merged.values_mut() {
        for key in matched_keys {
            objs.remove(key);
        }
    }
    for (persona, cur_objs) in current {
        for key in matched_keys {
            if let Some(v) = cur_objs.get(key) {
                merged
                    .entry(persona.clone())
                    .or_default()
                    .insert(key.clone(), v.clone());
            }
        }
    }
    merged
}

/// Replace `matched_keys` in a flat section with `current`'s value, dropping
/// a key entirely if `current` no longer has it. Everything else in
/// `baseline` is untouched.
fn merge_flat<V: Clone>(
    baseline: &BTreeMap<String, V>,
    current: &BTreeMap<String, V>,
    matched_keys: &BTreeSet<String>,
) -> BTreeMap<String, V> {
    let mut merged = baseline.clone();
    for key in matched_keys {
        merged.remove(key);
        if let Some(v) = current.get(key) {
            merged.insert(key.clone(), v.clone());
        }
    }
    merged
}

/// Merge `current` (a freshly built snapshot) into `baseline`, updating
/// only the entries in `functions`, `function_defs`, `privileges`, and
/// `policies` whose object identifier (a function signature or a
/// `schema.table` name) matches any of `patterns` (substring or glob, see
/// [`only_match`]). Every other entry, and every top-level field other than
/// those four sections (format, tool_version, target, mode, findings,
/// data), is copied from `baseline` unchanged.
///
/// Errors if `baseline` and `current` aren't comparable (different format
/// or mode -- the same guard `diff::diff` applies), or if any single
/// pattern matches nothing at all: a silent no-op on a typo'd pattern would
/// defeat the point of a scoped accept.
pub fn scoped_merge(
    baseline: &Snapshot,
    current: &Snapshot,
    patterns: &[String],
) -> Result<(Snapshot, OnlyReport), AcceptError> {
    if baseline.format != current.format {
        return Err(AcceptError::FormatMismatch {
            a: baseline.format,
            b: current.format,
        });
    }
    if baseline.mode != current.mode {
        return Err(AcceptError::ModeMismatch {
            a: baseline.mode.clone(),
            b: current.mode.clone(),
        });
    }

    let function_keys =
        matched_persona_major_keys(&baseline.functions, &current.functions, patterns);
    let privilege_keys =
        matched_persona_major_keys(&baseline.privileges, &current.privileges, patterns);
    let function_def_keys =
        matched_flat_keys(&baseline.function_defs, &current.function_defs, patterns);
    let policy_keys = matched_flat_keys(&baseline.policies, &current.policies, patterns);

    for pattern in patterns {
        let single = std::slice::from_ref(pattern);
        let matched_anywhere = function_keys.iter().any(|k| only_match(single, k))
            || privilege_keys.iter().any(|k| only_match(single, k))
            || function_def_keys.iter().any(|k| only_match(single, k))
            || policy_keys.iter().any(|k| only_match(single, k));
        if !matched_anywhere {
            return Err(AcceptError::NoMatch(pattern.clone()));
        }
    }

    let merged = Snapshot {
        functions: merge_persona_major(&baseline.functions, &current.functions, &function_keys),
        privileges: merge_persona_major(&baseline.privileges, &current.privileges, &privilege_keys),
        function_defs: merge_flat(
            &baseline.function_defs,
            &current.function_defs,
            &function_def_keys,
        ),
        policies: merge_flat(&baseline.policies, &current.policies, &policy_keys),
        ..baseline.clone()
    };

    let report = OnlyReport {
        functions: function_keys.into_iter().collect(),
        function_defs: function_def_keys.into_iter().collect(),
        privileges: privilege_keys.into_iter().collect(),
        policies: policy_keys.into_iter().collect(),
    };

    Ok((merged, report))
}
