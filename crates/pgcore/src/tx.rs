//! `RollbackTx`: a probe transaction that can only ever end in `ROLLBACK`.
//!
//! Safety invariant: the only transaction terminator this module sends is
//! `ROLLBACK`. `finish()` is the documented path; `Drop` makes a best-effort
//! attempt when a caller forgets, but is not guaranteed to run (e.g. if the
//! process aborts, or there is no tokio runtime available to spawn on).

use thiserror::Error;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Row};

#[derive(Debug, Error)]
pub enum TxError {
    #[error("refusing to begin a probe transaction: connection is already inside a transaction it does not own (possible pooler misconfiguration)")]
    AlreadyInTransaction,

    #[error("BEGIN did not start a transaction as expected (possible pooler misconfiguration: backend pid changed across the BEGIN boundary, meaning `SET LOCAL` and this transaction are not confined to one backend)")]
    BeginDidNotTakeEffect,

    #[error("postgres error: {0}")]
    Postgres(#[from] tokio_postgres::Error),
}

pub type Result<T> = std::result::Result<T, TxError>;

/// A probe transaction on a dedicated connection. Always ends in `ROLLBACK`.
pub struct RollbackTx {
    client: Option<Client>,
    finished: bool,
}

impl RollbackTx {
    /// Begin a new probe transaction on `client`.
    ///
    /// Verifies the connection was not already inside a transaction it does
    /// not own, issues `BEGIN`, verifies the transaction actually started and
    /// stayed pinned to one backend (guarding against a transaction-pooling
    /// proxy silently splitting the session), then sets `statement_timeout`
    /// and `lock_timeout` local to the transaction.
    pub async fn begin(
        client: Client,
        statement_timeout_ms: u64,
        lock_timeout_ms: u64,
    ) -> Result<Self> {
        let already_assigned: bool = client
            .query_one("SELECT pg_current_xact_id_if_assigned() IS NOT NULL", &[])
            .await?
            .get(0);
        if already_assigned {
            return Err(TxError::AlreadyInTransaction);
        }
        let pid_before: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);

        let messages = client.simple_query("BEGIN").await?;
        let began = messages
            .iter()
            .any(|m| matches!(m, tokio_postgres::SimpleQueryMessage::CommandComplete(_)));
        if !began {
            return Err(TxError::BeginDidNotTakeEffect);
        }

        let pid_after: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        if pid_after != pid_before {
            return Err(TxError::BeginDidNotTakeEffect);
        }

        client
            .batch_execute(&format!(
                "SET LOCAL statement_timeout = {statement_timeout_ms}; SET LOCAL lock_timeout = {lock_timeout_ms};"
            ))
            .await?;

        Ok(RollbackTx {
            client: Some(client),
            finished: false,
        })
    }

    /// The underlying client, for issuing queries within this transaction.
    pub fn client(&self) -> &Client {
        self.client
            .as_ref()
            .expect("RollbackTx used after finish()")
    }

    pub async fn batch_execute(&self, sql: &str) -> std::result::Result<(), tokio_postgres::Error> {
        self.client().batch_execute(sql).await
    }

    pub async fn query(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> std::result::Result<Vec<Row>, tokio_postgres::Error> {
        self.client().query(sql, params).await
    }

    pub async fn execute(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> std::result::Result<u64, tokio_postgres::Error> {
        self.client().execute(sql, params).await
    }

    /// Open a savepoint within this transaction.
    pub async fn savepoint(
        &self,
        name: &str,
    ) -> std::result::Result<Savepoint<'_>, tokio_postgres::Error> {
        let ident = quote_ident(name);
        self.client()
            .batch_execute(&format!("SAVEPOINT {ident}"))
            .await?;
        Ok(Savepoint {
            tx: self,
            name: name.to_string(),
            resolved: false,
        })
    }

    /// End the transaction with `ROLLBACK`. This is the documented,
    /// guaranteed-clean way to end a `RollbackTx`; any error path still
    /// rolls back before returning.
    pub async fn finish(mut self) -> std::result::Result<(), tokio_postgres::Error> {
        let client = self.client.take().expect("RollbackTx used after finish()");
        self.finished = true;
        client.batch_execute("ROLLBACK").await
    }
}

impl Drop for RollbackTx {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Some(client) = self.client.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    let _ = client.batch_execute("ROLLBACK").await;
                });
            }
            // If there is no runtime to spawn on, this is a best-effort
            // guarantee only; `finish()` is the documented path.
        }
    }
}

/// A named savepoint inside a `RollbackTx`.
pub struct Savepoint<'a> {
    tx: &'a RollbackTx,
    name: String,
    resolved: bool,
}

impl<'a> Savepoint<'a> {
    pub async fn release(mut self) -> std::result::Result<(), tokio_postgres::Error> {
        let ident = quote_ident(&self.name);
        self.resolved = true;
        self.tx
            .client()
            .batch_execute(&format!("RELEASE SAVEPOINT {ident}"))
            .await
    }

    pub async fn rollback_to(mut self) -> std::result::Result<(), tokio_postgres::Error> {
        let ident = quote_ident(&self.name);
        self.resolved = true;
        self.tx
            .client()
            .batch_execute(&format!("ROLLBACK TO SAVEPOINT {ident}"))
            .await
    }

    /// Roll back to this savepoint, discarding any effects since it was
    /// established, then release it. Used by probes that must never leave a
    /// side effect (e.g. an `INSERT`) visible to the rest of a persona's
    /// transaction, whether the probe succeeded or failed: `release()` alone
    /// would keep a successful probe's effects live in the surrounding,
    /// still-open transaction; `rollback_to()` alone would keep the
    /// (now-empty) savepoint on the stack for the lifetime of that
    /// transaction, which probes in a tight loop would otherwise accumulate
    /// by the thousand.
    pub async fn discard(mut self) -> std::result::Result<(), tokio_postgres::Error> {
        let ident = quote_ident(&self.name);
        self.resolved = true;
        self.tx
            .client()
            .batch_execute(&format!(
                "ROLLBACK TO SAVEPOINT {ident}; RELEASE SAVEPOINT {ident}"
            ))
            .await
    }
}

impl<'a> Drop for Savepoint<'a> {
    fn drop(&mut self) {
        // Best-effort: an un-resolved savepoint left dangling should not
        // poison the outer transaction. We cannot run async code in Drop
        // synchronously, so callers are expected to call release/rollback_to;
        // this is a no-op safety net documented here rather than enforced.
        let _ = self.resolved;
    }
}

/// Double-quote a Postgres identifier, escaping embedded double quotes.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Quote a string literal for interpolation where a bind parameter cannot be
/// used (e.g. inside `SET LOCAL ROLE`, which does not accept protocol
/// parameters). Escapes embedded single quotes.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn quote_ident_escapes_quotes() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("weird\"name"), "\"weird\"\"name\"");
    }

    #[test]
    fn quote_literal_escapes_quotes() {
        assert_eq!(quote_literal("plain"), "'plain'");
        assert_eq!(quote_literal("o'brien"), "'o''brien'");
    }
}
