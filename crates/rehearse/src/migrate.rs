//! `rehearse run`: execute a migration inside a `RollbackTx` and report what
//! it would have done.

use std::time::Instant;

use pgcore::{catalog, sqlsplit, Mode, RollbackTx};

use crate::config::LoadedConfig;
use crate::probe::{fetch_locks, fetch_writes, require_known_schemas};
use crate::report::{
    failure_snippet, snippet, Failure, Report, StatementRecord, StatementStatus, Summary, Timing,
};

pub struct RunArgs {
    pub migration_path: std::path::PathBuf,
    pub target_name: String,
    pub config_path: Option<std::path::PathBuf>,
    pub schemas: Vec<String>,
}

/// Run the migration and produce a report. Returns `(report, exit_code)`
/// where `exit_code` is 0 (clean) or 1 (migration failed). Any error
/// returned by this function itself (connection, config, file, an unknown
/// `--schema`, or a target configured read-only) is a tool error the caller
/// should map to exit code 2.
pub async fn run(args: RunArgs) -> anyhow::Result<(Report, i32)> {
    let loaded = LoadedConfig::load(args.config_path.as_deref())?;
    let target = loaded.target(&args.target_name)?;
    if target.mode == Mode::Catalog {
        anyhow::bail!(
            "target {:?} is configured with mode = \"catalog\" (read-only: catalog reads only, \
             no DML); refusing to run a migration against it. Use mode = \"behavioural\" for \
             targets `rehearse run` should execute against.",
            args.target_name
        );
    }
    let (statement_timeout_ms, lock_timeout_ms) = loaded.effective_timeouts(target);

    let sql_text = std::fs::read_to_string(&args.migration_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to read migration file {}: {e}",
            args.migration_path.display()
        )
    })?;

    let client = target.connect().await?;
    let tx = RollbackTx::begin(client, statement_timeout_ms, lock_timeout_ms).await?;
    require_known_schemas(&tx, &args.schemas).await?;

    let before = catalog::snapshot(&tx, &args.schemas).await?;

    let statements = sqlsplit::split(&sql_text);
    let mut records = Vec::with_capacity(statements.len());
    let mut per_statement_ms = Vec::with_capacity(statements.len());
    let mut failure = None;

    let start_all = Instant::now();
    for (index, stmt) in statements.iter().enumerate() {
        if let Some(reason) =
            transaction_control_violation(stmt).or_else(|| transaction_incompatible_violation(stmt))
        {
            // Never send this statement to Postgres at all: either it would
            // end the rehearsal transaction (defeating the tool's one
            // invariant, see the module doc on `lib.rs`), or Postgres would
            // refuse it outright with a confusing 25001 because it cannot
            // run inside a transaction block at all.
            per_statement_ms.push(0);
            records.push(StatementRecord {
                index,
                snippet: snippet(stmt),
                status: StatementStatus::Error,
            });
            failure = Some(Failure {
                statement_index: index,
                snippet: failure_snippet(stmt),
                sqlstate: String::new(),
                message: reason,
            });
            break;
        }

        // Wrap each statement in its own savepoint so a failure can be
        // recovered from (`ROLLBACK TO SAVEPOINT`) without losing the
        // transaction: that keeps it usable afterwards for reading back
        // locks, writes and the schema diff, instead of leaving the whole
        // rehearsal transaction aborted.
        let savepoint = tx.savepoint(&format!("rehearse_stmt_{index}")).await?;
        let t0 = Instant::now();
        let result = tx.client().simple_query(stmt).await;
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        per_statement_ms.push(elapsed_ms);

        match result {
            Ok(_) => {
                savepoint.release().await?;
                records.push(StatementRecord {
                    index,
                    snippet: snippet(stmt),
                    status: StatementStatus::Ok,
                });
            }
            Err(err) => {
                savepoint.rollback_to().await?;
                let (sqlstate, message) = match err.as_db_error() {
                    Some(db_err) => (
                        db_err.code().code().to_string(),
                        db_err.message().to_string(),
                    ),
                    None => (String::new(), err.to_string()),
                };
                records.push(StatementRecord {
                    index,
                    snippet: snippet(stmt),
                    status: StatementStatus::Error,
                });
                failure = Some(Failure {
                    statement_index: index,
                    snippet: failure_snippet(stmt),
                    sqlstate,
                    message,
                });
                break;
            }
        }
    }
    let total_ms = start_all.elapsed().as_millis() as u64;

    // The transaction is always usable here: either every statement
    // succeeded, or the failing statement's own savepoint was rolled back,
    // recovering the transaction to the state as of the last successful
    // statement. So locks, writes and the schema diff can always be read,
    // even for a failing run.
    let locks = fetch_locks(&tx).await?;
    let writes = fetch_writes(&tx).await?;
    let after = catalog::snapshot(&tx, &args.schemas).await?;
    let schema_changes = Some(pgcore::Catalog::diff(&before, &after));

    tx.finish().await?;

    let ok = failure.is_none();
    let report = Report {
        summary: Summary {
            ok,
            target: args.target_name,
            migration: args.migration_path.display().to_string(),
            statement_count: statements.len(),
        },
        statements: records,
        locks,
        writes,
        schema_changes,
        failure,
        timing: Timing {
            total_ms,
            per_statement_ms,
        },
    };

    Ok((report, if ok { 0 } else { 1 }))
}

/// Detects a statement that would itself end (or is a synonym for ending)
/// the rehearsal transaction — `COMMIT`, `END`, or a bare `ROLLBACK` (but
/// not `ROLLBACK TO SAVEPOINT`, which is safe: it stays inside the
/// transaction). Running any of these would defeat the tool's one
/// invariant: the migration only ever runs inside a single transaction that
/// rehearse itself ends with `ROLLBACK`. Returns a human-readable refusal
/// message when `stmt` matches; the statement is never sent to Postgres
/// when this returns `Some`.
///
/// Safety-rule note (see `lib.rs`): every occurrence of the words `COMMIT`,
/// `END`, and `ROLLBACK` below — in this function, its error messages, and
/// its unit tests — names a keyword this function detects and *refuses*,
/// never one it sends. This is the enforcement mechanism for the
/// always-`ROLLBACK` invariant, not an exception to it.
fn transaction_control_violation(stmt: &str) -> Option<String> {
    let effective = skip_leading_trivia(stmt);
    let upper = effective.to_ascii_uppercase();

    let word_boundary_follows = |rest: &str| -> bool {
        !rest
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
    };

    if let Some(rest) = upper.strip_prefix("COMMIT") {
        if word_boundary_follows(rest) {
            return Some(
                "refusing to run COMMIT: it would end the rehearsal transaction early, \
                 permanently applying every statement run so far"
                    .to_string(),
            );
        }
    }
    if let Some(rest) = upper.strip_prefix("END") {
        if word_boundary_follows(rest) {
            return Some(
                "refusing to run END: it is a synonym for COMMIT and would end the rehearsal \
                 transaction early, permanently applying every statement run so far"
                    .to_string(),
            );
        }
    }
    if let Some(rest) = upper.strip_prefix("ROLLBACK") {
        if word_boundary_follows(rest) && !rest.trim_start().starts_with("TO") {
            return Some(
                "refusing to run ROLLBACK: it would end the rehearsal transaction early, \
                 leaving later statements to run outside of it"
                    .to_string(),
            );
        }
    }
    None
}

/// Detects a statement that Postgres refuses to run at all inside a
/// transaction block (SQLSTATE 25001, "cannot run inside a transaction
/// block"): `CREATE`/`DROP INDEX CONCURRENTLY`, `REINDEX ... CONCURRENTLY`,
/// `VACUUM`, `CREATE`/`DROP DATABASE`, `ALTER SYSTEM`, and
/// `CREATE`/`DROP TABLESPACE`. Left undetected, one of these fails mid-run
/// with a confusing 25001 instead of a clear refusal; caught here, before it
/// is ever sent, the rest of the migration is not attempted either, so the
/// whole rehearsal is refused up front and rolled back like any other
/// invariant violation. Matching looks only at the statement's head (a
/// bounded run of leading keywords), never the rest of the statement, so a
/// string literal or later clause containing one of these words cannot
/// trigger a false positive.
fn transaction_incompatible_violation(stmt: &str) -> Option<String> {
    let effective = skip_leading_trivia(stmt);
    let words = head_words(effective, 8);
    let heads: Vec<&str> = words.iter().map(String::as_str).collect();

    let name = match heads.as_slice() {
        ["CREATE", "UNIQUE", "INDEX", "CONCURRENTLY", ..] => {
            Some("CREATE UNIQUE INDEX CONCURRENTLY")
        }
        ["CREATE", "INDEX", "CONCURRENTLY", ..] => Some("CREATE INDEX CONCURRENTLY"),
        ["DROP", "INDEX", "CONCURRENTLY", ..] => Some("DROP INDEX CONCURRENTLY"),
        ["VACUUM", ..] => Some("VACUUM"),
        ["CREATE", "DATABASE", ..] => Some("CREATE DATABASE"),
        ["DROP", "DATABASE", ..] => Some("DROP DATABASE"),
        ["ALTER", "SYSTEM", ..] => Some("ALTER SYSTEM"),
        ["CREATE", "TABLESPACE", ..] => Some("CREATE TABLESPACE"),
        ["DROP", "TABLESPACE", ..] => Some("DROP TABLESPACE"),
        ["REINDEX", rest @ ..] if rest.contains(&"CONCURRENTLY") => {
            Some("REINDEX ... CONCURRENTLY")
        }
        _ => None,
    }?;

    Some(format!(
        "refusing to run {name}: it cannot run inside a rehearsal transaction (Postgres does \
         not allow this statement inside a transaction block); run it directly against a \
         disposable branch or clone instead"
    ))
}

/// The first `max` alphanumeric/underscore "words" of `s`, upper-cased,
/// treating everything else (parentheses, commas, dots, whitespace) as a
/// separator. This tokenizes options like `VACUUM (FULL)` and
/// `REINDEX (CONCURRENTLY) INDEX foo`, and stays tolerant of `IF NOT
/// EXISTS`/`IF EXISTS` and schema-qualified names, while only ever looking
/// at a short, bounded prefix of the statement rather than searching its
/// full text (which is what keeps this from matching a keyword that
/// happens to appear inside a string literal or a later clause).
fn head_words(s: &str, max: usize) -> Vec<String> {
    s.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .take(max)
        .map(str::to_ascii_uppercase)
        .collect()
}

/// Skip leading whitespace and comments, so a comment placed before
/// `COMMIT`/`END`/`ROLLBACK` cannot hide it from `transaction_control_violation`.
fn skip_leading_trivia(s: &str) -> &str {
    let mut s = s;
    loop {
        let trimmed = s.trim_start();
        if let Some(rest) = trimmed.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, after)| after);
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |(_, after)| after);
            continue;
        }
        return trimmed;
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn blocks_commit_in_any_form() {
        assert!(transaction_control_violation("COMMIT").is_some());
        assert!(transaction_control_violation("commit;").is_some());
        assert!(transaction_control_violation("COMMIT WORK").is_some());
        assert!(transaction_control_violation("COMMIT AND CHAIN").is_some());
    }

    #[test]
    fn blocks_end_and_bare_rollback() {
        assert!(transaction_control_violation("END").is_some());
        assert!(transaction_control_violation("ROLLBACK").is_some());
        assert!(transaction_control_violation("ROLLBACK WORK").is_some());
        assert!(transaction_control_violation("ROLLBACK TRANSACTION").is_some());
    }

    #[test]
    fn allows_rollback_to_savepoint() {
        assert!(transaction_control_violation("ROLLBACK TO SAVEPOINT foo").is_none());
        assert!(transaction_control_violation("rollback to savepoint foo").is_none());
    }

    #[test]
    fn allows_ordinary_statements() {
        assert!(transaction_control_violation("SELECT 1").is_none());
        assert!(transaction_control_violation("UPDATE widgets SET qty = 1").is_none());
        assert!(transaction_control_violation("CREATE TABLE commit_log (id int)").is_none());
    }

    #[test]
    fn sees_through_leading_comments() {
        assert!(transaction_control_violation("-- sneaky\nCOMMIT").is_some());
        assert!(transaction_control_violation("/* sneaky */ COMMIT").is_some());
    }

    #[test]
    fn blocks_create_index_concurrently() {
        assert!(
            transaction_incompatible_violation("CREATE INDEX CONCURRENTLY idx ON t (c)").is_some()
        );
        assert!(transaction_incompatible_violation(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx ON t (c)"
        )
        .is_some());
        assert!(
            transaction_incompatible_violation("create index concurrently idx on t (c)").is_some()
        );
        assert!(transaction_incompatible_violation(
            "CREATE UNIQUE INDEX CONCURRENTLY idx ON t (c)"
        )
        .is_some());
    }

    #[test]
    fn allows_create_index_without_concurrently() {
        assert!(transaction_incompatible_violation("CREATE INDEX idx ON t (c)").is_none());
        assert!(transaction_incompatible_violation("CREATE UNIQUE INDEX idx ON t (c)").is_none());
        assert!(
            transaction_incompatible_violation("CREATE INDEX IF NOT EXISTS idx ON t (c)").is_none()
        );
    }

    #[test]
    fn blocks_drop_index_concurrently() {
        assert!(transaction_incompatible_violation("DROP INDEX CONCURRENTLY idx").is_some());
        assert!(
            transaction_incompatible_violation("DROP INDEX CONCURRENTLY IF EXISTS s.idx").is_some()
        );
    }

    #[test]
    fn blocks_reindex_concurrently_in_any_target_form() {
        assert!(transaction_incompatible_violation("REINDEX INDEX CONCURRENTLY idx").is_some());
        assert!(transaction_incompatible_violation("REINDEX TABLE CONCURRENTLY t").is_some());
        assert!(transaction_incompatible_violation("REINDEX SCHEMA CONCURRENTLY s").is_some());
        assert!(transaction_incompatible_violation("REINDEX (CONCURRENTLY) INDEX idx").is_some());
    }

    #[test]
    fn allows_reindex_without_concurrently() {
        assert!(transaction_incompatible_violation("REINDEX INDEX idx").is_none());
        assert!(transaction_incompatible_violation("REINDEX TABLE t").is_none());
    }

    #[test]
    fn blocks_vacuum_in_any_form() {
        assert!(transaction_incompatible_violation("VACUUM").is_some());
        assert!(transaction_incompatible_violation("VACUUM widgets").is_some());
        assert!(transaction_incompatible_violation("VACUUM (FULL) widgets").is_some());
        assert!(transaction_incompatible_violation("vacuum analyze widgets").is_some());
    }

    #[test]
    fn does_not_false_positive_on_vacuum_inside_a_string_literal() {
        assert!(transaction_incompatible_violation(
            "INSERT INTO logs (message) VALUES ('please VACUUM this table later')"
        )
        .is_none());
    }

    #[test]
    fn blocks_create_and_drop_database() {
        assert!(transaction_incompatible_violation("CREATE DATABASE foo").is_some());
        assert!(transaction_incompatible_violation("DROP DATABASE foo").is_some());
        assert!(transaction_incompatible_violation("DROP DATABASE IF EXISTS foo").is_some());
    }

    #[test]
    fn blocks_alter_system() {
        assert!(transaction_incompatible_violation("ALTER SYSTEM SET work_mem = '64MB'").is_some());
        assert!(transaction_incompatible_violation("ALTER SYSTEM RESET work_mem").is_some());
    }

    #[test]
    fn blocks_create_and_drop_tablespace() {
        assert!(
            transaction_incompatible_violation("CREATE TABLESPACE fast LOCATION '/data'").is_some()
        );
        assert!(transaction_incompatible_violation("DROP TABLESPACE fast").is_some());
    }

    #[test]
    fn allows_ordinary_statements_incompatible_check() {
        assert!(transaction_incompatible_violation("SELECT 1").is_none());
        assert!(transaction_incompatible_violation("CREATE TABLE widgets (id int)").is_none());
        assert!(
            transaction_incompatible_violation("ALTER TABLE widgets ADD COLUMN name text")
                .is_none()
        );
    }

    #[test]
    fn sees_through_leading_comments_for_transaction_incompatible_statements() {
        assert!(transaction_incompatible_violation("-- sneaky\nVACUUM").is_some());
        assert!(transaction_incompatible_violation("/* sneaky */ VACUUM").is_some());
    }

    #[test]
    fn transaction_incompatible_message_names_the_statement_and_a_disposable_alternative() {
        let msg = transaction_incompatible_violation("VACUUM").unwrap();
        assert!(msg.contains("VACUUM"));
        assert!(msg.contains("cannot run inside a rehearsal transaction"));
        assert!(msg.to_ascii_lowercase().contains("disposable"));
    }
}
