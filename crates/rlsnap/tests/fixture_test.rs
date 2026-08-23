//! The incident-class fixture catalogue: each test is a before/after schema
//! pair and asserts that `rlsnap diff`'s table output names the problem.
//!
//! Every test drives the real `rlsnap::run` seam end to end against a fresh
//! Postgres database -- no mocking.

mod support;

use support::TestHarness;

const STAFF_PERSONA: &str = r#"
[[personas]]
name = "staff"
role = "authenticated"
claims = { sub = "staff-1" }
"#;

#[tokio::test]
async fn missing_column_grant_shows_as_denied_privilege() {
    // `staff` has column-scoped grants on the pre-existing columns only
    // (the incident this models: a role that relies on per-column grants,
    // not a blanket table-level grant that would cover a column added
    // later automatically).
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key, name text not null); \
         GRANT SELECT (id, name), UPDATE (id, name) ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "before.json"])
        .await
        .unwrap();

    // A migration adds a column but forgets its column-scoped grants.
    h.db.load_fixture("ALTER TABLE widgets ADD COLUMN owner text;")
        .await;

    h.run(&["snapshot", "--target", "test", "--out", "after.json"])
        .await
        .unwrap();

    let code = h
        .run(&[
            "diff",
            h.path("before.json").to_str().unwrap(),
            h.path("after.json").to_str().unwrap(),
        ])
        .await
        .unwrap();
    assert_eq!(code, 1);

    // Re-run capturing stdout is awkward through the library seam (it
    // prints directly), so assert on the JSON diff via the files instead.
    let before: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("before.json")).unwrap()).unwrap();
    let after: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("after.json")).unwrap()).unwrap();

    // The new column must be denied for `staff` specifically -- proving the
    // tool caught the missing column-scoped grant, not merely that some
    // built-in persona with no grant on the table at all was denied.
    let owner = &after.privileges["staff"]["public.widgets"].columns["owner"];
    assert_eq!(owner.select, Some(pgcore::Outcome::DeniedPrivilege));
    assert_eq!(owner.update, Some(pgcore::Outcome::DeniedPrivilege));
    // The pre-existing column-scoped grant still works for `staff`, so the
    // denial above is specific to the new column, not the whole table.
    let name = &after.privileges["staff"]["public.widgets"].columns["name"];
    assert_eq!(name.select, Some(pgcore::Outcome::Allowed));
    assert_eq!(name.update, Some(pgcore::Outcome::Allowed));

    let d = rlsnap::diff::diff(&before, &after).unwrap();
    let table = rlsnap::diff::render_table(&d);
    assert!(
        table.contains("denied_privilege"),
        "expected a denied_privilege cell for the new column, got:\n{table}"
    );
    assert!(table.contains("owner"));

    h.close().await;
}

#[tokio::test]
async fn check_without_with_rows_flag_still_evaluates_findings_in_behavioural_mode() {
    let asserts = r#"
        [[asserts]]
        name = "always_fails"
        sql = "SELECT 1"
        per_persona = false
    "#;
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); GRANT SELECT ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}\n{asserts}"),
        "behavioural",
        1000,
    )
    .await;

    // Baseline accepted with the data layer explicitly requested.
    let code = h
        .run(&["accept", "--target", "test", "--with-rows"])
        .await
        .unwrap();
    assert_eq!(code, 0);
    let baseline: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("rlsnap.snap.json")).unwrap())
            .unwrap();
    assert!(
        baseline
            .findings
            .iter()
            .any(|f| f.name == "assert:always_fails"),
        "expected the baseline to record the always-failing assert: {:?}",
        baseline.findings
    );

    // `check` run bare, exactly as spec item 14 and the brief document it
    // (no --with-rows), must see the SAME finding on a behavioural target --
    // not report it as removed just because the flag was omitted this time.
    let code = h.run(&["check", "--target", "test"]).await.unwrap();
    assert_eq!(
        code, 0,
        "an always-present finding must not appear as a diff just because \
         --with-rows was omitted from `check`"
    );

    h.close().await;
}

#[tokio::test]
async fn insert_allowed_but_returning_denied_are_distinct_outcomes() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id serial primary key, name text); \
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
         GRANT INSERT ON widgets TO authenticated; \
         GRANT USAGE, SELECT ON SEQUENCE widgets_id_seq TO authenticated; \
         CREATE POLICY widgets_insert ON widgets FOR INSERT TO authenticated WITH CHECK (true);",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await
        .unwrap();
    let snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("snap.json")).unwrap()).unwrap();

    let table = snap
        .privileges
        .get("staff")
        .unwrap()
        .get("public.widgets")
        .unwrap();
    assert_eq!(table.insert, Some(pgcore::Outcome::Allowed));
    assert_ne!(
        table.insert_returning,
        Some(pgcore::Outcome::Allowed),
        "RETURNING must be denied without a SELECT policy: {:?}",
        table.insert_returning
    );

    h.close().await;
}

#[tokio::test]
async fn policy_text_change_with_unchanged_outcomes_appears_under_policies() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key, owner text); \
         INSERT INTO widgets VALUES (1, 'staff-1'); \
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON widgets TO authenticated; \
         CREATE POLICY widgets_select ON widgets FOR SELECT TO authenticated USING (true);",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "before.json"])
        .await
        .unwrap();

    h.db.load_fixture(
        "DROP POLICY widgets_select ON widgets; \
         CREATE POLICY widgets_select ON widgets FOR SELECT TO authenticated USING (owner = owner);",
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "after.json"])
        .await
        .unwrap();

    let before: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("before.json")).unwrap()).unwrap();
    let after: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("after.json")).unwrap()).unwrap();

    // Both policies always evaluate true, so no persona's outcome changes.
    assert_eq!(before.privileges, after.privileges);

    let d = rlsnap::diff::diff(&before, &after).unwrap();
    assert!(d.privilege_changes.is_empty());
    assert_eq!(d.policy_changes.len(), 1);
    assert_eq!(d.policy_changes[0].field, "qual");

    h.close().await;
}

#[tokio::test]
async fn function_newly_executable_by_anon() {
    let h = TestHarness::new(
        "CREATE FUNCTION visible_ids() RETURNS setof int AS $$ SELECT 1 $$ LANGUAGE sql; \
         REVOKE ALL ON FUNCTION visible_ids() FROM public;",
        "schemas = [\"public\"]\n",
        "behavioural",
        1000,
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "before.json"])
        .await
        .unwrap();

    h.db.load_fixture("GRANT EXECUTE ON FUNCTION visible_ids() TO anon;")
        .await;

    h.run(&["snapshot", "--target", "test", "--out", "after.json"])
        .await
        .unwrap();

    let before: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("before.json")).unwrap()).unwrap();
    let after: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("after.json")).unwrap()).unwrap();
    let d = rlsnap::diff::diff(&before, &after).unwrap();

    assert!(d.function_changes.iter().any(|c| c.persona == "anon"
        && c.before == Some(pgcore::Outcome::DeniedPrivilege)
        && c.after == Some(pgcore::Outcome::Allowed)));

    let table = rlsnap::diff::render_table(&d);
    assert!(table.contains("visible_ids"));
    assert!(table.contains("allowed"));

    h.close().await;
}

#[tokio::test]
async fn table_privilege_revoked_flips_all_cells_for_that_persona_to_denied_privilege() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key, name text); \
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT, UPDATE, DELETE ON widgets TO authenticated; \
         CREATE POLICY widgets_all ON widgets FOR ALL TO authenticated USING (true) WITH CHECK (true);",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "before.json"])
        .await
        .unwrap();

    // Table privilege is checked before RLS: revoking it must make the
    // table invisible outright, not merely RLS-denied.
    h.db.load_fixture("REVOKE ALL ON widgets FROM authenticated;")
        .await;

    h.run(&["snapshot", "--target", "test", "--out", "after.json"])
        .await
        .unwrap();

    let after: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("after.json")).unwrap()).unwrap();
    let table = after
        .privileges
        .get("staff")
        .unwrap()
        .get("public.widgets")
        .unwrap();
    assert_eq!(table.select_count, Some(pgcore::Outcome::DeniedPrivilege));
    assert_eq!(table.delete, Some(pgcore::Outcome::DeniedPrivilege));
    for cp in table.columns.values() {
        assert_eq!(cp.select, Some(pgcore::Outcome::DeniedPrivilege));
        assert_eq!(cp.update, Some(pgcore::Outcome::DeniedPrivilege));
    }

    h.close().await;
}

#[tokio::test]
async fn cap_warning_fires_when_persona_sees_more_than_max_rows() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); \
         INSERT INTO widgets SELECT generate_series(1, 10); \
         GRANT SELECT ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        5,
    )
    .await;

    h.run(&[
        "snapshot",
        "--target",
        "test",
        "--out",
        "snap.json",
        "--with-rows",
    ])
    .await
    .unwrap();
    let snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("snap.json")).unwrap()).unwrap();

    assert_eq!(
        snap.data.as_ref().unwrap().row_counts["staff"]["public.widgets"],
        10
    );
    assert!(
        snap.findings
            .iter()
            .any(|f| f.name == "cap:staff:public.widgets"),
        "expected a cap finding, got: {:?}",
        snap.findings
    );

    h.close().await;
}

#[tokio::test]
async fn tenant_disjointness_assert_fails_after_a_bad_policy_change() {
    let personas = r#"
        [[personas]]
        name = "tenant_a"
        role = "authenticated"
        claims = { tenant_id = "a" }

        [[personas]]
        name = "tenant_b"
        role = "authenticated"
        claims = { tenant_id = "b" }
    "#;
    let asserts = r#"
        [[asserts]]
        name = "tenant_disjointness"
        sql = "SELECT id FROM widgets WHERE tenant_id <> current_setting('request.jwt.claim.tenant_id', true)"
        per_persona = true
    "#;

    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key, tenant_id text not null); \
         INSERT INTO widgets VALUES (1, 'a'), (2, 'b'); \
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON widgets TO authenticated, anon, service_role; \
         CREATE POLICY widgets_tenant ON widgets FOR SELECT TO authenticated \
            USING (tenant_id = current_setting('request.jwt.claim.tenant_id', true));",
        &format!("schemas = [\"public\"]\n{personas}\n{asserts}"),
        "behavioural",
        1000,
    )
    .await;

    h.run(&[
        "snapshot",
        "--target",
        "test",
        "--out",
        "before.json",
        "--with-rows",
    ])
    .await
    .unwrap();
    let before: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("before.json")).unwrap()).unwrap();
    assert!(
        before.findings.is_empty(),
        "no violations expected before the bad migration: {:?}",
        before.findings
    );

    // A migration widens the policy so every tenant sees every row.
    h.db.load_fixture(
        "DROP POLICY widgets_tenant ON widgets; \
         CREATE POLICY widgets_tenant ON widgets FOR SELECT TO authenticated USING (true);",
    )
    .await;

    h.run(&[
        "snapshot",
        "--target",
        "test",
        "--out",
        "after.json",
        "--with-rows",
    ])
    .await
    .unwrap();
    let after: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("after.json")).unwrap()).unwrap();
    assert!(
        after
            .findings
            .iter()
            .any(|f| f.name.starts_with("assert:tenant_disjointness")),
        "expected a tenant_disjointness violation, got: {:?}",
        after.findings
    );

    let d = rlsnap::diff::diff(&before, &after).unwrap();
    assert!(!d.finding_changes.is_empty());
    assert_eq!(d.exit_code(false), 1);
    let table = rlsnap::diff::render_table(&d);
    assert!(table.contains("tenant_disjointness"));

    h.close().await;
}

#[tokio::test]
async fn check_then_accept_round_trips_to_zero_exit_code() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); GRANT SELECT ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    // No baseline yet: check must fail loudly, not silently pass.
    let err = h.run(&["check", "--target", "test"]).await;
    assert!(err.is_err());

    let code = h.run(&["accept", "--target", "test"]).await.unwrap();
    assert_eq!(code, 0);
    assert!(h.path("rlsnap.snap.json").is_file());

    let code = h.run(&["check", "--target", "test"]).await.unwrap();
    assert_eq!(code, 0, "check right after accept must report no changes");

    // Widen a grant; check must now report a change with exit code 1.
    h.db.load_fixture("GRANT UPDATE ON widgets TO authenticated;")
        .await;
    let code = h.run(&["check", "--target", "test"]).await.unwrap();
    assert_eq!(code, 1);

    h.close().await;
}

#[tokio::test]
async fn catalog_mode_target_rejects_with_rows() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key);",
        "schemas = [\"public\"]\n",
        "catalog",
        1000,
    )
    .await;

    let result = h
        .run(&[
            "snapshot",
            "--target",
            "test",
            "--out",
            "snap.json",
            "--with-rows",
        ])
        .await;
    assert!(result.is_err(), "catalog targets must reject --with-rows");

    h.close().await;
}

#[tokio::test]
async fn rls_disabled_but_policies_kept_is_flagged_in_catalog_mode() {
    let h = TestHarness::new(
        "CREATE TABLE orders (id int primary key, tenant_id text not null); \
         ALTER TABLE orders ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON orders TO authenticated; \
         CREATE POLICY orders_tenant ON orders FOR SELECT TO authenticated \
            USING (tenant_id = current_setting('request.jwt.claim.tenant_id', true));",
        "schemas = [\"public\"]\n",
        "catalog",
        1000,
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "before.json"])
        .await
        .unwrap();

    // The policy is left completely untouched, but RLS itself is switched
    // off: every row is now visible regardless of the policy's USING
    // clause, and `pg_policies` alone cannot tell you that happened.
    h.db.load_fixture("ALTER TABLE orders DISABLE ROW LEVEL SECURITY;")
        .await;

    h.run(&["snapshot", "--target", "test", "--out", "after.json"])
        .await
        .unwrap();

    let code = h
        .run(&[
            "diff",
            h.path("before.json").to_str().unwrap(),
            h.path("after.json").to_str().unwrap(),
        ])
        .await
        .unwrap();
    assert_eq!(
        code, 1,
        "disabling RLS while keeping the policy in place must be a reported diff, not silence"
    );

    let before: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("before.json")).unwrap()).unwrap();
    let after: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("after.json")).unwrap()).unwrap();

    assert!(before.policies["public.orders"].rls_enabled);
    assert!(!after.policies["public.orders"].rls_enabled);
    assert!(
        after.policies["public.orders"]
            .policies
            .contains_key("orders_tenant"),
        "the policy itself must still be present: DISABLE ROW LEVEL SECURITY never touches it"
    );

    let d = rlsnap::diff::diff(&before, &after).unwrap();
    let table = rlsnap::diff::render_table(&d);
    assert!(
        table.contains("public.orders"),
        "diff should name the table: {table}"
    );
    assert!(
        table.contains("rls_enabled"),
        "diff should name the flag that changed: {table}"
    );

    h.close().await;
}

#[tokio::test]
async fn function_body_change_is_named_in_the_diff() {
    let h = TestHarness::new(
        "CREATE FUNCTION is_admin() RETURNS boolean AS $$ SELECT true $$ LANGUAGE sql;",
        "schemas = [\"public\"]\n",
        "catalog",
        1000,
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "before.json"])
        .await
        .unwrap();

    // Same signature, same grants: only the body (restrictive -> permissive)
    // changes, which no persona's EXECUTE outcome would ever catch.
    h.db.load_fixture(
        "CREATE OR REPLACE FUNCTION is_admin() RETURNS boolean AS $$ SELECT false $$ LANGUAGE sql;",
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "after.json"])
        .await
        .unwrap();

    let code = h
        .run(&[
            "diff",
            h.path("before.json").to_str().unwrap(),
            h.path("after.json").to_str().unwrap(),
        ])
        .await
        .unwrap();
    assert_eq!(code, 1, "a changed function body must be a reported diff");

    let before: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("before.json")).unwrap()).unwrap();
    let after: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("after.json")).unwrap()).unwrap();

    let d = rlsnap::diff::diff(&before, &after).unwrap();
    assert!(
        d.function_def_changes
            .iter()
            .any(|c| c.function.contains("is_admin") && c.field == "definition"),
        "diff should record the changed function definition: {:?}",
        d.function_def_changes
    );

    let table = rlsnap::diff::render_table(&d);
    assert!(
        table.contains("is_admin"),
        "rendered diff should name the function: {table}"
    );

    h.close().await;
}

#[tokio::test]
async fn diff_rejects_a_catalog_snapshot_against_a_behavioural_snapshot_of_the_same_db() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key); GRANT SELECT ON widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    // A second target, same database, catalog mode -- the exact shape of
    // `accept --target local` (behavioural) followed by `check --target
    // prod` (catalog) sharing one snapshot_path.
    let extra = format!(
        "\n[targets.cat]\nurl_env = \"{}\"\nmode = \"catalog\"\nmax_rows = 1000\n",
        h.url_env_name
    );
    let mut toml = std::fs::read_to_string(h.path("rlsnap.toml")).unwrap();
    toml.push_str(&extra);
    std::fs::write(h.path("rlsnap.toml"), toml).unwrap();

    h.run(&["snapshot", "--target", "test", "--out", "beh.json"])
        .await
        .unwrap();
    h.run(&["snapshot", "--target", "cat", "--out", "cat.json"])
        .await
        .unwrap();

    let result = h
        .run(&[
            "diff",
            h.path("beh.json").to_str().unwrap(),
            h.path("cat.json").to_str().unwrap(),
        ])
        .await;
    assert!(
        result.is_err(),
        "diffing a behavioural snapshot against a catalog snapshot of the same schema must be \
         rejected, not silently produce a wall of privilege changes"
    );

    h.close().await;
}
