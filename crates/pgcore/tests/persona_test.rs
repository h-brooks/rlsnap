mod support;

use pgcore::{Persona, RollbackTx};
use support::TestDb;

#[tokio::test]
async fn persona_sees_only_own_rows_under_rls() {
    let db = TestDb::create().await;
    db.load_fixture(
        "CREATE TABLE items (id int primary key, owner text not null, label text); \
         INSERT INTO items VALUES (1, 'user-a', 'a-item'); \
         INSERT INTO items VALUES (2, 'user-b', 'b-item'); \
         ALTER TABLE items ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON items TO authenticated; \
         CREATE POLICY items_select ON items FOR SELECT TO authenticated \
            USING (owner = current_setting('request.jwt.claims', true)::json->>'sub');",
    )
    .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();
    let persona_a = Persona::new(
        "user_a",
        "authenticated",
        serde_json::json!({"sub": "user-a"}),
    );
    persona_a.apply(&tx).await.unwrap();

    let rows = tx
        .query("SELECT id, label FROM items ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "persona A should see exactly one row");
    let label: String = rows[0].get(1);
    assert_eq!(label, "a-item");
    tx.finish().await.unwrap();

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();
    let persona_b = Persona::new(
        "user_b",
        "authenticated",
        serde_json::json!({"sub": "user-b"}),
    );
    persona_b.apply(&tx).await.unwrap();

    let rows = tx
        .query("SELECT id, label FROM items ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "persona B should see exactly one row");
    let label: String = rows[0].get(1);
    assert_eq!(label, "b-item");
    tx.finish().await.unwrap();

    db.close().await;
}

#[tokio::test]
async fn built_in_anon_persona_is_blocked_by_rls() {
    let db = TestDb::create().await;
    db.load_fixture(
        "CREATE TABLE items (id int primary key, owner text not null); \
         INSERT INTO items VALUES (1, 'user-a'); \
         ALTER TABLE items ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON items TO authenticated; \
         CREATE POLICY items_select ON items FOR SELECT TO authenticated \
            USING (owner = current_setting('request.jwt.claims', true)::json->>'sub');",
    )
    .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();
    Persona::anon().apply(&tx).await.unwrap();

    // anon has no grant at all on items, so the query is denied outright.
    let err = tx.query("SELECT * FROM items", &[]).await.unwrap_err();
    assert!(err.as_db_error().is_some());

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn persona_setup_sql_runs_before_impersonation() {
    let db = TestDb::create().await;
    db.load_fixture(
        "CREATE TABLE grants_log (who text); \
         GRANT SELECT ON grants_log TO authenticated;",
    )
    .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    let persona = Persona::new("logger", "authenticated", serde_json::json!({}))
        .with_setup_sql("INSERT INTO grants_log VALUES ('setup-ran')");
    persona.apply(&tx).await.unwrap();

    let rows = tx.query("SELECT who FROM grants_log", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    let who: String = rows[0].get(0);
    assert_eq!(who, "setup-ran");

    tx.finish().await.unwrap();
    db.close().await;
}
