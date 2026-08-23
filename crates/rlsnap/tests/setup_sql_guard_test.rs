//! Item B: catalog mode must never execute a persona's `setup_sql` (its
//! whole promise is zero DML), and a persona's `setup_sql` running in
//! behavioural mode must be refused if it contains a transaction-control
//! statement (which would otherwise escape the persona's rollback-only
//! transaction entirely).

mod support;

use support::TestHarness;

#[tokio::test]
async fn catalog_target_with_persona_setup_sql_is_a_config_error() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key);",
        r#"
        schemas = ["public"]

        [[personas]]
        name = "staff"
        role = "authenticated"
        claims = { sub = "staff-1" }
        setup_sql = "INSERT INTO widgets VALUES (1)"
        "#,
        "catalog",
        1000,
    )
    .await;

    let result = h
        .run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await;
    assert!(
        result.is_err(),
        "a catalog target with a persona configured with setup_sql must be refused, \
         not silently skip the setup_sql"
    );
    let message = format!("{:#}", result.unwrap_err());
    assert!(
        message.contains("staff"),
        "error must name the offending persona: {message}"
    );
    assert!(
        message.to_ascii_lowercase().contains("setup_sql"),
        "error must name the reason: {message}"
    );

    // Nothing should have been written.
    assert!(!h.path("snap.json").exists());

    h.close().await;
}

#[tokio::test]
async fn commit_in_persona_setup_sql_is_refused_in_behavioural_mode() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key);",
        r#"
        schemas = ["public"]

        [[personas]]
        name = "staff"
        role = "authenticated"
        claims = { sub = "staff-1" }
        setup_sql = "INSERT INTO widgets VALUES (1); COMMIT; INSERT INTO widgets VALUES (2);"
        "#,
        "behavioural",
        1000,
    )
    .await;

    let result = h
        .run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await;
    assert!(
        result.is_err(),
        "setup_sql containing COMMIT must be refused, not executed"
    );
    let message = format!("{:#}", result.unwrap_err());
    assert!(
        message.contains("staff"),
        "error must name the offending persona: {message}"
    );
    assert!(
        message.contains("COMMIT"),
        "error must name the refused statement: {message}"
    );

    // Nothing must have reached the real database: not even the INSERT
    // before the COMMIT, since setup_sql is validated in full before any of
    // it runs.
    let client = h.db.connect().await;
    let count: i64 = client
        .query_one("SELECT count(*) FROM widgets", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        count, 0,
        "no statement from a refused setup_sql may reach the database"
    );

    h.close().await;
}

#[tokio::test]
async fn prepare_transaction_in_persona_setup_sql_is_refused_in_behavioural_mode() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key);",
        r#"
        schemas = ["public"]

        [[personas]]
        name = "staff"
        role = "authenticated"
        claims = { sub = "staff-1" }
        setup_sql = "PREPARE TRANSACTION 'rlsnap_gtx'"
        "#,
        "behavioural",
        1000,
    )
    .await;

    let result = h
        .run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await;
    assert!(
        result.is_err(),
        "setup_sql containing PREPARE TRANSACTION must be refused, not executed"
    );
    let message = format!("{:#}", result.unwrap_err());
    assert!(
        message.contains("staff"),
        "error must name the offending persona: {message}"
    );
    assert!(
        message.to_ascii_uppercase().contains("PREPARE TRANSACTION"),
        "error must name the refused statement: {message}"
    );

    h.close().await;
}

#[tokio::test]
async fn ordinary_persona_setup_sql_still_runs_in_behavioural_mode() {
    // Guards against the guard being overzealous: setup_sql with no
    // transaction-control statement must run exactly as before.
    let h = TestHarness::new(
        "CREATE TABLE audit_log (who text); GRANT SELECT ON audit_log TO authenticated;",
        r#"
        schemas = ["public"]

        [[personas]]
        name = "staff"
        role = "authenticated"
        claims = { sub = "staff-1" }
        setup_sql = "INSERT INTO audit_log VALUES ('staff-setup-ran')"
        "#,
        "behavioural",
        1000,
    )
    .await;

    let code = h
        .run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await
        .unwrap();
    assert_eq!(code, 0);

    h.close().await;
}
