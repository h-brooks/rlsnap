//! Catalog mode is the prod-safe default (spec items 20/38, v1 success
//! criterion #2), yet before this file existed no test ever produced a
//! catalog-mode snapshot successfully or compared it against behavioural
//! mode. These tests exercise the successful path end to end.

mod support;

use support::TestHarness;

const STAFF_PERSONA: &str = r#"
[[personas]]
name = "staff"
role = "authenticated"
claims = { sub = "staff-1" }
"#;

#[tokio::test]
async fn catalog_mode_reports_allowed_for_a_quoted_mixed_case_table() {
    // Casting text to regclass (what has_table_privilege does internally)
    // applies normal identifier folding: an unquoted "public.Widgets" is
    // looked up as "public.widgets". The table ref passed to
    // has_table_privilege/has_column_privilege must be pre-quoted, exactly
    // like the behavioural-mode table ref already is.
    let h = TestHarness::new(
        "CREATE TABLE \"Widgets\" (id int primary key, \"Label\" text); \
         GRANT SELECT ON \"Widgets\" TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "catalog",
        1000,
    )
    .await;

    let code = h
        .run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await
        .unwrap();
    assert_eq!(code, 0);
    let snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("snap.json")).unwrap()).unwrap();

    let table = snap.privileges["staff"]
        .get("public.Widgets")
        .unwrap_or_else(|| {
            panic!(
                "missing table cell, keys were: {:?}",
                snap.privileges["staff"].keys().collect::<Vec<_>>()
            )
        });
    assert_eq!(
        table.select_count,
        Some(pgcore::Outcome::Allowed),
        "a quoted mixed-case table must resolve through has_table_privilege, \
         not fold to lowercase and 42P01: got {:?}",
        table.select_count
    );
    let label = &table.columns["Label"];
    assert_eq!(
        label.select,
        Some(pgcore::Outcome::Allowed),
        "has_column_privilege must resolve the same mixed-case table: got {:?}",
        label.select
    );

    h.close().await;
}

#[tokio::test]
async fn catalog_mode_probe_error_does_not_poison_later_probes_for_the_same_persona() {
    // `"BadCase"` needs quoting for has_function_privilege to resolve it;
    // rlsnap does not currently quote function signatures (a separate,
    // known gap -- not fixed here). That is used purely as a convenient,
    // deterministic way to force a genuine catalog-mode probe error, to
    // prove it cannot poison whatever else is probed afterwards for the
    // same persona. `zzz_good` sorts after it alphabetically, so if the
    // shared bool_check() helper leaked an aborted transaction the way it
    // used to, this would come back as sqlstate 25P02 instead of Allowed.
    let h = TestHarness::new(
        "CREATE FUNCTION \"BadCase\"() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql; \
         GRANT EXECUTE ON FUNCTION \"BadCase\"() TO authenticated; \
         CREATE FUNCTION zzz_good() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql; \
         GRANT EXECUTE ON FUNCTION zzz_good() TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "catalog",
        1000,
    )
    .await;

    let code = h
        .run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await
        .unwrap();
    assert_eq!(code, 0);
    let snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("snap.json")).unwrap()).unwrap();

    let bad = snap.functions["staff"].get("public.BadCase()").cloned();
    assert!(
        matches!(bad, Some(pgcore::Outcome::Error { .. })),
        "expected the mixed-case function lookup to error, got {bad:?}"
    );

    let good = snap.functions["staff"].get("public.zzz_good()").cloned();
    assert_eq!(
        good,
        Some(pgcore::Outcome::Allowed),
        "an earlier probe error must not poison a later probe for the same persona \
         (25P02 leaking through as though it were an access verdict): got {good:?}"
    );

    h.close().await;
}

#[tokio::test]
async fn catalog_mode_agrees_with_behavioural_mode_for_a_simple_grant() {
    let fixture = "CREATE TABLE orders (id int primary key, note text); \
                    GRANT SELECT, UPDATE (note) ON orders TO authenticated;";
    let config_body = format!("schemas = [\"public\"]\n{STAFF_PERSONA}");

    let cat = TestHarness::new(fixture, &config_body, "catalog", 1000).await;
    cat.run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await
        .unwrap();
    let cat_snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(cat.path("snap.json")).unwrap()).unwrap();
    cat.close().await;

    let beh = TestHarness::new(fixture, &config_body, "behavioural", 1000).await;
    beh.run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await
        .unwrap();
    let beh_snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(beh.path("snap.json")).unwrap()).unwrap();
    beh.close().await;

    let cat_table = &cat_snap.privileges["staff"]["public.orders"];
    let beh_table = &beh_snap.privileges["staff"]["public.orders"];

    assert_eq!(cat_table.select_count, Some(pgcore::Outcome::Allowed));
    assert_eq!(cat_table.select_count, beh_table.select_count);

    assert_eq!(cat_table.delete, Some(pgcore::Outcome::DeniedPrivilege));
    assert_eq!(cat_table.delete, beh_table.delete);

    assert_eq!(
        cat_table.columns["note"].update,
        Some(pgcore::Outcome::Allowed)
    );
    assert_eq!(
        cat_table.columns["note"].update,
        beh_table.columns["note"].update
    );

    assert_eq!(
        cat_table.columns["id"].update,
        Some(pgcore::Outcome::DeniedPrivilege)
    );
    assert_eq!(
        cat_table.columns["id"].update,
        beh_table.columns["id"].update
    );
}
