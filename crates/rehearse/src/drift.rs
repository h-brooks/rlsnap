//! `rehearse drift`: catalog diff between two sources, each either a saved
//! JSON snapshot file or a live target given as `target:<name>`.

use pgcore::{catalog, Catalog, CatalogDiff, RollbackTx};

use crate::config::LoadedConfig;

pub struct DriftArgs {
    pub a: String,
    pub b: String,
    pub config_path: Option<std::path::PathBuf>,
    pub schemas: Vec<String>,
}

enum Source {
    File(std::path::PathBuf),
    Target(String),
}

fn parse_source(s: &str) -> Source {
    match s.strip_prefix("target:") {
        Some(name) => Source::Target(name.to_string()),
        None => Source::File(std::path::PathBuf::from(s)),
    }
}

async fn load_catalog(
    source: &Source,
    config_path: Option<&std::path::Path>,
    schemas: &[String],
) -> anyhow::Result<Catalog> {
    match source {
        Source::File(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("failed to read snapshot {}: {e}", path.display()))?;
            let catalog: Catalog = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("failed to parse snapshot {}: {e}", path.display()))?;
            Ok(catalog)
        }
        Source::Target(name) => {
            let loaded = LoadedConfig::load(config_path)?;
            let target = loaded.target(name)?;
            let (statement_timeout_ms, lock_timeout_ms) = loaded.effective_timeouts(target);
            let client = target.connect().await?;
            let tx = RollbackTx::begin(client, statement_timeout_ms, lock_timeout_ms).await?;
            let snap = catalog::snapshot(&tx, schemas).await?;
            tx.finish().await?;
            Ok(snap)
        }
    }
}

/// Compute the diff between the two sources. Exit code is 0 when identical,
/// 1 when they differ; a returned `Err` is a tool error (exit 2).
pub async fn run(args: DriftArgs) -> anyhow::Result<(CatalogDiff, i32)> {
    let a = parse_source(&args.a);
    let b = parse_source(&args.b);
    let config_path = args.config_path.as_deref();
    let catalog_a = load_catalog(&a, config_path, &args.schemas).await?;
    let catalog_b = load_catalog(&b, config_path, &args.schemas).await?;
    let diff = Catalog::diff(&catalog_a, &catalog_b);
    let exit_code = if diff.is_empty() { 0 } else { 1 };
    Ok((diff, exit_code))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn parses_target_prefix() {
        assert!(matches!(parse_source("target:prod"), Source::Target(n) if n == "prod"));
    }

    #[test]
    fn parses_bare_path_as_file() {
        assert!(
            matches!(parse_source("./a.json"), Source::File(p) if p == std::path::Path::new("./a.json"))
        );
    }
}
