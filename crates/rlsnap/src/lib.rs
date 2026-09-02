//! rlsnap: snapshot testing for Postgres access control.
//!
//! Safety invariant: this crate never sends `COMMIT` to Postgres, only
//! ever `ROLLBACK` (inherited from `pgcore::RollbackTx`; this crate issues
//! no transaction-terminating statements of its own). (This sentence is the
//! only place the word "COMMIT" appears in this crate's source.)

pub mod accept;
pub mod cli;
pub mod config;
pub mod diff;
pub mod explain;
pub mod glob;
pub mod init;
pub mod probe;
pub mod snapshot;

use std::path::Path;

/// The single library entry point the binary and every integration test
/// drive: parse `args` (argv-style, program name at index 0) and run the
/// requested subcommand against `cwd`. Returns the process exit code.
pub async fn run(args: &[String], cwd: &Path) -> anyhow::Result<i32> {
    cli::run(args, cwd).await
}
