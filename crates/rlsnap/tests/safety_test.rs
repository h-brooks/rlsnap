//! The two safety proofs the spec calls for: a full behavioural snapshot
//! run leaves the database byte-identical (as measured by per-table row
//! counts and content hashes), and running the same snapshot with
//! different pool sizes produces byte-identical output.

mod support;

use support::TestHarness;

// Every column has a default (an identity `id`, and defaults on the two
// NOT NULL columns), so `INSERT INTO widgets DEFAULT VALUES` genuinely
// succeeds -- and the default `owner` matches the `staff` persona's own
// claim, so INSERT...RETURNING succeeds too. That is deliberate: a table
// where the insert probe can only ever fail on a constraint before writing
// anything would make the no-mutation proof below pass unconditionally,
// even if ROLLBACK were replaced with COMMIT.
const FIXTURE_SQL: &str = "\
    CREATE TABLE widgets ( \
        id bigint generated always as identity primary key, \
        name text not null default 'x', \
        owner text not null default 'staff-1' \
    ); \
    INSERT INTO widgets (name, owner) VALUES ('a', 'staff-1'), ('b', 'staff-1'); \
    ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
    GRANT SELECT, INSERT, UPDATE, DELETE ON widgets TO authenticated; \
    CREATE POLICY widgets_all ON widgets FOR ALL TO authenticated \
        USING (owner = current_setting('request.jwt.claim.sub', true)) \
        WITH CHECK (owner = current_setting('request.jwt.claim.sub', true)); \
    CREATE FUNCTION widgets_count() RETURNS bigint AS $$ SELECT count(*) FROM widgets $$ \
        LANGUAGE sql SECURITY DEFINER;";

const PERSONAS: &str = r#"
[[personas]]
name = "staff"
role = "authenticated"
claims = { sub = "staff-1" }
"#;

async fn fingerprint(h: &TestHarness) -> (i64, Option<String>) {
    let client = h.db.connect().await;
    let count: i64 = client
        .query_one("SELECT count(*) FROM widgets", &[])
        .await
        .unwrap()
        .get(0);
    let hash: Option<String> = client
        .query_one(
            "SELECT md5(string_agg(t::text, ',' ORDER BY t::text)) FROM widgets t",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    (count, hash)
}

#[tokio::test]
async fn full_behavioural_snapshot_leaves_the_database_byte_identical() {
    let h = TestHarness::new(
        FIXTURE_SQL,
        &format!("schemas = [\"public\"]\n{PERSONAS}"),
        "behavioural",
        1000,
    )
    .await;

    let before = fingerprint(&h).await;

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

    // The proof below (row count and content hash unchanged) is only
    // meaningful if a real insert actually happened and was rolled back --
    // confirm that directly, so this test cannot pass vacuously.
    let snap: rlsnap::snapshot::Snapshot =
        serde_json::from_str(&std::fs::read_to_string(h.path("snap.json")).unwrap()).unwrap();
    let table = &snap.privileges["staff"]["public.widgets"];
    assert_eq!(
        table.insert,
        Some(pgcore::Outcome::Allowed),
        "fixture bug: the insert probe must succeed for this proof to test anything"
    );
    assert_eq!(
        table.insert_returning,
        Some(pgcore::Outcome::Allowed),
        "fixture bug: insert...returning must succeed for this proof to test anything"
    );

    let after = fingerprint(&h).await;
    assert_eq!(
        before, after,
        "a full behavioural snapshot (SELECT/INSERT/UPDATE/DELETE probes on every persona) must never mutate the database"
    );

    h.close().await;
}

#[tokio::test]
async fn same_snapshot_with_different_pool_sizes_is_byte_identical() {
    let h = TestHarness::new(
        FIXTURE_SQL,
        &format!("pool_size = 1\nschemas = [\"public\"]\n{PERSONAS}"),
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

    // Rewrite the same config with a wider pool and re-snapshot the same
    // (unchanged) database.
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
        "concurrency (pool size) must never affect snapshot output"
    );

    h.close().await;
}

#[tokio::test]
async fn same_database_snapshotted_twice_is_byte_identical() {
    let h = TestHarness::new(
        FIXTURE_SQL,
        &format!("schemas = [\"public\"]\n{PERSONAS}"),
        "behavioural",
        1000,
    )
    .await;

    h.run(&["snapshot", "--target", "test", "--out", "a.json"])
        .await
        .unwrap();
    h.run(&["snapshot", "--target", "test", "--out", "b.json"])
        .await
        .unwrap();

    let a = std::fs::read_to_string(h.path("a.json")).unwrap();
    let b = std::fs::read_to_string(h.path("b.json")).unwrap();
    assert_eq!(a, b);

    h.close().await;
}
