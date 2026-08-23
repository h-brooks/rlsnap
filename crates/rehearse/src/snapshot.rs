//! `rehearse schema`: save a catalog snapshot to a JSON file.

use pgcore::{catalog, RollbackTx};

use crate::config::LoadedConfig;

pub struct SchemaArgs {
    pub target_name: String,
    pub out_path: std::path::PathBuf,
    pub config_path: Option<std::path::PathBuf>,
    pub schemas: Vec<String>,
}

pub async fn run(args: SchemaArgs) -> anyhow::Result<()> {
    let loaded = LoadedConfig::load(args.config_path.as_deref())?;
    let target = loaded.target(&args.target_name)?;
    let (statement_timeout_ms, lock_timeout_ms) = loaded.effective_timeouts(target);

    let client = target.connect("rehearse").await?;
    let tx = RollbackTx::begin(client, statement_timeout_ms, lock_timeout_ms).await?;
    let snap = catalog::snapshot(&tx, &args.schemas).await?;
    tx.finish().await?;

    let json = serde_json::to_string_pretty(&snap)?;
    std::fs::write(&args.out_path, json).map_err(|e| {
        anyhow::anyhow!(
            "failed to write snapshot to {}: {e}",
            args.out_path.display()
        )
    })?;

    Ok(())
}
