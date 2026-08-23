//! Integration tests for `rehearse drift`.

mod support;

use support::{run_bin, unique_env_var_name, Scratch, TestDb};

#[tokio::test]
async fn drift_names_objects_that_differ() {
    let db_a = TestDb::create().await;
    let db_b = TestDb::create().await;
    let fixture = "CREATE TABLE widgets (id int PRIMARY KEY);";
    db_a.load_fixture(fixture).await;
    db_b.load_fixture(fixture).await;
    db_b.load_fixture(
        "ALTER TABLE widgets ADD COLUMN extra text;\n\
             ALTER TABLE widgets ENABLE ROW LEVEL SECURITY;\n\
             CREATE POLICY widgets_read ON widgets FOR SELECT USING (true);\n",
    )
    .await;

    let scratch = Scratch::new();
    let url_env_a = unique_env_var_name();
    let url_env_b = unique_env_var_name();
    let config = scratch.write(
        "rlsnap.toml",
        &format!(
            "[targets.a]\nurl_env = \"{url_env_a}\"\nmode = \"catalog\"\n\n\
             [targets.b]\nurl_env = \"{url_env_b}\"\nmode = \"catalog\"\n"
        ),
    );

    let result = run_bin(
        &[
            "drift",
            "target:a",
            "target:b",
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[(&url_env_a, &db_a.url()), (&url_env_b, &db_b.url())],
    );

    assert_eq!(result.status, 1, "stderr: {}", result.stderr);
    let json: serde_json::Value = serde_json::from_str(&result.stdout).expect("valid json");
    let added = json["added"].as_object().expect("added object");
    assert!(
        added
            .keys()
            .any(|k| k.contains("widgets") && k.contains("extra")),
        "expected the extra column named in the diff, got {added:?}"
    );
    assert!(
        added.keys().any(|k| k.contains("widgets_read")),
        "expected the new policy named in the diff, got {added:?}"
    );

    db_a.close().await;
    db_b.close().await;
}

#[tokio::test]
async fn drift_of_identical_databases_is_empty_and_exits_zero() {
    let db_a = TestDb::create().await;
    let db_b = TestDb::create().await;
    let fixture = "CREATE TABLE widgets (id int PRIMARY KEY, name text);";
    db_a.load_fixture(fixture).await;
    db_b.load_fixture(fixture).await;

    let scratch = Scratch::new();
    let url_env_a = unique_env_var_name();
    let url_env_b = unique_env_var_name();
    let config = scratch.write(
        "rlsnap.toml",
        &format!(
            "[targets.a]\nurl_env = \"{url_env_a}\"\nmode = \"catalog\"\n\n\
             [targets.b]\nurl_env = \"{url_env_b}\"\nmode = \"catalog\"\n"
        ),
    );

    let result = run_bin(
        &[
            "drift",
            "target:a",
            "target:b",
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[(&url_env_a, &db_a.url()), (&url_env_b, &db_b.url())],
    );

    assert_eq!(result.status, 0, "stderr: {}", result.stderr);
    let json: serde_json::Value = serde_json::from_str(&result.stdout).expect("valid json");
    assert!(json["added"].as_object().unwrap().is_empty());
    assert!(json["removed"].as_object().unwrap().is_empty());
    assert!(json["changed"].as_object().unwrap().is_empty());

    db_a.close().await;
    db_b.close().await;
}

#[tokio::test]
async fn drift_can_compare_a_saved_snapshot_file_against_a_live_target() {
    let db_a = TestDb::create().await;
    let db_b = TestDb::create().await;
    let fixture = "CREATE TABLE widgets (id int PRIMARY KEY);";
    db_a.load_fixture(fixture).await;
    db_b.load_fixture(fixture).await;
    db_b.load_fixture("ALTER TABLE widgets ADD COLUMN extra text;")
        .await;

    let scratch = Scratch::new();
    let url_env_a = unique_env_var_name();
    let url_env_b = unique_env_var_name();
    let config = scratch.write(
        "rlsnap.toml",
        &format!(
            "[targets.a]\nurl_env = \"{url_env_a}\"\nmode = \"catalog\"\n\n\
             [targets.b]\nurl_env = \"{url_env_b}\"\nmode = \"catalog\"\n"
        ),
    );
    let snapshot_path = scratch.path("a.json");

    let schema_result = run_bin(
        &[
            "schema",
            "--target",
            "a",
            "--out",
            snapshot_path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ],
        &[(&url_env_a, &db_a.url())],
    );
    assert_eq!(schema_result.status, 0, "stderr: {}", schema_result.stderr);

    let drift_result = run_bin(
        &[
            "drift",
            snapshot_path.to_str().unwrap(),
            "target:b",
            "--config",
            config.to_str().unwrap(),
            "--format",
            "json",
        ],
        &[(&url_env_a, &db_a.url()), (&url_env_b, &db_b.url())],
    );

    assert_eq!(drift_result.status, 1, "stderr: {}", drift_result.stderr);
    let json: serde_json::Value = serde_json::from_str(&drift_result.stdout).expect("valid json");
    let added = json["added"].as_object().expect("added object");
    assert!(
        added
            .keys()
            .any(|k| k.contains("widgets") && k.contains("extra")),
        "expected the extra column named in the diff, got {added:?}"
    );

    db_a.close().await;
    db_b.close().await;
}
