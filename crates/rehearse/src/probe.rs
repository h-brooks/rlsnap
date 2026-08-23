//! Reading locks held and rows written by the current probe transaction.

use pgcore::RollbackTx;
use serde::{Deserialize, Serialize};

/// Ordered weakest to strongest, matching Postgres's relation lock hierarchy.
const LOCK_MODE_ORDER: &[&str] = &[
    "AccessShareLock",
    "RowShareLock",
    "RowExclusiveLock",
    "ShareUpdateExclusiveLock",
    "ShareLock",
    "ShareRowExclusiveLock",
    "ExclusiveLock",
    "AccessExclusiveLock",
];

/// Locks at `ShareUpdateExclusiveLock` or stronger block other writers (and,
/// for the strongest modes, readers too).
const BLOCKING_THRESHOLD: &str = "ShareUpdateExclusiveLock";

fn lock_rank(mode: &str) -> usize {
    LOCK_MODE_ORDER.iter().position(|m| *m == mode).unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub schema: String,
    pub relation: String,
    pub mode: String,
    /// True when `mode` is `ShareUpdateExclusiveLock` or stronger.
    pub blocking: bool,
}

/// Relation-level locks held by this backend on relations in `schemas`, one
/// entry per relation with the strongest mode seen, sorted by schema then
/// relation.
///
/// Scoped to `schemas` (the same list the catalog snapshot uses) rather than
/// reporting every lock this backend holds: rehearse's own introspection
/// queries take `AccessShareLock` on `pg_catalog`/`information_schema`
/// system tables, which is noise the caller never asked about; and a lock on
/// a table's TOAST relation is named `pg_toast_<oid>`, where the OID comes
/// from a non-transactional counter that keeps advancing even though every
/// rehearsal rolls back — reporting it would make the JSON report
/// nondeterministic across otherwise-identical runs.
pub async fn fetch_locks(
    tx: &RollbackTx,
    schemas: &[String],
) -> Result<Vec<LockInfo>, tokio_postgres::Error> {
    let rows = tx
        .query(
            "SELECT n.nspname, c.relname, l.mode \
             FROM pg_locks l \
             JOIN pg_class c ON c.oid = l.relation \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE l.pid = pg_backend_pid() AND l.locktype = 'relation' AND l.granted \
                   AND n.nspname = ANY($1)",
            &[&schemas],
        )
        .await?;

    let mut strongest: std::collections::BTreeMap<(String, String), String> =
        std::collections::BTreeMap::new();
    for row in rows {
        let schema: String = row.get(0);
        let relation: String = row.get(1);
        let mode: String = row.get(2);
        let key = (schema, relation);
        match strongest.get(&key) {
            Some(current) if lock_rank(current) >= lock_rank(&mode) => {}
            _ => {
                strongest.insert(key, mode);
            }
        }
    }

    let threshold = lock_rank(BLOCKING_THRESHOLD);
    Ok(strongest
        .into_iter()
        .map(|((schema, relation), mode)| {
            let blocking = lock_rank(&mode) >= threshold;
            LockInfo {
                schema,
                relation,
                mode,
                blocking,
            }
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteInfo {
    pub schema: String,
    pub relation: String,
    pub n_tup_ins: i64,
    pub n_tup_upd: i64,
    pub n_tup_del: i64,
}

/// Rows written by this transaction so far, one entry per table with any
/// non-zero count, sorted by schema then relation.
pub async fn fetch_writes(tx: &RollbackTx) -> Result<Vec<WriteInfo>, tokio_postgres::Error> {
    let rows = tx
        .query(
            "SELECT schemaname, relname, n_tup_ins, n_tup_upd, n_tup_del \
             FROM pg_stat_xact_user_tables \
             WHERE n_tup_ins > 0 OR n_tup_upd > 0 OR n_tup_del > 0 \
             ORDER BY schemaname, relname",
            &[],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| WriteInfo {
            schema: row.get(0),
            relation: row.get(1),
            n_tup_ins: row.get(2),
            n_tup_upd: row.get(3),
            n_tup_del: row.get(4),
        })
        .collect())
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn lock_rank_orders_by_strength() {
        assert!(lock_rank("AccessShareLock") < lock_rank("RowExclusiveLock"));
        assert!(lock_rank("RowExclusiveLock") < lock_rank("ShareUpdateExclusiveLock"));
        assert!(lock_rank("ExclusiveLock") < lock_rank("AccessExclusiveLock"));
    }

    #[test]
    fn unknown_mode_ranks_weakest() {
        assert_eq!(lock_rank("SomeFutureMode"), 0);
    }
}
