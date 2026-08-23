//! Diff rendering against hand-built snapshots (no database needed): the
//! pure diff/render logic gets exercised here so the DB-backed tests below
//! only need to prove the probes themselves are correct.

use std::collections::BTreeMap;

use pgcore::Outcome;
use rlsnap::diff;
use rlsnap::snapshot::{ColumnPriv, Finding, Snapshot, TablePriv};

fn empty_snapshot() -> Snapshot {
    Snapshot {
        format: 1,
        tool_version: "0.0.0".to_string(),
        target: "test".to_string(),
        mode: "behavioural".to_string(),
        privileges: BTreeMap::new(),
        policies: BTreeMap::new(),
        functions: BTreeMap::new(),
        findings: Vec::new(),
        data: None,
    }
}

#[test]
fn identical_snapshots_diff_to_nothing() {
    let a = empty_snapshot();
    let b = a.clone();
    let d = diff::diff(&a, &b).unwrap();
    assert!(d.is_empty());
    assert_eq!(d.exit_code(false), 0);
    assert_eq!(d.exit_code(true), 0);
}

#[test]
fn column_select_denial_appears_as_privilege_change() {
    let mut a = empty_snapshot();
    let mut b = empty_snapshot();

    let mut cols_before = BTreeMap::new();
    cols_before.insert(
        "owner".to_string(),
        ColumnPriv {
            select: Some(Outcome::Allowed),
            update: Some(Outcome::Allowed),
        },
    );
    let table_before = TablePriv {
        columns: cols_before,
        ..Default::default()
    };
    let mut persona_before = BTreeMap::new();
    persona_before.insert("staff".to_string(), table_before);
    a.privileges.insert("staff".to_string(), persona_before);

    let mut cols_after = BTreeMap::new();
    cols_after.insert(
        "owner".to_string(),
        ColumnPriv {
            select: Some(Outcome::DeniedPrivilege),
            update: Some(Outcome::Allowed),
        },
    );
    let table_after = TablePriv {
        columns: cols_after,
        ..Default::default()
    };
    let mut persona_after = BTreeMap::new();
    persona_after.insert("staff".to_string(), table_after);
    b.privileges.insert("staff".to_string(), persona_after);

    let d = diff::diff(&a, &b).unwrap();
    assert_eq!(d.privilege_changes.len(), 1);
    let c = &d.privilege_changes[0];
    assert_eq!(c.op, "select");
    assert_eq!(c.column.as_deref(), Some("owner"));
    assert_eq!(c.before, Some(Outcome::Allowed));
    assert_eq!(c.after, Some(Outcome::DeniedPrivilege));

    let rendered = diff::render_table(&d);
    assert!(rendered.contains("denied_privilege"));
    assert_eq!(d.exit_code(false), 1);
}

#[test]
fn finding_appearing_is_a_diff_regardless_of_strict_data() {
    let a = empty_snapshot();
    let mut b = empty_snapshot();
    b.findings.push(Finding {
        name: "assert:tenant_disjointness".to_string(),
        message: "3 row(s) violate assert".to_string(),
    });

    let d = diff::diff(&a, &b).unwrap();
    assert_eq!(d.finding_changes.len(), 1);
    assert_eq!(d.exit_code(false), 1);
    let rendered = diff::render_table(&d);
    assert!(rendered.contains("tenant_disjointness"));
}

#[test]
fn data_only_diff_is_gated_by_strict_data() {
    use rlsnap::snapshot::DataSection;
    let mut a = empty_snapshot();
    let mut b = empty_snapshot();
    let mut rows_a = BTreeMap::new();
    rows_a.insert("public.widgets".to_string(), 3i64);
    let mut per_persona_a = BTreeMap::new();
    per_persona_a.insert("staff".to_string(), rows_a);
    a.data = Some(DataSection {
        row_counts: per_persona_a,
    });

    let mut rows_b = BTreeMap::new();
    rows_b.insert("public.widgets".to_string(), 4i64);
    let mut per_persona_b = BTreeMap::new();
    per_persona_b.insert("staff".to_string(), rows_b);
    b.data = Some(DataSection {
        row_counts: per_persona_b,
    });

    let d = diff::diff(&a, &b).unwrap();
    assert_eq!(d.data_changes.len(), 1);
    assert_eq!(
        d.exit_code(false),
        0,
        "data changes must not affect exit code by default"
    );
    assert_eq!(d.exit_code(true), 1, "but must with --strict-data");
}

#[test]
fn diff_rejects_a_format_version_mismatch_instead_of_reporting_no_changes() {
    let a = empty_snapshot();
    let mut b = empty_snapshot();
    b.format = a.format + 1;

    let err = diff::diff(&a, &b)
        .expect_err("a format-version mismatch must be a loud error, not a silent \"no changes\"");
    assert!(matches!(err, diff::DiffError::FormatMismatch { .. }));
}

#[test]
fn diff_rejects_a_catalog_vs_behavioural_mode_mismatch() {
    let a = empty_snapshot();
    let mut b = empty_snapshot();
    b.mode = "catalog".to_string();

    let err = diff::diff(&a, &b).expect_err(
        "diffing a catalog-mode snapshot against a behavioural-mode one must be rejected, \
         not produce a wall of meaningless privilege changes",
    );
    assert!(matches!(err, diff::DiffError::ModeMismatch { .. }));
}

#[test]
fn findings_sharing_a_name_do_not_collapse_in_the_diff() {
    let a = empty_snapshot();
    let mut b = empty_snapshot();
    b.findings.push(Finding {
        name: "assert:x".to_string(),
        message: "first violation".to_string(),
    });
    b.findings.push(Finding {
        name: "assert:x".to_string(),
        message: "second violation".to_string(),
    });

    let d = diff::diff(&a, &b).unwrap();
    assert_eq!(
        d.finding_changes.len(),
        1,
        "two findings with the same name are still one name-level diff entry"
    );
    let after = d.finding_changes[0].after.as_ref().unwrap();
    assert!(
        after.contains("first violation") && after.contains("second violation"),
        "both findings sharing a name must survive into the diff, not have one \
         silently overwrite the other: got {after:?}"
    );
}

#[test]
fn json_diff_round_trips_through_serde() {
    let a = empty_snapshot();
    let mut b = empty_snapshot();
    b.functions
        .entry("anon".to_string())
        .or_default()
        .insert("public.myfunc()".to_string(), Outcome::Allowed);

    let d = diff::diff(&a, &b).unwrap();
    let json = diff::render_json(&d);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["function_changes"][0]["function"], "public.myfunc()");
}
