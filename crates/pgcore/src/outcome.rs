//! Outcome classification for probe results.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The result of a single access-control probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Outcome {
    /// The operation succeeded.
    Allowed,
    /// SQLSTATE 42501, message names a table/column permission.
    DeniedPrivilege,
    /// SQLSTATE 42501, message names row-level security.
    DeniedRls,
    /// SQLSTATE class 23 (integrity constraint violation): privilege and RLS
    /// WITH CHECK both passed.
    Constraint { sqlstate: String },
    /// A legitimate probe outcome that isn't one of the above (see
    /// [`classify`] for exactly which SQLSTATEs qualify).
    Error { sqlstate: String, message: String },
}

/// A Postgres error that is not a legitimate probe outcome: a connection
/// failure, a resource limit, a statement or lock timeout or other operator
/// intervention, an internal error, or an error with no SQLSTATE at all (a
/// raw I/O error below the protocol level). None of these say anything
/// about whether a persona can perform an operation -- they say the
/// database, or this attempt, is broken -- so they must never be recorded
/// as an [`Outcome`] and end up in a baseline: a caller receiving this
/// should abort the whole snapshot run as a tool error, naming the
/// persona, table, and probe that hit it.
#[derive(Debug, Clone, Error)]
#[error("infrastructure failure (sqlstate {sqlstate:?}): {message}")]
pub struct InfrastructureError {
    pub sqlstate: String,
    pub message: String,
}

/// Classify a `tokio_postgres::Error` into an [`Outcome`], or reject it as
/// an [`InfrastructureError`] if it is not a legitimate probe outcome.
///
/// Legitimate probe outcomes (`Ok`): 42501 (privilege/RLS denial), class 23
/// (constraint violation), 0A000 (feature not supported -- e.g. `FOR
/// UPDATE` combined with `GROUP BY`/`DISTINCT`/an outer join), 428C9 (a
/// generated or identity column rejecting an explicit value -- this crate's
/// own column-update probe hits this for an identity primary key), 42883
/// (an unresolved function -- e.g. an unquoted mixed-case name failing to
/// resolve in `has_function_privilege`), and 2BP01 (dependent objects still
/// exist). This list is deliberately conservative and explicit: every class
/// 42 code other than 42883 is treated as an infrastructure failure rather
/// than assumed legitimate, since a probe hitting one (e.g. 42P01,
/// undefined_table) says something is wrong with the tool's own
/// assumptions, not with the persona being probed.
///
/// Everything else -- class 08 (connection), class 53 (resource), class 57
/// (operator intervention, including 57014 statement timeout and 55P03
/// lock timeout), class XX (internal), any other class 42 code, and any
/// error with no SQLSTATE at all -- is an [`InfrastructureError`]. A
/// timeout must never become baseline content.
///
/// Callers are responsible for producing [`Outcome::Allowed`] themselves when
/// an operation returns `Ok` — this function only has an error to look at.
pub fn classify(err: &tokio_postgres::Error) -> Result<Outcome, InfrastructureError> {
    let Some(db_err) = err.as_db_error() else {
        return Err(InfrastructureError {
            sqlstate: String::new(),
            message: err.to_string(),
        });
    };
    let code = db_err.code().code();
    let message = db_err.message();

    if code == "42501" {
        if message.contains("row-level security") {
            return Ok(Outcome::DeniedRls);
        }
        return Ok(Outcome::DeniedPrivilege);
    }

    if code.starts_with("23") {
        return Ok(Outcome::Constraint {
            sqlstate: code.to_string(),
        });
    }

    if matches!(code, "0A000" | "428C9" | "42883" | "2BP01") {
        return Ok(Outcome::Error {
            sqlstate: code.to_string(),
            message: message.to_string(),
        });
    }

    Err(InfrastructureError {
        sqlstate: code.to_string(),
        message: message.to_string(),
    })
}

#[cfg(test)]
mod serde_tests {
    use super::*;

    #[test]
    fn allowed_round_trips() {
        let o = Outcome::Allowed;
        let json = serde_json::to_string(&o).unwrap();
        let back: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(o, back);
    }

    #[test]
    fn constraint_round_trips() {
        let o = Outcome::Constraint {
            sqlstate: "23505".to_string(),
        };
        let json = serde_json::to_string(&o).unwrap();
        let back: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(o, back);
    }
}
