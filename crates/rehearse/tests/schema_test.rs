//! Integration tests for `rehearse schema`.

mod support;

use support::{run_bin, unique_env_var_name, write_config, Scratch, TestDb};

#[tokio::test]
async fn schema_snapshot_captures_catalog_and_writes_json_file() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int PRIMARY KEY, name text);")
        .await;

    let scratch = Scratch::new();
    let url_env = unique_env_var_name();
    let config = write_config(&scratch, "t", &url_env, None);
    let out_path = scratch.path("snapshot.json");

    let result = run_bin(
        &[
            "schema",
            "--target",
            "t",
            "--out",
            out_path.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
        ],
        &[(&url_env, &db.url())],
    );

    assert_eq!(result.status, 0, "stderr: {}", result.stderr);
    let contents = std::fs::read_to_string(&out_path).expect("snapshot file written");
    let json: serde_json::Value = serde_json::from_str(&contents).expect("valid json");
    let tables = json["tables"].as_object().expect("tables object");
    assert!(
        tables.contains_key("public.widgets"),
        "expected public.widgets in snapshot, got {tables:?}"
    );

    db.close().await;
}
