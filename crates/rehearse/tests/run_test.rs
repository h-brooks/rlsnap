//! Integration tests for `rehearse run`, driven through the built binary.

mod support;

use support::{run_bin, unique_env_var_name, write_config, Scratch, TestDb};

#[tokio::test]
async fn additive_migration_reports_changes_and_rolls_back() {
    let db = TestDb::create().await;
    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "CREATE TABLE widgets (id int PRIMARY KEY, name text);\n\
         CREATE INDEX widgets_name_idx ON widgets (name);\n\
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY;\n\
         CREATE POLICY widgets_read ON widgets FOR SELECT USING (true);\n",
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
    let added = json["schema_changes"]["added"]
        .as_object()
        .expect("added object");
    assert!(
        added.keys().any(|k| k.starts_with("tables.public.widgets")),
        "expected an added widgets entry, got {added:?}"
    );

    // The transaction was rolled back: the table must not exist for real.
    let client = db.connect().await;
    let count: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'widgets'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "widgets table should not exist after rollback");

    db.close().await;
}

#[tokio::test]
async fn dollar_quoted_function_body_splits_and_appears_in_diff() {
    let db = TestDb::create().await;
    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "CREATE FUNCTION widget_count() RETURNS int AS $$ \
         BEGIN \
           PERFORM 1; PERFORM 2; \
           RETURN 1; \
         END; \
         $$ LANGUAGE plpgsql;\n",
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
    assert_eq!(json["summary"]["statement_count"], 1);
    let added = json["schema_changes"]["added"]
        .as_object()
        .expect("added object");
    assert!(
        added
            .keys()
            .any(|k| k.starts_with("functions.public.widget_count")),
        "expected an added function entry, got {added:?}"
    );

    let client = db.connect().await;
    let count: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_proc WHERE proname = 'widget_count'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "function should not exist after rollback");

    db.close().await;
}

#[tokio::test]
async fn add_column_takes_access_exclusive_lock() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int);").await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "ALTER TABLE widgets ADD COLUMN name text;\n",
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
    let locks = json["locks"].as_array().expect("locks array");
    let widgets_lock = locks
        .iter()
        .find(|l| l["relation"] == "widgets")
        .unwrap_or_else(|| panic!("expected a lock on widgets, got {locks:?}"));
    assert_eq!(widgets_lock["mode"], "AccessExclusiveLock");
    assert_eq!(widgets_lock["blocking"], true);

    db.close().await;
}

#[tokio::test]
async fn data_migration_reports_write_counts_and_rolls_back_rows() {
    let db = TestDb::create().await;
    db.load_fixture(
        "CREATE TABLE widgets (id int, qty int);\n\
         INSERT INTO widgets VALUES (1, 1), (2, 1), (3, 1);\n",
    )
    .await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write("migration.sql", "UPDATE widgets SET qty = qty + 1;\n");

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
    let writes = json["writes"].as_array().expect("writes array");
    let widgets_write = writes
        .iter()
        .find(|w| w["relation"] == "widgets")
        .unwrap_or_else(|| panic!("expected a write entry for widgets, got {writes:?}"));
    assert_eq!(widgets_write["n_tup_upd"], 3);

    let client = db.connect().await;
    let rows = client
        .query("SELECT qty FROM widgets ORDER BY id", &[])
        .await
        .unwrap();
    let qtys: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(qtys, vec![1, 1, 1], "rows must be unchanged after rollback");

    db.close().await;
}

#[tokio::test]
async fn failing_statement_is_named_with_sqlstate_and_prior_timings_kept() {
    let db = TestDb::create().await;
    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "CREATE TABLE alpha (id int);\n\
         CREATE TABLE beta (id int);\n\
         SELECT * FROM does_not_exist;\n",
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
    let failure = &json["failure"];
    assert_eq!(failure["statement_index"], 2);
    assert_eq!(failure["sqlstate"], "42P01");

    let statements = json["statements"].as_array().expect("statements array");
    assert_eq!(statements.len(), 3);
    assert_eq!(statements[0]["status"], "ok");
    assert_eq!(statements[1]["status"], "ok");
    assert_eq!(statements[2]["status"], "error");

    let per_statement_ms = json["timing"]["per_statement_ms"]
        .as_array()
        .expect("per_statement_ms array");
    assert_eq!(per_statement_ms.len(), 3);

    assert!(
        json["schema_changes"].is_null(),
        "schema changes should not be computed on failure"
    );

    // Catalog unchanged: neither table exists for real.
    let client = db.connect().await;
    let count: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.tables WHERE table_name IN ('alpha', 'beta')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);

    db.close().await;
}

#[tokio::test]
async fn lock_timeout_is_honoured_and_fails_fast() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int);").await;

    // Hold an AccessExclusive lock from a second connection.
    let blocker = db.connect().await;
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker
        .batch_execute("LOCK TABLE widgets IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, Some(500));
    let migration = scratch.write(
        "migration.sql",
        "ALTER TABLE widgets ADD COLUMN name text;\n",
    );

    let start = std::time::Instant::now();
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
    let elapsed = start.elapsed();

    // Release the blocking lock regardless of the assertions below.
    let _ = blocker.batch_execute("ROLLBACK").await;

    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "rehearsal should fail fast on lock_timeout, took {elapsed:?}"
    );
    assert_eq!(result.status, 1, "stderr: {}", result.stderr);
    let json: serde_json::Value = serde_json::from_str(&result.stdout).expect("valid json");
    assert_eq!(json["failure"]["sqlstate"], "55P03");

    db.close().await;
}

#[tokio::test]
async fn json_report_is_deterministic_excluding_timing() {
    let db = TestDb::create().await;
    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "CREATE TABLE widgets (id int PRIMARY KEY, name text);\n\
         CREATE INDEX widgets_name_idx ON widgets (name);\n",
    );

    let args = [
        "run",
        migration.to_str().unwrap(),
        "--target",
        "t",
        "--config",
        config.to_str().unwrap(),
        "--format",
        "json",
    ];
    let envs = [(url_env.as_str(), db.url())];
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let first = run_bin(&args, &envs);
    let second = run_bin(&args, &envs);
    assert_eq!(first.status, 0);
    assert_eq!(second.status, 0);

    let mut first_json: serde_json::Value = serde_json::from_str(&first.stdout).unwrap();
    let mut second_json: serde_json::Value = serde_json::from_str(&second.stdout).unwrap();
    first_json
        .as_object_mut()
        .unwrap()
        .remove("timing")
        .expect("timing key present");
    second_json
        .as_object_mut()
        .unwrap()
        .remove("timing")
        .expect("timing key present");

    assert_eq!(
        first_json, second_json,
        "report must be byte-identical excluding the timing sub-object"
    );

    db.close().await;
}
