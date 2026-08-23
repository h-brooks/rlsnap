//! The `Snapshot` document produced by `rlsnap snapshot`/`check`/`accept`,
//! and the orchestration that builds one against a live target.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use pgcore::catalog::{self, ColumnInfo, FunctionInfo, PolicyInfo};
use pgcore::config::Mode;
use pgcore::{Outcome, Persona, RollbackTx};

use crate::config::{AssertCfg, RlsnapConfig};
use crate::glob::any_match;
use crate::probe;

/// Snapshot format 2 added `policies[table].rls_enabled`/`rls_forced`
/// (format 1 recorded only the policy list, silently losing an `ALTER TABLE
/// ... DISABLE ROW LEVEL SECURITY` that leaves the policies themselves
/// untouched) and the `function_defs` section.
pub const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColumnPriv {
    pub select: Option<Outcome>,
    pub update: Option<Outcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TablePriv {
    pub select_count: Option<Outcome>,
    pub insert: Option<Outcome>,
    pub insert_returning: Option<Outcome>,
    pub delete: Option<Outcome>,
    pub columns: BTreeMap<String, ColumnPriv>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub name: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DataSection {
    /// persona -> table -> row count (only present when the persona could
    /// select the table at all).
    pub row_counts: BTreeMap<String, BTreeMap<String, i64>>,
}

/// A table's row-level-security posture: whether RLS is on at all, whether
/// it is forced even for the table owner, and the policies defined on it.
/// Keeping `rls_enabled`/`rls_forced` alongside `policies` (rather than
/// dropping them) matters because `ALTER TABLE ... DISABLE ROW LEVEL
/// SECURITY` leaves every policy row in `pg_policies` untouched: a snapshot
/// that recorded only the policy list would see no diff at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TablePolicies {
    pub rls_enabled: bool,
    pub rls_forced: bool,
    pub policies: BTreeMap<String, PolicyInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub format: u32,
    pub tool_version: String,
    pub target: String,
    pub mode: String,
    /// persona -> table -> privileges.
    pub privileges: BTreeMap<String, BTreeMap<String, TablePriv>>,
    /// table -> RLS flags and policies.
    pub policies: BTreeMap<String, TablePolicies>,
    /// persona -> function signature -> EXECUTE outcome.
    pub functions: BTreeMap<String, BTreeMap<String, Outcome>>,
    /// function signature -> definition/owner/volatility/config, so a
    /// changed function body (or owner, volatility, search_path) is visible
    /// even when it doesn't change any persona's EXECUTE outcome.
    pub function_defs: BTreeMap<String, FunctionInfo>,
    pub findings: Vec<Finding>,
    pub data: Option<DataSection>,
}

fn mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::Catalog => "catalog",
        Mode::Behavioural => "behavioural",
    }
}

/// `"schema.func(args)"` -> `"schema.func"`, for matching exclude globs.
fn function_bare_name(sig: &str) -> &str {
    sig.split('(').next().unwrap_or(sig)
}

struct PersonaResult {
    name: String,
    tables: BTreeMap<String, TablePriv>,
    row_counts: BTreeMap<String, i64>,
    functions: BTreeMap<String, Outcome>,
    assert_findings: Vec<Finding>,
}

async fn run_persona(
    target: pgcore::config::Target,
    persona: Persona,
    tables: Arc<Vec<(String, BTreeMap<String, ColumnInfo>)>>,
    functions: Arc<Vec<String>>,
    per_persona_asserts: Arc<Vec<AssertCfg>>,
    mode: Mode,
    want_rows: bool,
) -> Result<PersonaResult> {
    let client = target
        .connect()
        .await
        .context("connect for persona probe")?;
    let tx = RollbackTx::begin(client, target.statement_timeout_ms, target.lock_timeout_ms)
        .await
        .context("begin persona probe transaction")?;
    persona
        .apply(&tx)
        .await
        .with_context(|| format!("apply persona {:?}", persona.name))?;

    let mut table_results = BTreeMap::new();
    let mut row_counts = BTreeMap::new();
    for (qualified, columns) in tables.iter() {
        let (tp, count) = probe::probe_table(
            &tx,
            mode,
            target.insert_probes,
            qualified,
            columns,
            want_rows,
        )
        .await;
        if let Some(c) = count {
            row_counts.insert(qualified.clone(), c);
        }
        table_results.insert(qualified.clone(), tp);
    }

    let mut function_results = BTreeMap::new();
    for sig in functions.iter() {
        let outcome = probe::probe_function(&tx, sig).await;
        function_results.insert(sig.clone(), outcome);
    }

    let mut assert_findings = Vec::new();
    if want_rows {
        for a in per_persona_asserts.iter() {
            let finding_name = format!("assert:{}:{}", a.name, persona.name);
            match probe::run_assert(&tx, &a.sql).await {
                Ok(0) => {}
                Ok(n) => assert_findings.push(Finding {
                    name: finding_name,
                    message: format!("{n} row(s) violate assert {:?} (expected 0)", a.name),
                }),
                Err(outcome) => assert_findings.push(Finding {
                    name: finding_name,
                    message: format!("assert {:?} errored: {outcome:?}", a.name),
                }),
            }
        }
    }

    tx.finish()
        .await
        .context("finish persona probe transaction")?;

    Ok(PersonaResult {
        name: persona.name.clone(),
        tables: table_results,
        row_counts,
        functions: function_results,
        assert_findings,
    })
}

/// Build a snapshot of `target_name` following `config`. `with_rows_flag` is
/// the CLI's `--with-rows` flag: its only remaining effect is to reject a
/// catalog target outright, since catalog mode can never produce the data
/// layer. The data layer itself (row counts, cap warnings, asserts) is
/// unconditional on any behavioural target -- per the snapshot format, it is
/// "present only in behavioural mode", not "present only when a flag was
/// passed". Gating it behind the flag made `accept --with-rows` followed by
/// the documented bare `check` report every finding as spuriously removed.
pub async fn build_snapshot(
    config: &RlsnapConfig,
    target_name: &str,
    with_rows_flag: bool,
) -> Result<Snapshot> {
    let target = config
        .core
        .target(target_name)
        .ok_or_else(|| anyhow!("unknown target {target_name:?}"))?
        .clone();

    if with_rows_flag && target.mode == Mode::Catalog {
        bail!("--with-rows requires a behavioural target (target {target_name:?} is catalog mode)");
    }
    let want_rows = target.mode == Mode::Behavioural;

    // Discover the schema shape (tables, columns, policies, functions) once,
    // as the connecting role, before probing any persona.
    let discovery_client = target.connect().await.context("connect for discovery")?;
    let discovery_tx = RollbackTx::begin(
        discovery_client,
        target.statement_timeout_ms,
        target.lock_timeout_ms,
    )
    .await
    .context("begin discovery transaction")?;
    let catalog = catalog::snapshot(&discovery_tx, &config.schemas)
        .await
        .context("snapshot catalog")?;

    let global_asserts: Vec<AssertCfg> = config
        .asserts
        .iter()
        .filter(|a| !a.per_persona)
        .cloned()
        .collect();
    let mut global_findings = Vec::new();
    if want_rows {
        for a in &global_asserts {
            let finding_name = format!("assert:{}", a.name);
            match probe::run_assert(&discovery_tx, &a.sql).await {
                Ok(0) => {}
                Ok(n) => global_findings.push(Finding {
                    name: finding_name,
                    message: format!("{n} row(s) violate assert {:?} (expected 0)", a.name),
                }),
                Err(outcome) => global_findings.push(Finding {
                    name: finding_name,
                    message: format!("assert {:?} errored: {outcome:?}", a.name),
                }),
            }
        }
    }
    discovery_tx
        .finish()
        .await
        .context("finish discovery transaction")?;

    let policies: BTreeMap<String, TablePolicies> = catalog
        .tables
        .iter()
        .filter(|(name, _)| !any_match(&config.excludes, name))
        .map(|(name, info)| {
            (
                name.clone(),
                TablePolicies {
                    rls_enabled: info.rls_enabled,
                    rls_forced: info.rls_forced,
                    policies: info.policies.clone(),
                },
            )
        })
        .collect();

    let function_defs: BTreeMap<String, FunctionInfo> = catalog
        .functions
        .iter()
        .filter(|(sig, _)| !any_match(&config.excludes, function_bare_name(sig)))
        .map(|(sig, info)| (sig.clone(), info.clone()))
        .collect();

    let filtered_tables: Vec<(String, BTreeMap<String, ColumnInfo>)> = catalog
        .tables
        .iter()
        .filter(|(name, _)| !any_match(&config.excludes, name))
        .map(|(name, info)| (name.clone(), info.columns.clone()))
        .collect();
    let filtered_functions: Vec<String> = catalog
        .functions
        .keys()
        .filter(|sig| !any_match(&config.excludes, function_bare_name(sig)))
        .cloned()
        .collect();

    let tables = Arc::new(filtered_tables);
    let functions = Arc::new(filtered_functions);
    let per_persona_asserts = Arc::new(
        config
            .asserts
            .iter()
            .filter(|a| a.per_persona)
            .cloned()
            .collect::<Vec<_>>(),
    );

    let mode = target.mode;
    let semaphore = Arc::new(Semaphore::new(config.pool_size.max(1)));
    let mut join_set = JoinSet::new();
    for persona in &config.personas {
        let target = target.clone();
        let persona = persona.clone();
        let tables = Arc::clone(&tables);
        let functions = Arc::clone(&functions);
        let per_persona_asserts = Arc::clone(&per_persona_asserts);
        let semaphore = Arc::clone(&semaphore);
        join_set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
            run_persona(
                target,
                persona,
                tables,
                functions,
                per_persona_asserts,
                mode,
                want_rows,
            )
            .await
        });
    }

    let mut privileges = BTreeMap::new();
    let mut functions_out = BTreeMap::new();
    let mut row_counts: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let mut findings = global_findings;

    while let Some(joined) = join_set.join_next().await {
        let result = joined.context("persona probe task panicked")??;
        privileges.insert(result.name.clone(), result.tables);
        functions_out.insert(result.name.clone(), result.functions);
        if !result.row_counts.is_empty() {
            row_counts.insert(result.name.clone(), result.row_counts);
        }
        findings.extend(result.assert_findings);
    }

    let data = if want_rows {
        for (persona, tables) in &row_counts {
            for (table, count) in tables {
                if *count as u64 > target.max_rows as u64 {
                    findings.push(Finding {
                        name: format!("cap:{persona}:{table}"),
                        message: format!(
                            "persona {persona:?} sees {count} row(s) in {table:?}, exceeding max_rows {}",
                            target.max_rows
                        ),
                    });
                }
            }
        }
        Some(DataSection { row_counts })
    } else {
        None
    };

    findings.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Snapshot {
        format: FORMAT_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        target: target_name.to_string(),
        mode: mode_str(mode).to_string(),
        privileges,
        policies,
        functions: functions_out,
        function_defs,
        findings,
        data,
    })
}
