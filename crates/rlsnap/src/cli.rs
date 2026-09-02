//! Clap CLI definition and dispatch. `run` is the single seam every
//! integration test drives.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::accept::{self, OnlyReport};
use crate::config::RlsnapConfig;
use crate::snapshot::{self, Snapshot};
use crate::{diff, explain, init};

#[derive(Parser)]
#[command(
    name = "rlsnap",
    about = "Snapshot testing for Postgres access control"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a starter rlsnap.toml.
    Init,
    /// Snapshot a target's access matrix.
    Snapshot {
        #[arg(long)]
        target: String,
        #[arg(long)]
        out: Option<PathBuf>,
        /// Behavioural targets always capture the data layer (row counts,
        /// cap warnings, asserts). This flag exists only to reject a
        /// catalog target explicitly, since catalog mode never has one.
        #[arg(long)]
        with_rows: bool,
    },
    /// Diff two snapshot files.
    Diff {
        a: PathBuf,
        b: PathBuf,
        #[arg(long, value_enum, default_value_t = DiffFormat::Table)]
        format: DiffFormat,
        /// Let data-section changes (row counts) affect the exit code.
        #[arg(long)]
        strict_data: bool,
    },
    /// Snapshot a target and diff it against the committed baseline.
    Check {
        #[arg(long)]
        target: String,
        #[arg(long, value_enum, default_value_t = DiffFormat::Table)]
        format: DiffFormat,
        #[arg(long)]
        strict_data: bool,
        #[arg(long)]
        with_rows: bool,
    },
    /// Snapshot a target and overwrite the committed baseline with it.
    Accept {
        #[arg(long)]
        target: String,
        #[arg(long)]
        with_rows: bool,
        /// Update only baseline entries whose object identifier (a
        /// function signature or a schema.table name) matches this
        /// pattern (substring, or glob if it contains `*`). Repeatable;
        /// every entry not matched by any --only stays byte-identical to
        /// the existing baseline. Requires an existing baseline.
        #[arg(long)]
        only: Vec<String>,
    },
    /// Print the ACL entries and policies behind one cell.
    Explain {
        persona: String,
        table: String,
        op: String,
        #[arg(long)]
        target: String,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum DiffFormat {
    Table,
    Json,
}

fn resolve(cwd: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn write_snapshot(path: &Path, snap: &Snapshot) -> Result<()> {
    let json = serde_json::to_string_pretty(snap).context("serialize snapshot")? + "\n";
    std::fs::write(path, json).with_context(|| format!("write {}", path.display()))
}

fn read_snapshot(path: &Path) -> Result<Snapshot> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read snapshot {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse snapshot {}", path.display()))
}

/// Print exactly which entries a scoped `accept --only` updated, per
/// section, so the reviewer can see at a glance that nothing outside the
/// named patterns moved.
fn print_only_report(report: &OnlyReport) {
    print_only_section("functions", &report.functions);
    print_only_section("function_defs", &report.function_defs);
    print_only_section("privileges", &report.privileges);
    print_only_section("policies", &report.policies);
}

fn print_only_section(name: &str, entries: &[String]) {
    if entries.is_empty() {
        println!("  {name}: (none)");
        return;
    }
    println!("  {name}:");
    for entry in entries {
        println!("    {entry}");
    }
}

fn render_diff(d: &diff::SnapshotDiff, format: DiffFormat) -> String {
    match format {
        DiffFormat::Table => diff::render_table(d),
        DiffFormat::Json => diff::render_json(d),
    }
}

/// Parse `args` (argv-style, including the program name at index 0) and run
/// the requested subcommand with `cwd` as the working directory. Returns the
/// process exit code; never calls `std::process::exit` itself.
pub async fn run(args: &[String], cwd: &Path) -> Result<i32> {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() { 2 } else { 0 };
            eprint!("{e}");
            return Ok(code);
        }
    };

    match cli.command {
        Command::Init => {
            init::run_init(cwd)?;
            println!("wrote {}", cwd.join("rlsnap.toml").display());
            Ok(0)
        }

        Command::Snapshot {
            target,
            out,
            with_rows,
        } => {
            let config = RlsnapConfig::find_and_load(cwd)?;
            let snap = snapshot::build_snapshot(&config, &target, with_rows).await?;
            match out {
                Some(p) => write_snapshot(&resolve(cwd, &p), &snap)?,
                None => println!(
                    "{}",
                    serde_json::to_string_pretty(&snap).context("serialize snapshot")?
                ),
            }
            Ok(0)
        }

        Command::Diff {
            a,
            b,
            format,
            strict_data,
        } => {
            let sa = read_snapshot(&resolve(cwd, &a))?;
            let sb = read_snapshot(&resolve(cwd, &b))?;
            let d = diff::diff(&sa, &sb)?;
            print!("{}", render_diff(&d, format));
            Ok(d.exit_code(strict_data))
        }

        Command::Check {
            target,
            format,
            strict_data,
            with_rows,
        } => {
            let config = RlsnapConfig::find_and_load(cwd)?;
            let baseline_path = resolve(cwd, Path::new(&config.snapshot_path));
            if !baseline_path.exists() {
                bail!(
                    "no baseline at {} -- run `rlsnap accept --target {target}` first",
                    baseline_path.display()
                );
            }
            let baseline = read_snapshot(&baseline_path)?;
            let current = snapshot::build_snapshot(&config, &target, with_rows).await?;
            let d = diff::diff(&baseline, &current)?;
            print!("{}", render_diff(&d, format));
            // Only for the human-readable table: appending a text line
            // after JSON output would break every consumer parsing it.
            if matches!(format, DiffFormat::Table) && diff::has_env_drift_shape(&d) {
                println!("{}", diff::ENV_DRIFT_HINT);
            }
            Ok(d.exit_code(strict_data))
        }

        Command::Accept {
            target,
            with_rows,
            only,
        } => {
            let config = RlsnapConfig::find_and_load(cwd)?;
            let baseline_path = resolve(cwd, Path::new(&config.snapshot_path));
            let snap = snapshot::build_snapshot(&config, &target, with_rows).await?;

            if only.is_empty() {
                write_snapshot(&baseline_path, &snap)?;
                println!("wrote {}", baseline_path.display());
                return Ok(0);
            }

            if !baseline_path.exists() {
                bail!(
                    "no baseline at {} -- run `rlsnap accept --target {target}` (without \
                     --only) first",
                    baseline_path.display()
                );
            }
            let baseline = read_snapshot(&baseline_path)?;
            let (merged, report) = accept::scoped_merge(&baseline, &snap, &only)?;
            write_snapshot(&baseline_path, &merged)?;
            println!(
                "wrote {} (scoped to {} --only pattern(s))",
                baseline_path.display(),
                only.len()
            );
            print_only_report(&report);
            Ok(0)
        }

        Command::Explain {
            persona,
            table,
            op,
            target,
        } => {
            let config = RlsnapConfig::find_and_load(cwd)?;
            let out = explain::run_explain(&config, &target, &persona, &table, &op).await?;
            print!("{out}");
            Ok(0)
        }
    }
}
