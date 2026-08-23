mod support;

use pgcore::{classify, Outcome, Persona, RollbackTx};
use support::TestDb;

#[tokio::test]
async fn classifies_denied_privilege() {
    let db = TestDb::create().await;
    db.load_fixture(
        "CREATE TABLE secrets (id int primary key, value text); \
         REVOKE ALL ON secrets FROM anon;",
    )
    .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();
    Persona::anon().apply(&tx).await.unwrap();

    let err = tx.query("SELECT * FROM secrets", &[]).await.unwrap_err();
    assert_eq!(classify(&err), Outcome::DeniedPrivilege);

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn classifies_denied_rls() {
    let db = TestDb::create().await;
    db.load_fixture(
        "CREATE TABLE owned (id int primary key, owner text not null); \
         ALTER TABLE owned ENABLE ROW LEVEL SECURITY; \
         GRANT INSERT ON owned TO authenticated; \
         CREATE POLICY owned_insert ON owned FOR INSERT TO authenticated \
            WITH CHECK (owner = current_setting('request.jwt.claims', true)::json->>'sub');",
    )
    .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();
    let persona = Persona::new(
        "user_a",
        "authenticated",
        serde_json::json!({"sub": "user-a"}),
    );
    persona.apply(&tx).await.unwrap();

    // Insert a row claiming an owner that does not match the persona's sub:
    // this must fail the WITH CHECK clause, which Postgres reports as a
    // row-level-security-denied error, not a plain constraint violation.
    let err = tx
        .execute(
            "INSERT INTO owned (id, owner) VALUES (1, 'someone-else')",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(classify(&err), Outcome::DeniedRls);

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn classifies_constraint_violation() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int primary key, name text not null);")
        .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    let err = tx
        .execute("INSERT INTO widgets (id, name) VALUES (1, NULL)", &[])
        .await
        .unwrap_err();
    match classify(&err) {
        Outcome::Constraint { sqlstate } => assert!(sqlstate.starts_with("23")),
        other => panic!("expected Constraint, got {other:?}"),
    }

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn classifies_generic_error() {
    let db = TestDb::create().await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    let err = tx
        .query("SELECT * FROM this_table_does_not_exist", &[])
        .await
        .unwrap_err();
    match classify(&err) {
        Outcome::Error { sqlstate, .. } => assert_eq!(sqlstate, "42P01"),
        other => panic!("expected Error, got {other:?}"),
    }

    tx.finish().await.unwrap();
    db.close().await;
}
