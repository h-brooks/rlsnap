//! Outcome classification for probe results.

use serde::{Deserialize, Serialize};

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
    /// Anything else.
    Error { sqlstate: String, message: String },
}

/// Classify a `tokio_postgres::Error` into an [`Outcome`].
///
/// Callers are responsible for producing [`Outcome::Allowed`] themselves when
/// an operation returns `Ok` — this function only has an error to look at.
pub fn classify(err: &tokio_postgres::Error) -> Outcome {
    let Some(db_err) = err.as_db_error() else {
        return Outcome::Error {
            sqlstate: String::new(),
            message: err.to_string(),
        };
    };
    let code = db_err.code().code();
    let message = db_err.message();

    if code == "42501" {
        if message.contains("row-level security") {
            return Outcome::DeniedRls;
        }
        return Outcome::DeniedPrivilege;
    }

    if code.starts_with("23") {
        return Outcome::Constraint {
            sqlstate: code.to_string(),
        };
    }

    Outcome::Error {
        sqlstate: code.to_string(),
        message: message.to_string(),
    }
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
