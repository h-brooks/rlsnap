//! `rehearse run`: execute a migration inside a `RollbackTx` and report what
//! it would have done.

use std::time::Instant;

use pgcore::{catalog, sqlsplit, RollbackTx};

use crate::config::LoadedConfig;
use crate::probe::{fetch_locks, fetch_writes};
use crate::report::{snippet, Failure, Report, StatementRecord, StatementStatus, Summary, Timing};

pub struct RunArgs {
    pub migration_path: std::path::PathBuf,
    pub target_name: String,
    pub config_path: Option<std::path::PathBuf>,
    pub schemas: Vec<String>,
}

/// Run the migration and produce a report. Returns `(report, exit_code)`
/// where `exit_code` is 0 (clean) or 1 (migration failed). Any error
/// returned by this function itself (connection, config, file) is a tool
/// error the caller should map to exit code 2.
pub async fn run(args: RunArgs) -> anyhow::Result<(Report, i32)> {
    let loaded = LoadedConfig::load(args.config_path.as_deref())?;
    let target = loaded.target(&args.target_name)?;
    let (statement_timeout_ms, lock_timeout_ms) = loaded.effective_timeouts(target);

    let sql_text = std::fs::read_to_string(&args.migration_path).map_err(|e| {
        anyhow::anyhow!(
            "failed to read migration file {}: {e}",
            args.migration_path.display()
        )
    })?;

    let client = target.connect().await?;
    let tx = RollbackTx::begin(client, statement_timeout_ms, lock_timeout_ms).await?;

    let before = catalog::snapshot(&tx, &args.schemas).await?;

    let statements = sqlsplit::split(&sql_text);
    let mut records = Vec::with_capacity(statements.len());
    let mut per_statement_ms = Vec::with_capacity(statements.len());
    let mut failure = None;
    let mut locks = Vec::new();
    let mut writes = Vec::new();

    let start_all = Instant::now();
    for (index, stmt) in statements.iter().enumerate() {
        let t0 = Instant::now();
        let result = tx.client().simple_query(stmt).await;
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        per_statement_ms.push(elapsed_ms);

        match result {
            Ok(_) => {
                records.push(StatementRecord {
                    index,
                    snippet: snippet(stmt),
                    status: StatementStatus::Ok,
                });
                // Locks and write counts are cumulative for the transaction,
                // so re-fetching after each successful statement keeps the
                // most recent state available even if a later statement
                // fails and aborts the transaction before we can query again.
                locks = fetch_locks(&tx, &args.schemas).await?;
                writes = fetch_writes(&tx).await?;
            }
            Err(err) => {
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
                    snippet: snippet(stmt),
                    sqlstate,
                    message,
                });
                break;
            }
        }
    }
    let total_ms = start_all.elapsed().as_millis() as u64;

    // If a statement failed, the transaction is left in an aborted state:
    // no further queries (including a catalog snapshot) can run until
    // ROLLBACK. Schema changes are only computed on a clean run.
    let schema_changes = if failure.is_none() {
        let after = catalog::snapshot(&tx, &args.schemas).await?;
        Some(pgcore::Catalog::diff(&before, &after))
    } else {
        None
    };

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
