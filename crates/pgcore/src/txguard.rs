//! Transaction-control guard: refuses any statement that would itself end
//! (or is a synonym or variant of ending) a transaction this crate does not
//! want ended -- rehearse's rehearsal transaction, or rlsnap's persona
//! probe transaction. Shared by both crates (and by `pgcore::Persona`'s own
//! `setup_sql`) so there is exactly one implementation of "which keywords
//! are transaction terminators", not two that could drift apart.
//!
//! Refuses: `COMMIT`, `END` (a synonym for `COMMIT`), a bare `ROLLBACK` (but
//! not `ROLLBACK TO SAVEPOINT`, which stays inside the transaction), `ABORT`
//! (a synonym for `ROLLBACK`), `PREPARE TRANSACTION` (detaches the current
//! transaction while keeping its locks held, strictly worse than `COMMIT`),
//! `COMMIT PREPARED`, and `ROLLBACK PREPARED` (both operate on an
//! already-detached prepared transaction and cannot run inside this one at
//! all). A plain `PREPARE` (a prepared *statement*, e.g. `PREPARE q AS
//! SELECT 1`) is not a transaction-control statement and is never refused;
//! the distinguishing keyword is `TRANSACTION`.
//!
//! Safety-rule note (see each crate's `lib.rs`): every occurrence of the
//! word `COMMIT` below -- in this module, its error messages, and its unit
//! tests -- names a keyword this function detects and *refuses*, never one
//! it sends.

fn word_boundary_follows(rest: &str) -> bool {
    !rest
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
}

/// Detects a statement that would itself end (or is a synonym or variant of
/// ending) the caller's transaction. Returns a human-readable refusal
/// message when `stmt` matches; the statement is never sent to Postgres
/// when this returns `Some`.
pub fn transaction_control_violation(stmt: &str) -> Option<String> {
    let effective = skip_leading_trivia(stmt);
    let upper = effective.to_ascii_uppercase();

    if let Some(rest) = upper.strip_prefix("COMMIT") {
        if word_boundary_follows(rest) {
            if rest.trim_start().starts_with("PREPARED") {
                return Some(
                    "refusing to run COMMIT PREPARED: it commits an already-prepared \
                     transaction and cannot run inside this transaction at all"
                        .to_string(),
                );
            }
            return Some(
                "refusing to run COMMIT: it would end the transaction early, \
                 permanently applying every statement run so far"
                    .to_string(),
            );
        }
    }
    if let Some(rest) = upper.strip_prefix("END") {
        if word_boundary_follows(rest) {
            return Some(
                "refusing to run END: it is a synonym for COMMIT and would end the \
                 transaction early, permanently applying every statement run so far"
                    .to_string(),
            );
        }
    }
    if let Some(rest) = upper.strip_prefix("ROLLBACK") {
        if word_boundary_follows(rest) {
            let trimmed_rest = rest.trim_start();
            if trimmed_rest.starts_with("TO") {
                return None;
            }
            if trimmed_rest.starts_with("PREPARED") {
                return Some(
                    "refusing to run ROLLBACK PREPARED: it discards an already-prepared \
                     transaction and cannot run inside this transaction at all"
                        .to_string(),
                );
            }
            return Some(
                "refusing to run ROLLBACK: it would end the transaction early, \
                 leaving later statements to run outside of it"
                    .to_string(),
            );
        }
    }
    if let Some(rest) = upper.strip_prefix("ABORT") {
        if word_boundary_follows(rest) {
            return Some(
                "refusing to run ABORT: it is a synonym for ROLLBACK and would end the \
                 transaction early, leaving later statements to run outside of it"
                    .to_string(),
            );
        }
    }
    if let Some(rest) = upper.strip_prefix("PREPARE") {
        if word_boundary_follows(rest) && rest.trim_start().starts_with("TRANSACTION") {
            return Some(
                "refusing to run PREPARE TRANSACTION: it detaches the transaction while \
                 keeping its locks held, which is strictly worse than COMMIT"
                    .to_string(),
            );
        }
    }
    None
}

/// Skip leading whitespace and comments, so a comment placed before
/// `COMMIT`/`END`/`ROLLBACK`/etc cannot hide it from
/// [`transaction_control_violation`].
pub fn skip_leading_trivia(s: &str) -> &str {
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
mod tests {
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
    fn blocks_abort_in_any_form() {
        assert!(transaction_control_violation("ABORT").is_some());
        assert!(transaction_control_violation("abort;").is_some());
        assert!(transaction_control_violation("ABORT WORK").is_some());
        assert!(transaction_control_violation("ABORT TRANSACTION").is_some());
    }

    #[test]
    fn blocks_prepare_transaction() {
        assert!(transaction_control_violation("PREPARE TRANSACTION 'foo'").is_some());
        assert!(transaction_control_violation("prepare transaction 'foo'").is_some());
    }

    #[test]
    fn allows_plain_prepare_statement() {
        assert!(transaction_control_violation("PREPARE q AS SELECT 1").is_none());
        assert!(transaction_control_violation("PREPARE q (int) AS SELECT $1").is_none());
    }

    #[test]
    fn blocks_commit_prepared_and_rollback_prepared() {
        assert!(transaction_control_violation("COMMIT PREPARED 'foo'").is_some());
        assert!(transaction_control_violation("ROLLBACK PREPARED 'foo'").is_some());
        assert!(transaction_control_violation("commit prepared 'foo'").is_some());
        assert!(transaction_control_violation("rollback prepared 'foo'").is_some());
    }
}
