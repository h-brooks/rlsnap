//! Pure tests for `rlsnap::accept::scoped_merge` against hand-built
//! snapshots (no database needed), matching the style of
//! `diff_unit_test.rs`: the merge algorithm gets exercised here so the
//! DB-backed CLI test only needs to prove the flag is wired up.

use std::collections::BTreeMap;

use pgcore::catalog::FunctionInfo;
use pgcore::Outcome;
use rlsnap::accept::{self, AcceptError};
use rlsnap::snapshot::{ColumnPriv, Snapshot, TablePolicies, TablePriv};

fn empty_snapshot() -> Snapshot {
    Snapshot {
        format: 2,
        tool_version: "0.0.0".to_string(),
        target: "test".to_string(),
        mode: "behavioural".to_string(),
        privileges: BTreeMap::new(),
        policies: BTreeMap::new(),
        functions: BTreeMap::new(),
        function_defs: BTreeMap::new(),
        findings: Vec::new(),
        data: None,
    }
}

/// The motivating incident: a baseline hand-edited against a drifted
/// database recorded the wrong outcome for one function and one persona.
/// `accept --only <that function>` must fix only that function's entries
/// across every persona, leaving an unrelated table's privileges (also
/// drifted, but out of scope for this accept) byte-identical.
#[test]
fn only_updates_matching_function_across_all_personas_leaves_other_entries_untouched() {
    let mut baseline = empty_snapshot();
    baseline
        .functions
        .entry("anon".to_string())
        .or_default()
        .insert(
            "public.grant_billing_export_access(text)".to_string(),
            Outcome::DeniedPrivilege,
        );
    baseline
        .functions
        .entry("service_role".to_string())
        .or_default()
        .insert(
            "public.grant_billing_export_access(text)".to_string(),
            // Hand-edited wrong: a clean environment probes Allowed.
            Outcome::DeniedPrivilege,
        );
    // An unrelated table, also drifted relative to `current`, but not named
    // by the --only pattern: must survive the merge untouched.
    let mut unrelated_cols = BTreeMap::new();
    unrelated_cols.insert(
        "id".to_string(),
        ColumnPriv {
            select: Some(Outcome::Allowed),
            update: Some(Outcome::Allowed),
        },
    );
    baseline
        .privileges
        .entry("anon".to_string())
        .or_default()
        .insert(
            "public.widgets".to_string(),
            TablePriv {
                columns: unrelated_cols,
                ..Default::default()
            },
        );

    let mut current = baseline.clone();
    // The freshly probed, clean-environment truth for the matched function.
    current.functions.get_mut("anon").unwrap().insert(
        "public.grant_billing_export_access(text)".to_string(),
        Outcome::Allowed,
    );
    current.functions.get_mut("service_role").unwrap().insert(
        "public.grant_billing_export_access(text)".to_string(),
        Outcome::Allowed,
    );
    // The unrelated table also drifted in `current` (simulating the shared
    // dev database's unrelated drift) but is out of scope for this accept.
    current
        .privileges
        .get_mut("anon")
        .unwrap()
        .get_mut("public.widgets")
        .unwrap()
        .columns
        .get_mut("id")
        .unwrap()
        .select = Some(Outcome::DeniedPrivilege);

    let patterns = vec!["grant_billing_export_access".to_string()];
    let (merged, report) = accept::scoped_merge(&baseline, &current, &patterns).unwrap();

    assert_eq!(
        merged.functions["anon"]["public.grant_billing_export_access(text)"],
        Outcome::Allowed
    );
    assert_eq!(
        merged.functions["service_role"]["public.grant_billing_export_access(text)"],
        Outcome::Allowed
    );
    // The unrelated table's drifted outcome in `current` must NOT leak into
    // the merged baseline: it stays exactly as the original baseline had it.
    assert_eq!(
        merged.privileges["anon"]["public.widgets"].columns["id"].select,
        Some(Outcome::Allowed),
        "an entry not named by --only must stay byte-identical to the baseline"
    );
    assert_eq!(merged.privileges, baseline.privileges);

    assert_eq!(
        report.functions,
        vec!["public.grant_billing_export_access(text)".to_string()]
    );
    assert!(report.privileges.is_empty());
    assert!(report.policies.is_empty());
    assert!(report.function_defs.is_empty());
}

#[test]
fn only_updates_matching_table_privileges_by_glob() {
    let mut baseline = empty_snapshot();
    let mut current = empty_snapshot();

    for (persona, table, outcome) in [
        ("staff", "public.widgets", Outcome::DeniedPrivilege),
        ("staff", "public.orders", Outcome::Allowed),
    ] {
        baseline
            .privileges
            .entry(persona.to_string())
            .or_default()
            .insert(
                table.to_string(),
                TablePriv {
                    select_count: Some(outcome.clone()),
                    ..Default::default()
                },
            );
    }
    // `current` flips both tables; only "public.widgets" is in scope.
    for (persona, table, outcome) in [
        ("staff", "public.widgets", Outcome::Allowed),
        ("staff", "public.orders", Outcome::DeniedPrivilege),
    ] {
        current
            .privileges
            .entry(persona.to_string())
            .or_default()
            .insert(
                table.to_string(),
                TablePriv {
                    select_count: Some(outcome.clone()),
                    ..Default::default()
                },
            );
    }

    let patterns = vec!["public.widg*".to_string()];
    let (merged, report) = accept::scoped_merge(&baseline, &current, &patterns).unwrap();

    assert_eq!(
        merged.privileges["staff"]["public.widgets"].select_count,
        Some(Outcome::Allowed)
    );
    // Out of scope: stays at the baseline's (stale) value, even though
    // `current` shows it flipped too.
    assert_eq!(
        merged.privileges["staff"]["public.orders"].select_count,
        Some(Outcome::Allowed)
    );
    assert_eq!(report.privileges, vec!["public.widgets".to_string()]);
}

#[test]
fn only_updates_matching_policy_table() {
    let mut baseline = empty_snapshot();
    let mut current = empty_snapshot();

    baseline.policies.insert(
        "public.widgets".to_string(),
        TablePolicies {
            rls_enabled: true,
            rls_forced: false,
            policies: BTreeMap::new(),
        },
    );
    current.policies.insert(
        "public.widgets".to_string(),
        TablePolicies {
            rls_enabled: false,
            rls_forced: false,
            policies: BTreeMap::new(),
        },
    );

    let patterns = vec!["public.widgets".to_string()];
    let (merged, report) = accept::scoped_merge(&baseline, &current, &patterns).unwrap();

    assert!(!merged.policies["public.widgets"].rls_enabled);
    assert_eq!(report.policies, vec!["public.widgets".to_string()]);
}

#[test]
fn only_updates_matching_function_def() {
    let mut baseline = empty_snapshot();
    let mut current = empty_snapshot();

    baseline.function_defs.insert(
        "public.is_admin()".to_string(),
        FunctionInfo {
            definition: Some("SELECT true".to_string()),
            owner: "postgres".to_string(),
            volatility: "v".to_string(),
            ..Default::default()
        },
    );
    current.function_defs.insert(
        "public.is_admin()".to_string(),
        FunctionInfo {
            definition: Some("SELECT false".to_string()),
            owner: "postgres".to_string(),
            volatility: "v".to_string(),
            ..Default::default()
        },
    );

    let patterns = vec!["is_admin".to_string()];
    let (merged, report) = accept::scoped_merge(&baseline, &current, &patterns).unwrap();

    assert_eq!(
        merged.function_defs["public.is_admin()"].definition,
        Some("SELECT false".to_string())
    );
    assert_eq!(report.function_defs, vec!["public.is_admin()".to_string()]);
}

/// A matched function that no longer exists in `current` (dropped or
/// renamed) must be removed from the merged baseline, not left stale: the
/// whole point of --only is that the matched entries reflect current truth.
#[test]
fn only_removes_a_matched_entry_that_no_longer_exists_in_current() {
    let mut baseline = empty_snapshot();
    baseline
        .functions
        .entry("anon".to_string())
        .or_default()
        .insert("public.old_fn()".to_string(), Outcome::Allowed);
    let mut current = baseline.clone();
    current.functions.get_mut("anon").unwrap().clear();

    let patterns = vec!["old_fn".to_string()];
    let (merged, _report) = accept::scoped_merge(&baseline, &current, &patterns).unwrap();

    assert!(!merged.functions["anon"].contains_key("public.old_fn()"));
}

/// Top-level fields not covered by --only (format, tool_version, target,
/// mode, findings, data) stay exactly as the baseline had them: --only
/// touches only the four object-keyed sections.
#[test]
fn only_leaves_untouched_top_level_fields_alone() {
    let mut baseline = empty_snapshot();
    baseline.tool_version = "0.1.0".to_string();
    baseline
        .functions
        .entry("anon".to_string())
        .or_default()
        .insert("public.f()".to_string(), Outcome::DeniedPrivilege);

    let mut current = baseline.clone();
    current.tool_version = "9.9.9".to_string();
    current
        .functions
        .get_mut("anon")
        .unwrap()
        .insert("public.f()".to_string(), Outcome::Allowed);

    let patterns = vec!["f(".to_string()];
    let (merged, _report) = accept::scoped_merge(&baseline, &current, &patterns).unwrap();

    assert_eq!(merged.tool_version, "0.1.0");
}

#[test]
fn no_match_anywhere_is_a_clear_error() {
    let baseline = empty_snapshot();
    let current = empty_snapshot();

    let patterns = vec!["nonexistent_thing".to_string()];
    let err = accept::scoped_merge(&baseline, &current, &patterns)
        .expect_err("a pattern matching nothing must be a loud error");
    match err {
        AcceptError::NoMatch(pattern) => assert_eq!(pattern, "nonexistent_thing"),
        other => panic!("expected NoMatch, got {other:?}"),
    }
}

/// Each pattern is checked independently: one good pattern must not paper
/// over another pattern that matches nothing.
#[test]
fn one_matching_pattern_does_not_excuse_another_matching_nothing() {
    let mut baseline = empty_snapshot();
    baseline
        .functions
        .entry("anon".to_string())
        .or_default()
        .insert("public.real_fn()".to_string(), Outcome::Allowed);
    let current = baseline.clone();

    let patterns = vec!["real_fn".to_string(), "totally_absent".to_string()];
    let err = accept::scoped_merge(&baseline, &current, &patterns).unwrap_err();
    assert!(matches!(err, AcceptError::NoMatch(p) if p == "totally_absent"));
}

#[test]
fn format_mismatch_between_baseline_and_current_is_rejected() {
    let baseline = empty_snapshot();
    let mut current = empty_snapshot();
    current.format = baseline.format + 1;

    let err = accept::scoped_merge(&baseline, &current, &["anything".to_string()]).unwrap_err();
    assert!(matches!(err, AcceptError::FormatMismatch { .. }));
}
