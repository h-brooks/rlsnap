//! Per-cell probes: catalog-mode privilege checks (`has_table_privilege` &
//! friends) and behavioural-mode actual DML, each wrapped in its own
//! savepoint so an error (denial, timeout, or otherwise) never poisons the
//! rest of the persona's transaction, and retried on a statement timeout so
//! output stays deterministic across runs (see [`is_statement_timeout`]).
//!
//! Every savepoint is always discarded (`ROLLBACK TO SAVEPOINT`, then
//! `RELEASE SAVEPOINT`) whether the probe succeeded or failed, so a
//! successful behavioural probe's effects (e.g. an `INSERT ... DEFAULT
//! VALUES` row) never stay visible to a later probe, row count, or assert
//! running in the same persona's transaction. A failure to discard a
//! savepoint means the transaction is in an unknown, possibly poisoned
//! state, so it is propagated as a hard error rather than ignored: every
//! subsequent outcome in that run would otherwise be untrustworthy.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use pgcore::catalog::ColumnInfo;
use pgcore::config::Mode;
use pgcore::tx::quote_ident;
use pgcore::{classify, Outcome, RollbackTx};

use crate::snapshot::{ColumnPriv, TablePriv};

static SAVEPOINT_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_savepoint_name() -> String {
    format!(
        "rlsnap_sp_{}",
        SAVEPOINT_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// `"schema.table"` -> `"schema"."table"` with each part quoted.
fn quoted_table_ref(qualified: &str) -> String {
    match qualified.split_once('.') {
        Some((schema, table)) => format!("{}.{}", quote_ident(schema), quote_ident(table)),
        None => quote_ident(qualified),
    }
}

/// A statement-timeout error (57014) is wall-clock dependent, not an
/// access-control verdict: it says nothing about whether the persona can
/// perform the operation, only that this attempt didn't finish in time. Left
/// unretried, a probe that occasionally lands on either side of the timeout
/// boundary makes the whole snapshot non-deterministic across otherwise
/// identical runs. Retry a bounded number of times before accepting one as
/// final, so a run against an unchanged database converges to the same
/// answer: allowed/denied if the probe can complete within budget on any
/// attempt, or a consistent timeout error if it never can.
const MAX_TIMEOUT_ATTEMPTS: u32 = 3;

fn is_statement_timeout(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Error { sqlstate, .. } if sqlstate == "57014")
}

/// Run a boolean ACL-check function (`has_table_privilege`,
/// `has_column_privilege`, `has_function_privilege`) and translate the
/// result into an [`Outcome`]. These never touch table data. Wrapped in its
/// own savepoint so a lookup error (e.g. a relation that doesn't resolve)
/// cannot poison the rest of the persona's transaction, and retried on a
/// statement timeout for determinism (see [`is_statement_timeout`]).
pub(crate) async fn bool_check(
    tx: &RollbackTx,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<Outcome> {
    for attempt in 0..MAX_TIMEOUT_ATTEMPTS {
        let name = next_savepoint_name();
        let outcome = match tx.savepoint(&name).await {
            Ok(sp) => match tx.query(sql, params).await {
                Ok(rows) => {
                    let ok: bool = rows[0].get(0);
                    sp.discard().await.context("discard probe savepoint")?;
                    if ok {
                        Outcome::Allowed
                    } else {
                        Outcome::DeniedPrivilege
                    }
                }
                Err(e) => {
                    let outcome = classify(&e);
                    sp.discard()
                        .await
                        .context("discard probe savepoint after error")?;
                    outcome
                }
            },
            Err(e) => classify(&e),
        };
        if !is_statement_timeout(&outcome) || attempt + 1 == MAX_TIMEOUT_ATTEMPTS {
            return Ok(outcome);
        }
    }
    unreachable!("loop always returns on its last iteration")
}

pub(crate) async fn has_table_priv(
    tx: &RollbackTx,
    qualified: &str,
    priv_: &str,
) -> Result<Outcome> {
    let table_ref = quoted_table_ref(qualified);
    bool_check(
        tx,
        "SELECT has_table_privilege($1, $2)",
        &[&table_ref, &priv_],
    )
    .await
}

async fn has_column_priv(
    tx: &RollbackTx,
    qualified: &str,
    column: &str,
    priv_: &str,
) -> Result<Outcome> {
    let table_ref = quoted_table_ref(qualified);
    bool_check(
        tx,
        "SELECT has_column_privilege($1, $2, $3)",
        &[&table_ref, &column, &priv_],
    )
    .await
}

/// Run `sql` as a one-off DML probe inside its own savepoint, always
/// discarded afterwards (see the module doc) so neither a successful nor a
/// failed probe leaves the transaction changed. Retried on a statement
/// timeout (see [`is_statement_timeout`]).
async fn probe_dml(tx: &RollbackTx, sql: &str) -> Result<Outcome> {
    for attempt in 0..MAX_TIMEOUT_ATTEMPTS {
        let name = next_savepoint_name();
        let outcome = match tx.savepoint(&name).await {
            Ok(sp) => match tx.execute(sql, &[]).await {
                Ok(_) => {
                    sp.discard().await.context("discard probe savepoint")?;
                    Outcome::Allowed
                }
                Err(e) => {
                    let outcome = classify(&e);
                    sp.discard()
                        .await
                        .context("discard probe savepoint after error")?;
                    outcome
                }
            },
            Err(e) => classify(&e),
        };
        if !is_statement_timeout(&outcome) || attempt + 1 == MAX_TIMEOUT_ATTEMPTS {
            return Ok(outcome);
        }
    }
    unreachable!("loop always returns on its last iteration")
}

/// Like [`probe_dml`] but also returns the `count(*)` value on success. The
/// count is read-only and never needs undoing, but the savepoint is still
/// always discarded rather than released, for the same reason as every
/// other probe: consistency, and not leaving unreleased savepoints to
/// accumulate on the transaction's stack.
async fn probe_select_count(tx: &RollbackTx, qualified: &str) -> Result<(Outcome, Option<i64>)> {
    let sql = format!("SELECT count(*) FROM {}", quoted_table_ref(qualified));
    for attempt in 0..MAX_TIMEOUT_ATTEMPTS {
        let name = next_savepoint_name();
        let (outcome, count) = match tx.savepoint(&name).await {
            Ok(sp) => match tx.query(&sql, &[]).await {
                Ok(rows) => {
                    let count: i64 = rows[0].get(0);
                    sp.discard().await.context("discard probe savepoint")?;
                    (Outcome::Allowed, Some(count))
                }
                Err(e) => {
                    let outcome = classify(&e);
                    sp.discard()
                        .await
                        .context("discard probe savepoint after error")?;
                    (outcome, None)
                }
            },
            Err(e) => (classify(&e), None),
        };
        if !is_statement_timeout(&outcome) || attempt + 1 == MAX_TIMEOUT_ATTEMPTS {
            return Ok((outcome, count));
        }
    }
    unreachable!("loop always returns on its last iteration")
}

/// Probe every operation on one table for the currently-impersonated
/// persona. `want_row_count` controls whether the actual `count(*)` value
/// is captured (only meaningful, and only asked for, in behavioural mode).
pub async fn probe_table(
    tx: &RollbackTx,
    mode: Mode,
    insert_probes: bool,
    qualified: &str,
    columns: &BTreeMap<String, ColumnInfo>,
    want_row_count: bool,
) -> Result<(TablePriv, Option<i64>)> {
    let mut tp = TablePriv::default();
    let mut row_count = None;
    let table_ref = quoted_table_ref(qualified);

    tp.select_count = Some(match mode {
        Mode::Catalog => has_table_priv(tx, qualified, "SELECT").await?,
        Mode::Behavioural => {
            let (outcome, count) = probe_select_count(tx, qualified).await?;
            if want_row_count {
                row_count = count;
            }
            outcome
        }
    });

    if insert_probes {
        let insert_outcome = match mode {
            Mode::Catalog => has_table_priv(tx, qualified, "INSERT").await?,
            Mode::Behavioural => {
                probe_dml(tx, &format!("INSERT INTO {table_ref} DEFAULT VALUES")).await?
            }
        };
        tp.insert_returning = Some(match mode {
            Mode::Catalog => {
                if insert_outcome == Outcome::Allowed {
                    has_table_priv(tx, qualified, "SELECT").await?
                } else {
                    Outcome::DeniedPrivilege
                }
            }
            Mode::Behavioural => {
                probe_dml(
                    tx,
                    &format!("INSERT INTO {table_ref} DEFAULT VALUES RETURNING *"),
                )
                .await?
            }
        });
        tp.insert = Some(insert_outcome);
    }

    tp.delete = Some(match mode {
        Mode::Catalog => has_table_priv(tx, qualified, "DELETE").await?,
        Mode::Behavioural => probe_dml(tx, &format!("DELETE FROM {table_ref} WHERE false")).await?,
    });

    for column in columns.keys() {
        let column_ref = quote_ident(column);
        let select = Some(match mode {
            Mode::Catalog => has_column_priv(tx, qualified, column, "SELECT").await?,
            Mode::Behavioural => {
                probe_dml(tx, &format!("SELECT {column_ref} FROM {table_ref} LIMIT 0")).await?
            }
        });
        let update = Some(match mode {
            Mode::Catalog => has_column_priv(tx, qualified, column, "UPDATE").await?,
            Mode::Behavioural => {
                probe_dml(
                    tx,
                    &format!("UPDATE {table_ref} SET {column_ref} = {column_ref} WHERE false"),
                )
                .await?
            }
        });
        tp.columns
            .insert(column.clone(), ColumnPriv { select, update });
    }

    Ok((tp, row_count))
}

/// Probe EXECUTE privilege on one function. Never calls the function body
/// (out of scope in v1), and behaves the same in both modes.
pub async fn probe_function(tx: &RollbackTx, function_sig: &str) -> Result<Outcome> {
    bool_check(
        tx,
        "SELECT has_function_privilege($1, 'EXECUTE')",
        &[&function_sig],
    )
    .await
}

/// Run an assert query, returning how many rows it produced (0 = pass) as
/// the inner `Result`, or the classified [`Outcome`] if the assert query
/// itself errored (e.g. a malformed assert): that is a soft failure, meant
/// to be recorded as a finding, not a tool error. The outer `Result` is a
/// hard failure: the savepoint wrapping this probe could not be discarded,
/// which is always a tool error regardless of what the assert query did.
pub async fn run_assert(tx: &RollbackTx, sql: &str) -> Result<std::result::Result<i64, Outcome>> {
    let name = next_savepoint_name();
    let sp = match tx.savepoint(&name).await {
        Ok(sp) => sp,
        Err(e) => return Ok(Err(classify(&e))),
    };
    let wrapped = format!("SELECT count(*) FROM ({sql}) AS rlsnap_assert");
    match tx.query(&wrapped, &[]).await {
        Ok(rows) => {
            let count: i64 = rows[0].get(0);
            sp.discard().await.context("discard assert savepoint")?;
            Ok(Ok(count))
        }
        Err(e) => {
            let outcome = classify(&e);
            sp.discard()
                .await
                .context("discard assert savepoint after error")?;
            Ok(Err(outcome))
        }
    }
}
