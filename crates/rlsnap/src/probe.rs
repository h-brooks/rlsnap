//! Per-cell probes: catalog-mode privilege checks (`has_table_privilege` &
//! friends) and behavioural-mode actual DML, each wrapped so a denial never
//! poisons the rest of the persona's transaction.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Run a boolean ACL-check function (`has_table_privilege`,
/// `has_column_privilege`, `has_function_privilege`) and translate the
/// result into an [`Outcome`]. These never touch table data.
async fn bool_check(
    tx: &RollbackTx,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Outcome {
    match tx.query(sql, params).await {
        Ok(rows) => {
            let ok: bool = rows[0].get(0);
            if ok {
                Outcome::Allowed
            } else {
                Outcome::DeniedPrivilege
            }
        }
        Err(e) => classify(&e),
    }
}

async fn has_table_priv(tx: &RollbackTx, qualified: &str, priv_: &str) -> Outcome {
    bool_check(
        tx,
        "SELECT has_table_privilege($1, $2)",
        &[&qualified, &priv_],
    )
    .await
}

async fn has_column_priv(tx: &RollbackTx, qualified: &str, column: &str, priv_: &str) -> Outcome {
    bool_check(
        tx,
        "SELECT has_column_privilege($1, $2, $3)",
        &[&qualified, &column, &priv_],
    )
    .await
}

/// Run `sql` as a one-off DML probe inside its own savepoint: on error, the
/// savepoint is rolled back so the outer transaction stays usable for the
/// next probe; on success it is released.
async fn probe_dml(tx: &RollbackTx, sql: &str) -> Outcome {
    let name = next_savepoint_name();
    let sp = match tx.savepoint(&name).await {
        Ok(sp) => sp,
        Err(e) => return classify(&e),
    };
    match tx.execute(sql, &[]).await {
        Ok(_) => {
            let _ = sp.release().await;
            Outcome::Allowed
        }
        Err(e) => {
            let outcome = classify(&e);
            let _ = sp.rollback_to().await;
            outcome
        }
    }
}

/// Like [`probe_dml`] but also returns the `count(*)` value on success.
async fn probe_select_count(tx: &RollbackTx, qualified: &str) -> (Outcome, Option<i64>) {
    let name = next_savepoint_name();
    let sp = match tx.savepoint(&name).await {
        Ok(sp) => sp,
        Err(e) => return (classify(&e), None),
    };
    let sql = format!("SELECT count(*) FROM {}", quoted_table_ref(qualified));
    match tx.query(&sql, &[]).await {
        Ok(rows) => {
            let count: i64 = rows[0].get(0);
            let _ = sp.release().await;
            (Outcome::Allowed, Some(count))
        }
        Err(e) => {
            let outcome = classify(&e);
            let _ = sp.rollback_to().await;
            (outcome, None)
        }
    }
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
) -> (TablePriv, Option<i64>) {
    let mut tp = TablePriv::default();
    let mut row_count = None;
    let table_ref = quoted_table_ref(qualified);

    tp.select_count = Some(match mode {
        Mode::Catalog => has_table_priv(tx, qualified, "SELECT").await,
        Mode::Behavioural => {
            let (outcome, count) = probe_select_count(tx, qualified).await;
            if want_row_count {
                row_count = count;
            }
            outcome
        }
    });

    if insert_probes {
        let insert_outcome = match mode {
            Mode::Catalog => has_table_priv(tx, qualified, "INSERT").await,
            Mode::Behavioural => {
                probe_dml(tx, &format!("INSERT INTO {table_ref} DEFAULT VALUES")).await
            }
        };
        tp.insert_returning = Some(match mode {
            Mode::Catalog => {
                if insert_outcome == Outcome::Allowed {
                    has_table_priv(tx, qualified, "SELECT").await
                } else {
                    Outcome::DeniedPrivilege
                }
            }
            Mode::Behavioural => {
                probe_dml(
                    tx,
                    &format!("INSERT INTO {table_ref} DEFAULT VALUES RETURNING *"),
                )
                .await
            }
        });
        tp.insert = Some(insert_outcome);
    }

    tp.delete = Some(match mode {
        Mode::Catalog => has_table_priv(tx, qualified, "DELETE").await,
        Mode::Behavioural => probe_dml(tx, &format!("DELETE FROM {table_ref} WHERE false")).await,
    });

    for column in columns.keys() {
        let column_ref = quote_ident(column);
        let select = Some(match mode {
            Mode::Catalog => has_column_priv(tx, qualified, column, "SELECT").await,
            Mode::Behavioural => {
                probe_dml(tx, &format!("SELECT {column_ref} FROM {table_ref} LIMIT 0")).await
            }
        });
        let update = Some(match mode {
            Mode::Catalog => has_column_priv(tx, qualified, column, "UPDATE").await,
            Mode::Behavioural => {
                probe_dml(
                    tx,
                    &format!("UPDATE {table_ref} SET {column_ref} = {column_ref} WHERE false"),
                )
                .await
            }
        });
        tp.columns
            .insert(column.clone(), ColumnPriv { select, update });
    }

    (tp, row_count)
}

/// Probe EXECUTE privilege on one function. Never calls the function body
/// (out of scope in v1), and behaves the same in both modes.
pub async fn probe_function(tx: &RollbackTx, function_sig: &str) -> Outcome {
    bool_check(
        tx,
        "SELECT has_function_privilege($1, 'EXECUTE')",
        &[&function_sig],
    )
    .await
}

/// Run an assert query, returning how many rows it produced (0 = pass).
/// Wrapped in a savepoint so a malformed assert query cannot poison the
/// rest of the transaction.
pub async fn run_assert(tx: &RollbackTx, sql: &str) -> Result<i64, Outcome> {
    let name = next_savepoint_name();
    let sp = tx.savepoint(&name).await.map_err(|e| classify(&e))?;
    let wrapped = format!("SELECT count(*) FROM ({sql}) AS rlsnap_assert");
    match tx.query(&wrapped, &[]).await {
        Ok(rows) => {
            let count: i64 = rows[0].get(0);
            let _ = sp.release().await;
            Ok(count)
        }
        Err(e) => {
            let outcome = classify(&e);
            let _ = sp.rollback_to().await;
            Err(outcome)
        }
    }
}
