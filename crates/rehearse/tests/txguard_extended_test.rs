//! Item C: rehearse refuses ABORT and PREPARE TRANSACTION / COMMIT PREPARED
//! / ROLLBACK PREPARED in a migration file, the same as it already refused
//! COMMIT/END/ROLLBACK -- because `rehearse::migrate` now uses the shared
//! guard (`pgcore::txguard::transaction_control_violation`) extended in
//! item B to cover these forms too. Plain `PREPARE` (a prepared statement)
//! must NOT be refused.

mod support;

use support::{run_bin, unique_env_var_name, write_config, Scratch, TestDb};

#[tokio::test]
async fn abort_in_migration_is_refused_and_nothing_reaches_postgres() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int, qty int); INSERT INTO widgets VALUES (1, 1);")
        .await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "ALTER TABLE widgets ADD COLUMN sneaky text;\nABORT;\n",
    );

    let result = run_bin(
        &[
            "run",
            migration.to_str().unwrap(),
            "--target",
            "t",
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[(&url_env, &db.url())],
    );

    assert_eq!(result.status, 1, "stderr: {}", result.stderr);
    let json: serde_json::Value = serde_json::from_str(&result.stdout).expect("valid json");
    assert_eq!(json["summary"]["ok"], false);
    assert_eq!(json["failure"]["statement_index"], 1);
    assert!(json["failure"]["message"]
        .as_str()
        .unwrap()
        .contains("ABORT"));

    let client = db.connect().await;
    let sneaky_cols: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_name = 'widgets' AND column_name = 'sneaky'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        sneaky_cols, 0,
        "the ALTER TABLE before ABORT must have been rolled back"
    );

    db.close().await;
}

#[tokio::test]
async fn prepare_transaction_in_migration_is_refused_and_nothing_reaches_postgres() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int);").await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "ALTER TABLE widgets ADD COLUMN sneaky text;\nPREPARE TRANSACTION 'rehearse_gtx';\n",
    );

    let result = run_bin(
        &[
            "run",
            migration.to_str().unwrap(),
            "--target",
            "t",
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[(&url_env, &db.url())],
    );

    assert_eq!(result.status, 1, "stderr: {}", result.stderr);
    let json: serde_json::Value = serde_json::from_str(&result.stdout).expect("valid json");
    assert_eq!(json["summary"]["ok"], false);
    assert_eq!(json["failure"]["statement_index"], 1);
    assert!(json["failure"]["message"]
        .as_str()
        .unwrap()
        .to_ascii_uppercase()
        .contains("PREPARE TRANSACTION"));

    let client = db.connect().await;
    let sneaky_cols: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_name = 'widgets' AND column_name = 'sneaky'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        sneaky_cols, 0,
        "the ALTER TABLE before PREPARE TRANSACTION must have been rolled back"
    );

    // No prepared transaction should have been left dangling either.
    let prepared_count: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_prepared_xacts WHERE gid = 'rehearse_gtx'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(prepared_count, 0);

    db.close().await;
}

#[tokio::test]
async fn commit_prepared_in_migration_is_refused() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int);").await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write("migration.sql", "COMMIT PREPARED 'some_gtx';\n");

    let result = run_bin(
        &[
            "run",
            migration.to_str().unwrap(),
            "--target",
            "t",
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[(&url_env, &db.url())],
    );

    assert_eq!(result.status, 1, "stderr: {}", result.stderr);
    let json: serde_json::Value = serde_json::from_str(&result.stdout).expect("valid json");
    assert!(json["failure"]["message"]
        .as_str()
        .unwrap()
        .to_ascii_uppercase()
        .contains("COMMIT PREPARED"));

    db.close().await;
}

#[tokio::test]
async fn rollback_prepared_in_migration_is_refused() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int);").await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write("migration.sql", "ROLLBACK PREPARED 'some_gtx';\n");

    let result = run_bin(
        &[
            "run",
            migration.to_str().unwrap(),
            "--target",
            "t",
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[(&url_env, &db.url())],
    );

    assert_eq!(result.status, 1, "stderr: {}", result.stderr);
    let json: serde_json::Value = serde_json::from_str(&result.stdout).expect("valid json");
    assert!(json["failure"]["message"]
        .as_str()
        .unwrap()
        .to_ascii_uppercase()
        .contains("ROLLBACK PREPARED"));

    db.close().await;
}

#[tokio::test]
async fn plain_prepare_statement_in_migration_is_allowed() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int);").await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "PREPARE widget_by_id (int) AS SELECT id FROM widgets WHERE id = $1;\n",
    );

    let result = run_bin(
        &[
            "run",
            migration.to_str().unwrap(),
            "--target",
            "t",
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[(&url_env, &db.url())],
    );

    assert_eq!(result.status, 0, "stderr: {}", result.stderr);
    let json: serde_json::Value = serde_json::from_str(&result.stdout).expect("valid json");
    assert_eq!(json["summary"]["ok"], true);

    db.close().await;
}
