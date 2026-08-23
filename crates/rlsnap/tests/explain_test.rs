//! `rlsnap explain` prints the grants/policies behind one cell.

mod support;

use support::TestHarness;

#[tokio::test]
async fn explain_names_the_policy_and_grant_behind_a_cell() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key, owner text); \
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON widgets TO authenticated; \
         CREATE POLICY widgets_owner_select ON widgets FOR SELECT TO authenticated \
            USING (owner = current_setting('request.jwt.claim.sub', true));",
        "schemas = [\"public\"]\n\n[[personas]]\nname = \"staff\"\nrole = \"authenticated\"\nclaims = { sub = \"staff-1\" }\n",
        "behavioural",
        1000,
    )
    .await;

    let code = h
        .run(&[
            "explain",
            "staff",
            "public.widgets",
            "select",
            "--target",
            "test",
        ])
        .await
        .unwrap();
    assert_eq!(code, 0);

    // The CLI path only prints the output; drive `run_explain` directly (the
    // function's own return value) to assert on what it actually says --
    // the previous version of this test asserted only the exit code, so it
    // never noticed the grant/policy names were wrong or missing.
    let config = rlsnap::config::RlsnapConfig::find_and_load(&h.dir).unwrap();
    let out = rlsnap::explain::run_explain(&config, "test", "staff", "public.widgets", "select")
        .await
        .unwrap();
    assert!(
        out.contains("widgets_owner_select"),
        "expected the policy name in the output, got:\n{out}"
    );
    assert!(
        out.contains("TO authenticated"),
        "expected the table-level grant to authenticated in the output, got:\n{out}"
    );
    assert!(
        out.to_lowercase().contains("outcome: allowed"),
        "expected the cell's own outcome printed alongside the explanation, got:\n{out}"
    );

    h.close().await;
}

#[tokio::test]
async fn explain_resolves_grants_and_policies_via_role_membership() {
    // `app_staff` holds no grants of its own; it inherits `authenticated`'s
    // via role membership, the normal shape for a custom application role
    // in Supabase. explain must name that inherited grant and policy, not
    // report "(none)" just because the literal role string doesn't match.
    let h = TestHarness::new(
        "DO $$ BEGIN CREATE ROLE app_staff NOLOGIN; \
            EXCEPTION WHEN duplicate_object THEN NULL; END $$; \
         GRANT authenticated TO app_staff; \
         CREATE TABLE orders (id int primary key, owner text); \
         ALTER TABLE orders ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON orders TO authenticated; \
         CREATE POLICY orders_owner_select ON orders FOR SELECT TO authenticated \
            USING (owner = current_setting('request.jwt.claim.sub', true));",
        "schemas = [\"public\"]\n\n[[personas]]\nname = \"staff\"\nrole = \"app_staff\"\nclaims = { sub = \"staff-1\" }\n",
        "behavioural",
        1000,
    )
    .await;

    let config = rlsnap::config::RlsnapConfig::find_and_load(&h.dir).unwrap();
    let out = rlsnap::explain::run_explain(&config, "test", "staff", "public.orders", "select")
        .await
        .unwrap();
    assert!(
        out.contains("TO authenticated"),
        "expected the grant inherited via membership in authenticated to be named, got:\n{out}"
    );
    assert!(
        out.contains("orders_owner_select"),
        "expected the policy whose role membership covers app_staff to be named, got:\n{out}"
    );

    h.close().await;
}

#[tokio::test]
async fn explain_rejects_unknown_op() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key);",
        "schemas = [\"public\"]\n",
        "behavioural",
        1000,
    )
    .await;

    let result = h
        .run(&[
            "explain",
            "anon",
            "public.widgets",
            "yolo",
            "--target",
            "test",
        ])
        .await;
    assert!(result.is_err());

    h.close().await;
}
