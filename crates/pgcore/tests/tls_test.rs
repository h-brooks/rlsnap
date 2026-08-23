//! Item D: TLS support in `Target::connect`. `sslmode` absent (the default
//! for every other integration test in this workspace) must fall back to
//! plaintext against the shared test Postgres container, which has no TLS
//! support configured; `sslmode=require` against that same container must
//! fail loudly with a clear error, never silently fall back.

mod support;

use support::TestDb;

fn unique_env_var(label: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "PGCORE_TLS_TEST_{label}_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[tokio::test]
async fn sslmode_absent_falls_back_to_plaintext_against_the_local_container() {
    let db = TestDb::create().await;
    let env_var = unique_env_var("absent");
    // SAFETY: each test uses a unique env var name, so concurrent tests
    // never race on the same key; this only guards against a torn write to
    // the process environment table itself.
    unsafe {
        std::env::set_var(&env_var, db.url());
    }

    let cfg = pgcore::Config::parse(&format!("[targets.t]\nurl_env = \"{env_var}\"\n")).unwrap();
    let target = cfg.target("t").unwrap();
    let client = target.connect("test").await.expect(
        "sslmode absent must default to prefer and fall back to plaintext against a \
         container with no TLS support",
    );
    let one: i32 = client.query_one("SELECT 1", &[]).await.unwrap().get(0);
    assert_eq!(one, 1);

    // SAFETY: see the comment above.
    unsafe {
        std::env::remove_var(&env_var);
    }
    db.close().await;
}

#[tokio::test]
async fn sslmode_require_against_the_local_container_fails_with_a_clear_error() {
    let db = TestDb::create().await;
    let url_with_require = format!("{}?sslmode=require", db.url());
    let env_var = unique_env_var("require");
    // SAFETY: see the comment in the sibling test above.
    unsafe {
        std::env::set_var(&env_var, &url_with_require);
    }

    let cfg = pgcore::Config::parse(&format!("[targets.t]\nurl_env = \"{env_var}\"\n")).unwrap();
    let target = cfg.target("t").unwrap();
    let err = target
        .connect("test")
        .await
        .expect_err("sslmode=require against a container with no TLS support must fail");
    let message = err.to_string();
    assert!(
        message.to_ascii_lowercase().contains("tls"),
        "expected a clear TLS-related error naming why the connection was refused, got: {message}"
    );

    // SAFETY: see the comment in the sibling test above.
    unsafe {
        std::env::remove_var(&env_var);
    }
    db.close().await;
}
