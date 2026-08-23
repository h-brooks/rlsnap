//! `rlsnap init` writes a starter config and refuses to clobber one.

#[tokio::test]
async fn init_writes_starter_config_parseable_by_rlsnap() {
    let dir = std::env::temp_dir().join(format!("rlsnap_init_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let args: Vec<String> = vec!["rlsnap".to_string(), "init".to_string()];
    let code = rlsnap::run(&args, &dir).await.unwrap();
    assert_eq!(code, 0);

    let written = dir.join("rlsnap.toml");
    assert!(written.is_file());

    // The written file must be a valid rlsnap config.
    let cfg = rlsnap::config::RlsnapConfig::load(&written).unwrap();
    assert!(cfg.core.target("local").is_some());
    assert!(cfg.core.target("prod").is_some());
    assert_eq!(cfg.schemas, vec!["public".to_string()]);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn init_refuses_to_overwrite_existing_config() {
    let dir =
        std::env::temp_dir().join(format!("rlsnap_init_test_overwrite_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("rlsnap.toml"), "# pre-existing\n").unwrap();

    let args: Vec<String> = vec!["rlsnap".to_string(), "init".to_string()];
    let result = rlsnap::run(&args, &dir).await;
    assert!(
        result.is_err(),
        "init must not overwrite an existing config"
    );

    let contents = std::fs::read_to_string(dir.join("rlsnap.toml")).unwrap();
    assert_eq!(contents, "# pre-existing\n");

    std::fs::remove_dir_all(&dir).ok();
}
