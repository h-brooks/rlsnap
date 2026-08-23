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
    assert_eq!(classify(&err).unwrap(), Outcome::DeniedPrivilege);

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
    assert_eq!(classify(&err).unwrap(), Outcome::DeniedRls);

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
    match classify(&err).unwrap() {
        Outcome::Constraint { sqlstate } => assert!(sqlstate.starts_with("23")),
        other => panic!("expected Constraint, got {other:?}"),
    }

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn classifies_feature_not_supported_as_a_legitimate_outcome() {
    // `FOR UPDATE` combined with `GROUP BY` is rejected outright by
    // Postgres with SQLSTATE 0A000 (feature not supported): a legitimate
    // classify() outcome, verified directly here even though none of
    // rlsnap's own probes currently issue `FOR UPDATE` themselves.
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int primary key, category text);")
        .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    let err = tx
        .query(
            "SELECT category FROM widgets GROUP BY category FOR UPDATE",
            &[],
        )
        .await
        .unwrap_err();
    match classify(&err).unwrap() {
        Outcome::Error { sqlstate, .. } => assert_eq!(sqlstate, "0A000"),
        other => panic!("expected Error(0A000), got {other:?}"),
    }

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn classifies_identity_column_rejection_as_a_legitimate_outcome() {
    // Explicitly setting a GENERATED ALWAYS AS IDENTITY column is rejected
    // with SQLSTATE 428C9 -- a legitimate probe outcome (this is exactly
    // what rlsnap's own column-update probe hits for an identity primary
    // key), not an infrastructure failure.
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id bigint generated always as identity primary key);")
        .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    let err = tx
        .execute("UPDATE widgets SET id = id", &[])
        .await
        .unwrap_err();
    match classify(&err).unwrap() {
        Outcome::Error { sqlstate, .. } => assert_eq!(sqlstate, "428C9"),
        other => panic!("expected Error(428C9), got {other:?}"),
    }

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn classifies_unresolved_function_as_a_legitimate_outcome() {
    // has_function_privilege on a signature that doesn't resolve (rlsnap
    // does not quote mixed-case function names, a known gap) fails with
    // SQLSTATE 42883 (undefined_function) -- a legitimate probe outcome,
    // not an infrastructure failure: see catalog_mode_test.rs, which relies
    // on this not poisoning the rest of that persona's probes.
    let db = TestDb::create().await;
    db.load_fixture("CREATE FUNCTION \"BadCase\"() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql;")
        .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    let err = tx
        .query(
            "SELECT has_function_privilege($1, 'EXECUTE')",
            &[&"public.BadCase()"],
        )
        .await
        .unwrap_err();
    match classify(&err).unwrap() {
        Outcome::Error { sqlstate, .. } => assert_eq!(sqlstate, "42883"),
        other => panic!("expected Error(42883), got {other:?}"),
    }

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn classifies_undefined_table_as_an_infrastructure_failure() {
    // Not a SQLSTATE any of rlsnap's own probes are known to legitimately
    // produce (probed table names always come from live catalog
    // discovery): treated conservatively as an infrastructure failure, not
    // recorded as baseline content.
    let db = TestDb::create().await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    let err = tx
        .query("SELECT * FROM this_table_does_not_exist", &[])
        .await
        .unwrap_err();
    let infra = classify(&err).unwrap_err();
    assert_eq!(infra.sqlstate, "42P01");

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn classifies_statement_timeout_as_an_infrastructure_failure() {
    let db = TestDb::create().await;

    let client = db.connect().await;
    // A 50ms statement_timeout is comfortably shorter than pg_sleep(1)'s
    // guaranteed 1-second wait, so this is deterministic, not a
    // near-the-boundary race.
    let tx = RollbackTx::begin(client, 50, 5000).await.unwrap();

    let err = tx.query("SELECT pg_sleep(1)", &[]).await.unwrap_err();
    let infra = classify(&err).unwrap_err();
    assert_eq!(infra.sqlstate, "57014");

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn classifies_a_terminated_connection_as_an_infrastructure_failure() {
    let db = TestDb::create().await;
    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    // Terminate the backend out from under the still-open transaction (this
    // fresh test database has exactly one other connection, the admin one
    // below), so the next query on it fails at the connection level.
    // Depending on timing this surfaces either as a raw I/O error with no
    // SQLSTATE at all, or as a clean 57P01 (admin_shutdown) error from
    // Postgres -- classify() must treat both as an infrastructure failure,
    // never as a legitimate probe outcome.
    let admin = db.connect().await;
    admin
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid()",
            &[],
        )
        .await
        .ok();

    let err = tx.query("SELECT 1", &[]).await.unwrap_err();
    classify(&err).unwrap_err();

    db.close().await;
}
