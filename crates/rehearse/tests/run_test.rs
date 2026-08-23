//! Integration tests for `rehearse run`, driven through the built binary.

mod support;

use support::{run_bin, unique_env_var_name, write_config, Scratch, TestDb};

#[tokio::test]
async fn additive_migration_reports_changes_and_rolls_back() {
    let db = TestDb::create().await;
    // A pre-existing table, so the migration also exercises ADD COLUMN on a
    // table that already exists (not just objects the migration itself
    // creates from scratch).
    db.load_fixture("CREATE TABLE tenants (id int PRIMARY KEY, name text);")
        .await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "CREATE TABLE widgets (id int PRIMARY KEY, name text);\n\
         CREATE INDEX widgets_name_idx ON widgets (name);\n\
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY;\n\
         CREATE POLICY widgets_read ON widgets FOR SELECT USING (true);\n\
         ALTER TABLE tenants ADD COLUMN region text;\n",
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
    assert!(
        added
            .keys()
            .any(|k| k.contains("widgets.indexes.widgets_name_idx")),
        "expected the new index named in the diff, got {added:?}"
    );
    assert!(
        added
            .keys()
            .any(|k| k.contains("widgets.policies.widgets_read")),
        "expected the new policy named in the diff, got {added:?}"
    );
    assert!(
        added.keys().any(|k| k.contains("tenants.columns.region")),
        "expected the new column on the pre-existing table named in the diff, got {added:?}"
    );

    // The transaction was rolled back: neither the table nor the added
    // column exist for real.
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
    let region_count: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_name = 'tenants' AND column_name = 'region'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        region_count, 0,
        "region column should not exist after rollback"
    );

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

    // The migration failed on statement 2, but statements 0 and 1 (creating
    // alpha and beta) succeeded and are still visible: the transaction was
    // recovered via a savepoint rollback rather than left aborted, so
    // locks/writes/schema_changes reflect state as of the last successful
    // statement instead of being dropped entirely.
    assert!(
        !json["schema_changes"].is_null(),
        "schema changes should still be computed using the state as of the last successful \
         statement, even though the migration failed later"
    );
    let added = json["schema_changes"]["added"]
        .as_object()
        .expect("added object");
    assert!(added.keys().any(|k| k.starts_with("tables.public.alpha")));
    assert!(added.keys().any(|k| k.starts_with("tables.public.beta")));

    let locks = json["locks"].as_array().expect("locks array");
    assert!(
        locks.iter().any(|l| l["relation"] == "alpha"),
        "expected the lock from the successful CREATE TABLE alpha, got {locks:?}"
    );
    assert!(
        locks.iter().any(|l| l["relation"] == "beta"),
        "expected the lock from the successful CREATE TABLE beta, got {locks:?}"
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

    // Compare the raw serialized text (with only the "timing" sub-object's
    // text excised), not parsed `serde_json::Value`s: `Value::Object`'s
    // `PartialEq` is order-insensitive, so a regression that made some map
    // (e.g. a `BTreeMap` swapped for a `HashMap`) serialize in a
    // process-random order would still pass a `Value` comparison. A raw
    // string comparison actually catches that, which is the property this
    // test is named for.
    let first_stripped = strip_timing(&first.stdout);
    let second_stripped = strip_timing(&second.stdout);

    assert_eq!(
        first_stripped, second_stripped,
        "report must be byte-identical excluding the timing sub-object"
    );

    db.close().await;
}

/// Remove the `"timing": { ... }` span (key and value) from pretty-printed
/// JSON text, by brace-counting from the opening `{` rather than parsing,
/// so the comparison in the test above stays a comparison of raw text.
fn strip_timing(json_text: &str) -> String {
    let key_start = json_text.find("\"timing\"").expect("timing key present");
    let brace_start = json_text[key_start..]
        .find('{')
        .map(|i| key_start + i)
        .expect("timing value is an object");
    let mut depth = 0i32;
    let mut value_end = brace_start;
    for (i, c) in json_text[brace_start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    value_end = brace_start + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    format!("{}{}", &json_text[..key_start], &json_text[value_end..])
}

#[tokio::test]
async fn commit_in_migration_is_refused_and_nothing_reaches_postgres() {
    let db = TestDb::create().await;
    db.load_fixture(
        "CREATE TABLE widgets (id int, qty int);\n\
         INSERT INTO widgets VALUES (1, 1), (2, 1);\n",
    )
    .await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "ALTER TABLE widgets ADD COLUMN sneaky text;\n\
         COMMIT;\n\
         UPDATE widgets SET qty = 99;\n",
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

    let statements = json["statements"].as_array().expect("statements array");
    assert_eq!(
        statements.len(),
        2,
        "the UPDATE after COMMIT must never run: {statements:?}"
    );
    assert_eq!(statements[0]["status"], "ok");
    assert_eq!(statements[1]["status"], "error");

    // Out-of-band, independent of rehearse's own report: prove COMMIT never
    // reached Postgres by checking the real database directly.
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
        "the ALTER TABLE before COMMIT must have been rolled back"
    );
    let rows = client
        .query("SELECT qty FROM widgets ORDER BY id", &[])
        .await
        .unwrap();
    let qtys: Vec<i32> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(
        qtys,
        vec![1, 1],
        "the UPDATE after COMMIT must never have run against the real database"
    );

    db.close().await;
}

#[tokio::test]
async fn create_index_concurrently_is_refused_and_prior_statement_still_rolls_back() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int, name text);")
        .await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "ALTER TABLE widgets ADD COLUMN sneaky text;\n\
         CREATE INDEX CONCURRENTLY widgets_name_idx ON widgets (name);\n",
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
    let message = json["failure"]["message"].as_str().unwrap();
    assert!(
        message.contains("cannot run inside a rehearsal transaction"),
        "failure message should explain why: {message}"
    );
    assert!(
        json["failure"]["snippet"]
            .as_str()
            .unwrap()
            .contains("CONCURRENTLY"),
        "report should name the refused statement: {json}"
    );

    let statements = json["statements"].as_array().expect("statements array");
    assert_eq!(
        statements.len(),
        2,
        "CREATE INDEX CONCURRENTLY must never be sent: {statements:?}"
    );
    assert_eq!(statements[0]["status"], "ok");
    assert_eq!(statements[1]["status"], "error");

    // Out-of-band, independent of rehearse's own report: neither the
    // earlier ALTER TABLE nor the index ever reached the real database.
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
        "the ALTER TABLE before the refused statement must have rolled back"
    );
    let idx_count: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_indexes WHERE indexname = 'widgets_name_idx'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        idx_count, 0,
        "CREATE INDEX CONCURRENTLY must never have reached Postgres"
    );

    db.close().await;
}

#[tokio::test]
async fn plain_create_index_is_still_allowed() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int, name text);")
        .await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "CREATE INDEX widgets_name_idx ON widgets (name);\n",
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

#[tokio::test]
async fn vacuum_in_a_migration_is_refused_before_running() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int);").await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write("migration.sql", "VACUUM widgets;\n");

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
    assert_eq!(json["failure"]["statement_index"], 0);
    assert!(json["failure"]["message"]
        .as_str()
        .unwrap()
        .contains("cannot run inside a rehearsal transaction"));

    db.close().await;
}

#[tokio::test]
async fn cross_schema_lock_is_reported_without_a_matching_schema_flag() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE SCHEMA app; CREATE TABLE app.orders (id int);")
        .await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "ALTER TABLE app.orders ADD COLUMN total numeric;\n",
    );

    // Deliberately no --schema flag: the default is ["public"], but the
    // migration locks a table in the "app" schema.
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
    let orders_lock = locks
        .iter()
        .find(|l| l["schema"] == "app" && l["relation"] == "orders")
        .unwrap_or_else(|| panic!("expected a lock on app.orders, got {locks:?}"));
    assert_eq!(orders_lock["mode"], "AccessExclusiveLock");
    assert_eq!(orders_lock["blocking"], true);

    db.close().await;
}

#[tokio::test]
async fn sequence_and_index_locks_are_excluded_as_noise() {
    let db = TestDb::create().await;
    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "CREATE TABLE tenants (id serial PRIMARY KEY, name text UNIQUE NOT NULL);\n",
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
    assert!(
        locks.iter().any(|l| l["relation"] == "tenants"),
        "expected the table lock to be reported, got {locks:?}"
    );
    assert!(
        !locks.iter().any(|l| {
            let r = l["relation"].as_str().unwrap();
            r.contains("_seq") || r.contains("_pkey") || r.contains("_key")
        }),
        "sequence/index locks are noise and should not be reported, got {locks:?}"
    );

    db.close().await;
}

#[tokio::test]
async fn unknown_schema_flag_is_a_tool_error_not_a_silently_empty_report() {
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
            "--schema",
            "pubic",
        ],
        &[(&url_env, &db.url())],
    );

    assert_eq!(
        result.status, 2,
        "a typo'd --schema must be a tool error, not a silently clean report; \
         stdout: {} stderr: {}",
        result.stdout, result.stderr
    );
    assert!(
        result.stdout.is_empty(),
        "no report should print on a tool error: {}",
        result.stdout
    );
    assert!(
        result.stderr.contains("pubic"),
        "stderr should name the bad schema: {}",
        result.stderr
    );

    db.close().await;
}

#[tokio::test]
async fn missing_migration_file_is_a_tool_error_exit_2() {
    let db = TestDb::create().await;
    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);

    let result = run_bin(
        &[
            "run",
            scratch.path("does_not_exist.sql").to_str().unwrap(),
            "--target",
            "t",
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[(&url_env, &db.url())],
    );

    assert_eq!(result.status, 2, "stderr: {}", result.stderr);
    assert!(result.stdout.is_empty());
    assert!(result.stderr.starts_with("rehearse:"));

    db.close().await;
}

#[tokio::test]
async fn table_format_renders_human_readable_report() {
    let db = TestDb::create().await;
    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "CREATE TABLE widgets (id int PRIMARY KEY);\n",
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
            "table",
        ],
        &[(&url_env, &db.url())],
    );

    assert_eq!(result.status, 0, "stderr: {}", result.stderr);
    assert!(result.stdout.contains("== Summary =="));
    assert!(result.stdout.contains("status: ok"));
    assert!(result.stdout.contains("== Statements =="));
    assert!(result.stdout.contains("== Locks =="));
    assert!(result.stdout.contains("== Writes =="));
    assert!(result.stdout.contains("== Schema changes =="));
    assert!(result.stdout.contains("widgets"));

    db.close().await;
}

#[tokio::test]
async fn catalog_mode_target_refuses_to_run_a_migration() {
    let db = TestDb::create().await;
    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = scratch.write(
        "rlsnap.toml",
        &format!("[targets.t]\nurl_env = \"{url_env}\"\nmode = \"catalog\"\n"),
    );
    let migration = scratch.write("migration.sql", "CREATE TABLE widgets (id int);\n");

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

    assert_eq!(result.status, 2, "stderr: {}", result.stderr);
    assert!(result.stdout.is_empty());
    assert!(
        result.stderr.contains("catalog"),
        "stderr should explain the refusal: {}",
        result.stderr
    );

    // Nothing ran at all: refused before ever connecting to run anything.
    let client = db.connect().await;
    let count: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.tables WHERE table_name = 'widgets'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);

    db.close().await;
}

#[tokio::test]
async fn long_statement_succeeds_when_statement_timeout_not_configured() {
    let db = TestDb::create().await;
    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    // Exceeds pgcore's old 3000ms statement_timeout default; must not be
    // silently reapplied by rehearse.
    let migration = scratch.write("migration.sql", "SELECT pg_sleep(4);\n");

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

#[tokio::test]
async fn multiline_failing_statement_snippet_is_first_200_chars_not_first_line() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int, qty int);")
        .await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "INSERT INTO widgets (id, qty)\nSELECT bogus_column\nFROM a_table_that_does_not_exist;\n",
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
    let snippet = json["failure"]["snippet"].as_str().unwrap();
    assert!(
        snippet.contains("a_table_that_does_not_exist"),
        "failure snippet should include the whole (short, multi-line) statement, \
         not just its first line: {snippet:?}"
    );
    assert!(snippet.contains("SELECT bogus_column"));

    db.close().await;
}

#[tokio::test]
async fn first_statement_failing_still_produces_a_well_formed_report() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int);").await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let migration = scratch.write(
        "migration.sql",
        "ALTER TABLE widgets ADD COLUMN bad_col int NOT NULL DEFAULT 'not-an-int';\n",
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
    assert_eq!(json["failure"]["sqlstate"], "22P02");

    // Even though the very first (and only) statement failed, the report is
    // still fully populated rather than dropping these sections entirely.
    assert!(!json["schema_changes"].is_null());
    assert!(json["schema_changes"]["added"]
        .as_object()
        .unwrap()
        .is_empty());
    assert!(json["locks"].is_array());
    assert!(json["writes"].is_array());

    db.close().await;
}
