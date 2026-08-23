//! Item E: a statement timeout (or any other infrastructure failure --
//! connection loss, resource exhaustion, an internal error) is not an
//! access-control verdict and must never become baseline content. It must
//! abort the whole snapshot run as a tool error, naming the persona,
//! table, and probe.

mod support;

use support::TestHarness;

const STAFF_PERSONA: &str = r#"
[[personas]]
name = "staff"
role = "authenticated"
claims = { sub = "staff-1" }
"#;

#[tokio::test]
async fn statement_timeout_never_becomes_baseline_content_and_aborts_the_run() {
    // `delay`'s default expression sleeps for 1 second, unconditionally, on
    // every INSERT DEFAULT VALUES. Set against a statement_timeout of
    // 100ms, the INSERT probe is deterministically cancelled by Postgres on
    // every attempt (not a flaky near-the-boundary race depending on
    // machine speed).
    let h = TestHarness::new(
        "CREATE TABLE slow_widgets ( \
            id bigint generated always as identity primary key, \
            delay text DEFAULT (pg_sleep(1)::text) \
         ); \
         GRANT SELECT, INSERT ON slow_widgets TO authenticated;",
        &format!("schemas = [\"public\"]\n{STAFF_PERSONA}"),
        "behavioural",
        1000,
    )
    .await;

    let config_text = std::fs::read_to_string(h.path("rlsnap.toml")).unwrap();
    let config_text = config_text.replace(
        "max_rows = 1000",
        "max_rows = 1000\nstatement_timeout_ms = 100",
    );
    std::fs::write(h.path("rlsnap.toml"), config_text).unwrap();

    let result = h
        .run(&["snapshot", "--target", "test", "--out", "snap.json"])
        .await;
    assert!(
        result.is_err(),
        "a statement timeout must abort the run as a tool error, never become recorded \
         baseline content"
    );
    let message = format!("{:#}", result.unwrap_err());
    // Every built-in persona sharing the "authenticated" role (and "staff",
    // which also uses it) probes this table concurrently, so whichever
    // persona's task happens to fail first is not deterministic; only that
    // *a* persona is named is asserted here.
    assert!(
        message.contains("persona"),
        "error must name the persona: {message}"
    );
    assert!(
        message.contains("slow_widgets"),
        "error must name the table: {message}"
    );
    assert!(
        message.to_ascii_lowercase().contains("insert"),
        "error must name the probe: {message}"
    );
    assert!(
        message.contains("57014"),
        "error should surface the timeout sqlstate: {message}"
    );

    assert!(
        !h.path("snap.json").exists(),
        "nothing must be written on an infrastructure abort"
    );

    h.close().await;
}
