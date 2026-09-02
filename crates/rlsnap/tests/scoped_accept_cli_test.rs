//! End-to-end tests for `rlsnap accept --only`, driving the real built
//! binary (not the library seam) so the printed report and error text are
//! actually observed, against a real Postgres.
//!
//! The scenario models the incident this flag exists for: a developer runs
//! `accept --only <pattern>` because a wholesale accept would also
//! re-baseline unrelated drift they don't want captured. The unrelated
//! drift here is simulated by hand-editing the baseline after an initial
//! full accept, then confirming a scoped accept leaves it exactly alone.

mod support;

use support::TestHarness;

const STAFF_PERSONA: &str = r#"
[[personas]]
name = "staff"
role = "authenticated"
claims = { sub = "staff-1" }
"#;

#[tokio::test]
async fn only_updates_the_matched_function_and_leaves_unrelated_drift_alone() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); \
         GRANT SELECT ON widgets TO authenticated; \
         CREATE FUNCTION set_widget_owner(p_id int) RETURNS void AS $$ BEGIN END $$ \
             LANGUAGE plpgsql; \
         GRANT EXECUTE ON FUNCTION set_widget_owner(int) TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    // Establish a real baseline against the clean database.
    let out = h.run_bin(&["accept", "--target", "test"]);
    assert_eq!(out.status, 0, "initial accept failed: {}", out.stderr);

    // Hand-edit the baseline to inject two kinds of drift: the incident
    // itself (staff's outcome on set_widget_owner recorded wrong) and an
    // unrelated table entry that a wholesale accept would silently absorb.
    let path = h.path("rlsnap.snap.json");
    let mut snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        snap.functions["staff"]
            .keys()
            .find(|k| k.contains("set_widget_owner"))
            .map(|k| snap.functions["staff"][k].clone()),
        Some(pgcore::Outcome::Allowed),
        "sanity: staff really can execute the function before we corrupt it"
    );
    for (key, outcome) in snap.functions.get_mut("staff").unwrap().iter_mut() {
        if key.contains("set_widget_owner") {
            *outcome = pgcore::Outcome::DeniedPrivilege;
        }
    }
    snap.privileges
        .get_mut("staff")
        .unwrap()
        .get_mut("public.widgets")
        .unwrap()
        .select_count = Some(pgcore::Outcome::DeniedPrivilege);
    std::fs::write(&path, serde_json::to_string_pretty(&snap).unwrap() + "\n").unwrap();

    // A scoped accept naming only the function must fix the function's
    // entry and leave the hand-edited table drift exactly as it was.
    let out = h.run_bin(&["accept", "--target", "test", "--only", "set_widget_owner"]);
    assert_eq!(out.status, 0, "scoped accept failed: {}", out.stderr);
    assert!(
        out.stdout.contains("functions:"),
        "expected the functions section to list the matched entry: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("set_widget_owner"),
        "expected the matched function signature printed: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("privileges: (none)"),
        "privileges section must report nothing matched: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("policies: (none)"),
        "policies section must report nothing matched: {}",
        out.stdout
    );

    let merged: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let fixed = merged.functions["staff"]
        .iter()
        .find(|(k, _)| k.contains("set_widget_owner"))
        .map(|(_, v)| v.clone());
    assert_eq!(
        fixed,
        Some(pgcore::Outcome::Allowed),
        "the matched function must be fixed back to the clean-environment truth"
    );
    assert_eq!(
        merged.privileges["staff"]["public.widgets"].select_count,
        Some(pgcore::Outcome::DeniedPrivilege),
        "the unrelated hand-edited drift must survive a scoped accept untouched"
    );

    h.close().await;
}

#[tokio::test]
async fn only_with_a_pattern_matching_nothing_is_a_clear_error() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); GRANT SELECT ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    let out = h.run_bin(&["accept", "--target", "test"]);
    assert_eq!(out.status, 0);

    let out = h.run_bin(&[
        "accept",
        "--target",
        "test",
        "--only",
        "totally_nonexistent_object",
    ]);
    assert_ne!(out.status, 0, "a pattern matching nothing must fail");
    assert!(
        out.stderr.contains("totally_nonexistent_object") && out.stderr.contains("matched nothing"),
        "expected a clear message naming the pattern: {}",
        out.stderr
    );

    h.close().await;
}

#[tokio::test]
async fn only_without_an_existing_baseline_is_a_clear_error() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); GRANT SELECT ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    let out = h.run_bin(&["accept", "--target", "test", "--only", "widgets"]);
    assert_ne!(out.status, 0);
    assert!(
        out.stderr.contains("no baseline"),
        "expected the missing-baseline message: {}",
        out.stderr
    );

    h.close().await;
}
