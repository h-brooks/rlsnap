//! End-to-end test for the environment-drift hint on `rlsnap check`,
//! driving the real binary so the printed hint text is actually observed.

mod support;

use support::TestHarness;

const STAFF_PERSONA: &str = r#"
[[personas]]
name = "staff"
role = "authenticated"
claims = { sub = "staff-1" }
"#;

#[tokio::test]
async fn check_prints_the_drift_hint_once_when_a_cell_flips_denied_privilege_to_allowed() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); GRANT SELECT ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    let out = h.run_bin(&["accept", "--target", "test"]);
    assert_eq!(out.status, 0);

    // Widen a grant so a cell flips DeniedPrivilege -> Allowed: the exact
    // shape the hint targets. (SELECT was already allowed on the table
    // itself; UPDATE was not, so this flips the column-level UPDATE cell.)
    h.db.load_fixture("GRANT UPDATE ON widgets TO authenticated;")
        .await;

    let out = h.run_bin(&["check", "--target", "test"]);
    assert_eq!(out.status, 1, "the grant change must be a reported diff");
    assert!(
        out.stdout.contains("hint:"),
        "expected the drift hint in check's table output: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("DeniedPrivilege/Allowed flips"),
        "hint should name the flip it's about: {}",
        out.stdout
    );
    let hint_count = out.stdout.matches("hint:").count();
    assert_eq!(
        hint_count, 1,
        "the hint must appear exactly once even though multiple columns flipped: {}",
        out.stdout
    );

    h.close().await;
}

#[tokio::test]
async fn check_json_format_never_gets_the_hint_appended() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); GRANT SELECT ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    let out = h.run_bin(&["accept", "--target", "test"]);
    assert_eq!(out.status, 0);

    h.db.load_fixture("GRANT UPDATE ON widgets TO authenticated;")
        .await;

    let out = h.run_bin(&["check", "--target", "test", "--format", "json"]);
    assert_eq!(out.status, 1);
    // A hint line appended after the JSON body would make this fail to
    // parse; the JSON diff format is a documented, machine-readable
    // contract and must stay pure JSON.
    let _: serde_json::Value = serde_json::from_str(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "check --format json must stay valid JSON: {e}\n{}",
            out.stdout
        )
    });

    h.close().await;
}

#[tokio::test]
async fn check_does_not_print_the_hint_when_nothing_changed() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); GRANT SELECT ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    let out = h.run_bin(&["accept", "--target", "test"]);
    assert_eq!(out.status, 0);

    let out = h.run_bin(&["check", "--target", "test"]);
    assert_eq!(out.status, 0);
    assert!(
        !out.stdout.contains("hint:"),
        "no diff, no hint: {}",
        out.stdout
    );

    h.close().await;
}
