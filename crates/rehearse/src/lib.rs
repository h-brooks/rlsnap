//! rehearse: run a migration against a real database inside a transaction
//! that always rolls back, and report what it would have done.
//!
//! Safety invariant: this crate never sends `COMMIT` to Postgres, only
//! `ROLLBACK` (via `pgcore::RollbackTx`). (This sentence is the only place
//! the word "COMMIT" appears in this crate's source.)

pub mod cli;
pub mod config;
pub mod drift;
pub mod migrate;
pub mod probe;
pub mod report;
pub mod snapshot;

use clap::Parser;

use cli::{resolve_schemas, Cli, Command, Format};

/// Parse `args` (argv-style, including the program name in position 0) and
/// run the requested subcommand. Returns the process exit code: 0 clean,
/// 1 the migration/drift check ran but found a problem, 2 a tool error
/// (connection, config, file).
pub async fn run<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return e.exit_code();
        }
    };

    match cli.command {
        Command::Run(args) => run_run(args).await,
        Command::Schema(args) => run_schema(args).await,
        Command::Drift(args) => run_drift(args).await,
    }
}

async fn run_run(args: cli::RunCli) -> i32 {
    let schemas = resolve_schemas(args.schemas);
    let run_args = migrate::RunArgs {
        migration_path: args.migration,
        target_name: args.target,
        config_path: args.config,
        schemas,
    };
    match migrate::run(run_args).await {
        Ok((report, exit_code)) => {
            match args.format {
                Format::Table => println!("{}", report.render_table()),
                Format::Json => println!("{}", report.to_json_pretty()),
            }
            exit_code
        }
        Err(e) => {
            eprintln!("rehearse: {e:#}");
            2
        }
    }
}

async fn run_schema(args: cli::SchemaCli) -> i32 {
    let schemas = resolve_schemas(args.schemas);
    let schema_args = snapshot::SchemaArgs {
        target_name: args.target,
        out_path: args.out,
        config_path: args.config,
        schemas,
    };
    match snapshot::run(schema_args).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("rehearse: {e:#}");
            2
        }
    }
}

async fn run_drift(args: cli::DriftCli) -> i32 {
    let schemas = resolve_schemas(args.schemas);
    let drift_args = drift::DriftArgs {
        a: args.a,
        b: args.b,
        config_path: args.config,
        schemas,
    };
    match drift::run(drift_args).await {
        Ok((diff, exit_code)) => {
            match args.format {
                Format::Table => {
                    if diff.is_empty() {
                        println!("no differences");
                    } else {
                        println!("{}", diff.render());
                    }
                }
                Format::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&diff).expect("CatalogDiff always serializes")
                ),
            }
            exit_code
        }
        Err(e) => {
            eprintln!("rehearse: {e:#}");
            2
        }
    }
}
