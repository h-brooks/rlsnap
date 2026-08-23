//! Item A: a successful behavioural probe (e.g. `INSERT ... DEFAULT VALUES`)
//! must never stay visible to a later probe, row count, or assert running in
//! the same persona's transaction. Before the fix, the probe's savepoint was
//! `RELEASE`d on success, which keeps its effects inside the still-open
//! outer transaction until the whole persona's run finally rolls back --
//! meaning anything checked *afterwards*, in that same transaction, saw the
//! leaked row.
//!
//! `probe_table` captures a table's `select_count` before running its insert
//! probes, so the leak cannot be observed by re-reading the same table's
//! count within the same `probe_table` call. The only things that run after
//! a table's own probes, in the same persona transaction, are: later
//! tables' probes, and that persona's asserts. These tests use a
//! per-persona assert (evaluated after every table has been probed) as the
//! vehicle to observe whether an earlier successful insert leaked.

mod support;

use support::TestHarness;

const STAFF_PERSONA: &str = r#"
[[personas]]
name = "staff"
role = "authenticated"
claims = { sub = "staff-1" }
"#;

#[tokio::test]
async fn insert_probe_row_does_not_survive_to_a_later_assert_in_the_same_transaction() {
    // Every column defaults, so `INSERT INTO widgets DEFAULT VALUES`
    // genuinely succeeds; the default `owner` matches `staff`'s own claim,
    // so RLS lets it through too. Two pre-existing rows are seeded; the
    // assert fails if the visible count is anything other than 2 -- which
    // is exactly what a leaked, un-rolled-back probe insert would produce.
    let asserts = r#"
        [[asserts]]
        name = "widgets_count_unchanged"
        sql = "SELECT 1 WHERE (SELECT count(*) FROM widgets) <> 2"
        per_persona = true
    "#;
    let h = TestHarness::new(
        "CREATE TABLE widgets ( \
            id bigint generated always as identity primary key, \
            name text not null default 'x', \
            owner text not null default 'staff-1' \
         ); \
         INSERT INTO widgets (name, owner) VALUES ('a', 'staff-1'), ('b', 'staff-1'); \
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT, INSERT ON widgets TO authenticated; \
         CREATE POLICY widgets_all ON widgets FOR ALL TO authenticated \
            USING (owner = current_setting('request.jwt.claim.sub', true)) \
            WITH CHECK (owner = current_setting('request.jwt.claim.sub', true));",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}\n{asserts}"),
        "behavioural",
        1000,
    )
    .await;

    let code = h
        .run(&[
            "snapshot",
            "--target",
            "test",
            "--out",
            "snap.json",
            "--with-rows",
        ])
        .await
        .unwrap();
    assert_eq!(code, 0);

    let snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("snap.json")).unwrap()).unwrap();

    // Guard against a vacuous pass: the insert probe must actually have
    // succeeded for this proof to test anything.
    let table = &snap.privileges["staff"]["public.widgets"];
    assert_eq!(
        table.insert,
        Some(pgcore::Outcome::Allowed),
        "fixture bug: the insert probe must succeed for this proof to test anything"
    );

    // Built-in personas (`anon`, `authenticated`, `service_role`) see a
    // different visible count for unrelated reasons (no matching `sub`
    // claim, so RLS hides every row, or no grant at all) -- only `staff`'s
    // finding is evidence of the leak this test targets.
    assert!(
        !snap
            .findings
            .iter()
            .any(|f| f.name == "assert:widgets_count_unchanged:staff"),
        "the insert probe's row must be rolled back before the persona's assert runs, \
         but the assert fired for staff (implying a leaked row was still visible): {:?}",
        snap.findings
    );

    h.close().await;
}

#[tokio::test]
async fn probe_isolation_holds_regardless_of_table_discovery_order() {
    // Tables are discovered in a deterministic (alphabetical) order. This
    // fixture has one table that sorts first and one that sorts last, each
    // with its own successful insert probe and its own per-table assert, so
    // isolation is proven for the table probed earliest AND the one probed
    // latest in the same persona transaction.
    let asserts = r#"
        [[asserts]]
        name = "aaa_count_unchanged"
        sql = "SELECT 1 WHERE (SELECT count(*) FROM aaa_widgets) <> 1"
        per_persona = true

        [[asserts]]
        name = "zzz_count_unchanged"
        sql = "SELECT 1 WHERE (SELECT count(*) FROM zzz_widgets) <> 1"
        per_persona = true
    "#;
    let fixture = "\
        CREATE TABLE aaa_widgets ( \
            id bigint generated always as identity primary key, \
            owner text not null default 'staff-1' \
        ); \
        CREATE TABLE zzz_widgets ( \
            id bigint generated always as identity primary key, \
            owner text not null default 'staff-1' \
        ); \
        INSERT INTO aaa_widgets DEFAULT VALUES; \
        INSERT INTO zzz_widgets DEFAULT VALUES; \
        ALTER TABLE aaa_widgets ENABLE ROW LEVEL SECURITY; \
        ALTER TABLE zzz_widgets ENABLE ROW LEVEL SECURITY; \
        GRANT SELECT, INSERT ON aaa_widgets, zzz_widgets TO authenticated; \
        CREATE POLICY aaa_all ON aaa_widgets FOR ALL TO authenticated \
            USING (owner = current_setting('request.jwt.claim.sub', true)) \
            WITH CHECK (owner = current_setting('request.jwt.claim.sub', true)); \
        CREATE POLICY zzz_all ON zzz_widgets FOR ALL TO authenticated \
            USING (owner = current_setting('request.jwt.claim.sub', true)) \
            WITH CHECK (owner = current_setting('request.jwt.claim.sub', true));";

    let h = TestHarness::new(
        fixture,
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}\n{asserts}"),
        "behavioural",
        1000,
    )
    .await;

    let code = h
        .run(&[
            "snapshot",
            "--target",
            "test",
            "--out",
            "snap.json",
            "--with-rows",
        ])
        .await
        .unwrap();
    assert_eq!(code, 0);

    let snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("snap.json")).unwrap()).unwrap();

    assert_eq!(
        snap.privileges["staff"]["public.aaa_widgets"].insert,
        Some(pgcore::Outcome::Allowed),
        "fixture bug: insert probe on the first-discovered table must succeed"
    );
    assert_eq!(
        snap.privileges["staff"]["public.zzz_widgets"].insert,
        Some(pgcore::Outcome::Allowed),
        "fixture bug: insert probe on the last-discovered table must succeed"
    );

    // See the comment in the sibling test above about why only `staff`'s
    // findings are checked: built-in personas see different (unrelated)
    // visible counts for their own reasons.
    assert!(
        !snap
            .findings
            .iter()
            .any(|f| f.name == "assert:aaa_count_unchanged:staff"
                || f.name == "assert:zzz_count_unchanged:staff"),
        "neither table's insert probe may leak into staff's asserts, \
         regardless of discovery order: {:?}",
        snap.findings
    );

    h.close().await;
}

#[tokio::test]
async fn same_fixture_with_pool_size_one_and_four_is_byte_identical_with_multiple_personas() {
    // Two personas, each with their own successful insert probe, so a
    // cross-persona interaction (if the isolation fix were somehow
    // concurrency-dependent) would have a chance to show up as a
    // pool-size-dependent difference.
    let personas = r#"
        [[personas]]
        name = "staff_a"
        role = "authenticated"
        claims = { sub = "staff-1" }

        [[personas]]
        name = "staff_b"
        role = "authenticated"
        claims = { sub = "staff-2" }
    "#;
    let fixture = "\
        CREATE TABLE widgets ( \
            id bigint generated always as identity primary key, \
            owner text not null default 'staff-1' \
        ); \
        ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
        GRANT SELECT, INSERT ON widgets TO authenticated; \
        CREATE POLICY widgets_all ON widgets FOR ALL TO authenticated \
            USING (owner = current_setting('request.jwt.claim.sub', true)) \
            WITH CHECK (owner = current_setting('request.jwt.claim.sub', true));";

    let h = TestHarness::new(
        fixture,
        &format!("pool_size = 1\nschemas = [\"public\"]\n{personas}"),
        "behavioural",
        1000,
    )
    .await;

    h.run(&[
        "snapshot",
        "--target",
        "test",
        "--out",
        "pool1.json",
        "--with-rows",
    ])
    .await
    .unwrap();
    let pool1 = std::fs::read_to_string(h.path("pool1.json")).unwrap();

    let config_text = std::fs::read_to_string(h.path("rlsnap.toml")).unwrap();
    std::fs::write(
        h.path("rlsnap.toml"),
        config_text.replace("pool_size = 1", "pool_size = 4"),
    )
    .unwrap();

    h.run(&[
        "snapshot",
        "--target",
        "test",
        "--out",
        "pool4.json",
        "--with-rows",
    ])
    .await
    .unwrap();
    let pool4 = std::fs::read_to_string(h.path("pool4.json")).unwrap();

    assert_eq!(
        pool1, pool4,
        "concurrency (pool size) must never affect snapshot output, even when multiple \
         personas each run a successful insert probe"
    );

    h.close().await;
}
