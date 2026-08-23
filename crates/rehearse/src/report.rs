//! The `rehearse run` report.
//!
//! Determinism: every field is deterministic across runs against the same
//! migration and starting database state, *except* the two fields nested
//! under `timing` (`total_ms` and `per_statement_ms`). Callers that want to
//! compare two JSON reports for equality should parse both and drop the
//! `timing` key before comparing.

use pgcore::CatalogDiff;
use serde::{Deserialize, Serialize};

use crate::probe::{LockInfo, WriteInfo};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementRecord {
    pub index: usize,
    /// First line of the statement, truncated to 200 chars.
    pub snippet: String,
    pub status: StatementStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatementStatus {
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub statement_index: usize,
    /// First 200 chars of the full (possibly multi-line) statement — not
    /// limited to the first line, unlike `StatementRecord::snippet`, so a
    /// failing statement formatted across several lines still names the
    /// construct that actually errored.
    pub snippet: String,
    pub sqlstate: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub ok: bool,
    pub target: String,
    pub migration: String,
    pub statement_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timing {
    pub total_ms: u64,
    pub per_statement_ms: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub summary: Summary,
    pub statements: Vec<StatementRecord>,
    pub locks: Vec<LockInfo>,
    pub writes: Vec<WriteInfo>,
    pub schema_changes: Option<CatalogDiff>,
    pub failure: Option<Failure>,
    pub timing: Timing,
}

impl Report {
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("Report always serializes")
    }

    pub fn render_table(&self) -> String {
        let mut out = String::new();
        out.push_str("== Summary ==\n");
        out.push_str(&format!(
            "status: {}\n",
            if self.summary.ok { "ok" } else { "failed" }
        ));
        out.push_str(&format!("target: {}\n", self.summary.target));
        out.push_str(&format!("migration: {}\n", self.summary.migration));
        out.push_str(&format!("statements: {}\n", self.summary.statement_count));
        out.push_str(&format!("total_ms: {}\n", self.timing.total_ms));

        out.push_str("\n== Statements ==\n");
        for stmt in &self.statements {
            let ms = self.timing.per_statement_ms.get(stmt.index).unwrap_or(&0);
            let status = match stmt.status {
                StatementStatus::Ok => "ok",
                StatementStatus::Error => "ERROR",
            };
            out.push_str(&format!(
                "[{}] {}ms {} {}\n",
                stmt.index, ms, status, stmt.snippet
            ));
        }

        out.push_str("\n== Locks ==\n");
        if self.locks.is_empty() {
            out.push_str("(none)\n");
        }
        for lock in &self.locks {
            let note = if lock.blocking {
                "  (would block reads/writes)"
            } else {
                ""
            };
            out.push_str(&format!(
                "{}.{}: {}{}\n",
                lock.schema, lock.relation, lock.mode, note
            ));
        }

        out.push_str("\n== Writes ==\n");
        if self.writes.is_empty() {
            out.push_str("(none)\n");
        }
        for w in &self.writes {
            out.push_str(&format!(
                "{}.{}: ins={} upd={} del={}\n",
                w.schema, w.relation, w.n_tup_ins, w.n_tup_upd, w.n_tup_del
            ));
        }

        out.push_str("\n== Schema changes ==\n");
        match &self.schema_changes {
            None => out.push_str("(not computed: migration failed)\n"),
            Some(diff) if diff.is_empty() => out.push_str("(none)\n"),
            Some(diff) => {
                out.push_str(&diff.render());
                out.push('\n');
            }
        }

        if let Some(failure) = &self.failure {
            out.push_str("\n== Failure ==\n");
            out.push_str(&format!(
                "statement {}: SQLSTATE {}: {}\n{}\n",
                failure.statement_index, failure.sqlstate, failure.message, failure.snippet
            ));
        }

        out
    }
}

/// First line of `stmt`, truncated to 200 chars.
pub fn snippet(stmt: &str) -> String {
    let first_line = stmt.lines().next().unwrap_or("");
    first_line.chars().take(200).collect()
}

/// First 200 characters of the full (possibly multi-line) statement, used
/// for `Failure::snippet`. Real migrations are routinely formatted across
/// several lines, so naming only the first line of a failing statement
/// (as `snippet` does for the statements list) often omits the construct
/// that actually errored.
pub fn failure_snippet(stmt: &str) -> String {
    stmt.chars().take(200).collect()
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn snippet_takes_first_line() {
        assert_eq!(snippet("SELECT 1\nFROM t"), "SELECT 1");
    }

    #[test]
    fn snippet_truncates_to_200_chars() {
        let long = "x".repeat(300);
        assert_eq!(snippet(&long).chars().count(), 200);
    }

    #[test]
    fn failure_snippet_keeps_all_lines_up_to_200_chars() {
        let stmt =
            "INSERT INTO widgets (id, qty)\nSELECT bogus_column\nFROM a_table_that_does_not_exist";
        assert_eq!(failure_snippet(stmt), stmt);
        assert!(failure_snippet(stmt).contains("a_table_that_does_not_exist"));
    }

    #[test]
    fn failure_snippet_truncates_to_200_chars() {
        let long = "x".repeat(300);
        assert_eq!(failure_snippet(&long).chars().count(), 200);
    }
}
