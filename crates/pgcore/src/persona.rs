//! `Persona`: a role plus JWT claims, applied inside a `RollbackTx`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::sqlsplit;
use crate::tx::{quote_ident, RollbackTx};
use crate::txguard::transaction_control_violation;

/// Failure applying a persona: either its `setup_sql` was refused by the
/// transaction-control guard, or a plain Postgres error.
#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("persona {persona:?} setup_sql refused: {reason}")]
    SetupSqlRefused { persona: String, reason: String },
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub name: String,
    pub role: String,
    pub claims: Value,
    pub setup_sql: Option<String>,
}

impl Persona {
    pub fn new(name: impl Into<String>, role: impl Into<String>, claims: Value) -> Self {
        Persona {
            name: name.into(),
            role: role.into(),
            claims,
            setup_sql: None,
        }
    }

    pub fn with_setup_sql(mut self, sql: impl Into<String>) -> Self {
        self.setup_sql = Some(sql.into());
        self
    }

    /// Built-in `anon` persona: role `anon`, no claims.
    pub fn anon() -> Self {
        Persona::new("anon", "anon", Value::Object(Default::default()))
    }

    /// Built-in `authenticated` persona: role `authenticated`, claims carry
    /// `role: "authenticated"` as Supabase JWTs do.
    pub fn authenticated() -> Self {
        let mut claims = serde_json::Map::new();
        claims.insert(
            "role".to_string(),
            Value::String("authenticated".to_string()),
        );
        Persona::new("authenticated", "authenticated", Value::Object(claims))
    }

    /// Built-in `service_role` persona: role `service_role`, no claims (the
    /// service role bypasses RLS by grant, not by claim).
    pub fn service_role() -> Self {
        Persona::new(
            "service_role",
            "service_role",
            Value::Object(Default::default()),
        )
    }

    /// Apply this persona inside `tx`: run `setup_sql` (as the connecting
    /// role), then `SET LOCAL ROLE`, then set `request.jwt.claims` and one
    /// `request.jwt.claim.<key>` per top-level claim key.
    ///
    /// Every statement of `setup_sql` is checked against the
    /// transaction-control guard ([`transaction_control_violation`]) before
    /// any of it runs: `setup_sql` is user-supplied, and a `COMMIT` buried
    /// inside it would otherwise escape the caller's rollback-only
    /// transaction entirely, permanently applying every statement run so
    /// far. Nothing in `setup_sql` is sent to Postgres if any statement is
    /// refused.
    pub async fn apply(&self, tx: &RollbackTx) -> Result<(), ApplyError> {
        if let Some(sql) = &self.setup_sql {
            for stmt in sqlsplit::split(sql) {
                if let Some(reason) = transaction_control_violation(&stmt) {
                    return Err(ApplyError::SetupSqlRefused {
                        persona: self.name.clone(),
                        reason,
                    });
                }
            }
            tx.batch_execute(sql).await?;
        }

        self.apply_role_and_claims(tx).await?;
        Ok(())
    }

    /// Apply only the role/claims half of persona impersonation: `SET LOCAL
    /// ROLE`, then `request.jwt.claims` and one `request.jwt.claim.<key>`
    /// per top-level claim key. Skips `setup_sql` entirely.
    ///
    /// Used for catalog-mode probing, which never executes user-supplied
    /// SQL (catalog mode's whole promise is zero DML). Callers using this
    /// method are responsible for having already rejected any persona
    /// configured with `setup_sql` for a catalog target: silently skipping
    /// it here, with no caller-side check, would change results without
    /// saying so.
    pub async fn apply_role_and_claims(
        &self,
        tx: &RollbackTx,
    ) -> Result<(), tokio_postgres::Error> {
        let ident = quote_ident(&self.role);
        tx.batch_execute(&format!("SET LOCAL ROLE {ident}")).await?;

        let claims_text = self.claims.to_string();
        tx.query(
            "SELECT set_config('request.jwt.claims', $1, true)",
            &[&claims_text],
        )
        .await?;

        if let Value::Object(map) = &self.claims {
            for (key, value) in map {
                let setting_name = format!("request.jwt.claim.{key}");
                let setting_value = claim_value_to_text(value);
                tx.query(
                    "SELECT set_config($1, $2, true)",
                    &[&setting_name, &setting_value],
                )
                .await?;
            }
        }

        Ok(())
    }
}

/// Render a claim value as the text a GUC setting holds: bare for strings,
/// their canonical text form for scalars, JSON text for compound values.
fn claim_value_to_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn claim_value_to_text_unwraps_strings() {
        assert_eq!(claim_value_to_text(&Value::String("abc".into())), "abc");
    }

    #[test]
    fn claim_value_to_text_renders_scalars() {
        assert_eq!(claim_value_to_text(&Value::from(42)), "42");
        assert_eq!(claim_value_to_text(&Value::Bool(true)), "true");
    }

    #[test]
    fn built_in_personas_have_expected_roles() {
        assert_eq!(Persona::anon().role, "anon");
        assert_eq!(Persona::authenticated().role, "authenticated");
        assert_eq!(Persona::service_role().role, "service_role");
        assert_eq!(
            Persona::authenticated().claims["role"],
            Value::String("authenticated".into())
        );
    }
}
