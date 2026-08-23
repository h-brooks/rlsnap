//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Table,
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "rehearse",
    about = "Rehearse a migration against a real database, always rolled back"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a migration inside a transaction that always rolls back, and
    /// report timings, locks, writes, and schema changes.
    Run(RunCli),
    /// Save a catalog snapshot of a target to a JSON file.
    Schema(SchemaCli),
    /// Diff two catalog snapshots (files, or live targets via `target:<name>`).
    Drift(DriftCli),
}

#[derive(Debug, Parser)]
pub struct RunCli {
    /// Path to the migration SQL file.
    pub migration: PathBuf,
    /// Name of the target to run against (from the config file).
    #[arg(long)]
    pub target: String,
    #[arg(long, value_enum, default_value = "table")]
    pub format: Format,
    /// Path to the config file (default: discover rlsnap.toml / pgkit.toml).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Schema to include in the catalog snapshot; repeatable (default: public).
    #[arg(long = "schema")]
    pub schemas: Vec<String>,
}

#[derive(Debug, Parser)]
pub struct SchemaCli {
    #[arg(long)]
    pub target: String,
    #[arg(long = "out")]
    pub out: PathBuf,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long = "schema")]
    pub schemas: Vec<String>,
}

#[derive(Debug, Parser)]
pub struct DriftCli {
    /// A JSON snapshot file path, or `target:<name>` for a live target.
    pub a: String,
    /// A JSON snapshot file path, or `target:<name>` for a live target.
    pub b: String,
    #[arg(long, value_enum, default_value = "table")]
    pub format: Format,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long = "schema")]
    pub schemas: Vec<String>,
}

/// Fall back to `["public"]` when no `--schema` flags were given.
pub fn resolve_schemas(schemas: Vec<String>) -> Vec<String> {
    if schemas.is_empty() {
        vec!["public".to_string()]
    } else {
        schemas
    }
}
