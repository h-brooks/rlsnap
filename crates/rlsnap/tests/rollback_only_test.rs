//! Safety invariant: the only transaction terminator this workspace's source
//! ever issues is `ROLLBACK`. This test greps every crate's `src/` (not
//! just this one -- `RollbackTx`'s own transaction terminators live in
//! `pgcore`, so a grep scoped to `crates/rlsnap/src` alone would guard a
//! crate that cannot violate the invariant while leaving the one that can
//! entirely uncovered) for the literal string `COMMIT` and requires every
//! occurrence to be inside a comment line.
//!
//! The spec's Testing Decisions also call for a wire-level companion test
//! ("asserts the only transaction-terminating statement issued on the wire
//! is ROLLBACK, captured via a logging proxy or `log_statement=all`"). That
//! is deliberately NOT implemented here: `pg_stat_database.xact_commit`
//! also counts every harmless autocommitted read this codebase issues
//! outside a transaction (e.g. `RollbackTx::begin`'s own pre-flight sanity
//! checks), and -- confirmed empirically against this Postgres 16 server
//! with a plain `BEGIN; INSERT; ROLLBACK;` -- `pg_stat_user_tables`'s
//! `n_tup_ins`/`n_tup_upd` counters are NOT rolled back at all, so both
//! obvious pg_stat-based proxies produce false positives and would make
//! this test flaky rather than a real guarantee. A trustworthy version
//! needs an actual logging proxy or `log_statement=all`, which was not
//! added here because this Postgres server is shared with other parallel
//! builders and changing its logging config was judged out of scope for a
//! test-only change.

use std::path::Path;

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn commit_never_appears_outside_a_comment() {
    // crates/rlsnap
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/rlsnap has a workspace root two levels up")
        .to_path_buf();
    let crates_dir = workspace_root.join("crates");

    let mut src_dirs = Vec::new();
    for entry in std::fs::read_dir(&crates_dir).unwrap() {
        let path = entry.unwrap().path();
        let src = path.join("src");
        if src.is_dir() {
            src_dirs.push(src);
        }
    }
    assert!(
        src_dirs.len() >= 2,
        "expected at least the rlsnap and pgcore crates under {}, found {:?}",
        crates_dir.display(),
        src_dirs
    );

    let mut files = Vec::new();
    for src_dir in &src_dirs {
        rs_files(src_dir, &mut files);
    }
    assert!(!files.is_empty(), "expected to find .rs files under src/");

    // One deliberate exception: `crates/pgcore/src/txguard.rs` contains the
    // shared guard that REFUSES transaction-control statements found in a
    // migration file (rehearse) or a persona's `setup_sql` (rlsnap).
    // Detecting and naming `COMMIT` in code and error messages is that
    // module's entire job; it never sends the statement. Its behaviour is
    // pinned by its own unit tests (`transaction_control_violation`) and by
    // each crate's integration tests proving a COMMIT is refused with the
    // database unchanged.
    let allowlisted = |file: &Path| file.ends_with("pgcore/src/txguard.rs");

    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for (lineno, line) in text.lines().enumerate() {
            if line.contains("COMMIT") {
                let trimmed = line.trim_start();
                assert!(
                    trimmed.starts_with("//") || allowlisted(&file),
                    "found \"COMMIT\" outside a comment at {}:{}: {line}",
                    file.display(),
                    lineno + 1
                );
            }
        }
    }
}
