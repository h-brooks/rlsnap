mod support;

use pgcore::{catalog, Catalog, RollbackTx};
use support::TestDb;

async fn snapshot_public(db: &TestDb) -> Catalog {
    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();
    let schemas = vec!["public".to_string()];
    let snap = catalog::snapshot(&tx, &schemas).await.unwrap();
    tx.finish().await.unwrap();
    snap
}

#[tokio::test]
async fn snapshot_twice_is_byte_identical() {
    let db = TestDb::create().await;
    db.load_fixture(
        "CREATE TABLE widgets (id int primary key, name text not null, created_at timestamptz default now()); \
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON widgets TO authenticated; \
         CREATE POLICY widgets_select ON widgets FOR SELECT TO authenticated USING (true);",
    )
    .await;

    let a = snapshot_public(&db).await;
    let b = snapshot_public(&db).await;

    let json_a = serde_json::to_string(&a).unwrap();
    let json_b = serde_json::to_string(&b).unwrap();
    assert_eq!(
        json_a, json_b,
        "two snapshots of an unchanged schema must be byte-identical"
    );

    db.close().await;
}

#[tokio::test]
async fn diff_names_added_column_and_policy_exactly() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int primary key);")
        .await;
    let before = snapshot_public(&db).await;

    db.load_fixture(
        "ALTER TABLE widgets ADD COLUMN owner text; \
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON widgets TO authenticated; \
         CREATE POLICY widgets_owner_select ON widgets FOR SELECT TO authenticated \
            USING (owner = current_setting('request.jwt.claims', true)::json->>'sub');",
    )
    .await;
    let after = snapshot_public(&db).await;

    let diff = Catalog::diff(&before, &after);
    assert!(!diff.is_empty());

    let added_paths: Vec<&String> = diff.added.keys().collect();
    assert!(
        added_paths
            .iter()
            .any(|p| p.contains("widgets.columns.owner")),
        "diff should name the new column: {added_paths:?}"
    );
    assert!(
        added_paths
            .iter()
            .any(|p| p.contains("widgets.policies.widgets_owner_select")),
        "diff should name the new policy: {added_paths:?}"
    );

    let changed_paths: Vec<&String> = diff.changed.keys().collect();
    assert!(
        changed_paths
            .iter()
            .any(|p| p.ends_with("widgets.rls_enabled")),
        "diff should flip rls_enabled: {changed_paths:?}"
    );

    db.close().await;
}

#[tokio::test]
async fn diff_of_identical_snapshots_is_empty() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int primary key, name text);")
        .await;

    let a = snapshot_public(&db).await;
    let b = snapshot_public(&db).await;
    assert!(Catalog::diff(&a, &b).is_empty());

    db.close().await;
}
