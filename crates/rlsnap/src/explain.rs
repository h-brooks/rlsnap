//! `rlsnap explain <persona> <table> <op>`: print the ACL entries and
//! policies from the catalog relevant to one cell.

use anyhow::{anyhow, Context, Result};

use pgcore::catalog;
use pgcore::RollbackTx;

use crate::config::RlsnapConfig;
use crate::probe;

/// Map an op name (as used in the privileges section) to the Postgres
/// privilege/command it corresponds to.
fn op_to_privilege(op: &str) -> Result<&'static str> {
    match op {
        "select_count" | "select" => Ok("SELECT"),
        "insert" | "insert_returning" => Ok("INSERT"),
        "update" => Ok("UPDATE"),
        "delete" => Ok("DELETE"),
        other => Err(anyhow!(
            "unknown op {other:?} (expected one of: select_count, select, insert, insert_returning, update, delete)"
        )),
    }
}

/// Whether `persona_role` would exercise a grant made to `grantee` -- either
/// because it's a literal match, `grantee` is the `public` pseudo-role (any
/// casing, matching how `information_schema` reports it), or `persona_role`
/// is a (possibly indirect) member of `grantee` and so inherits its
/// privileges the same way `has_table_privilege`/`has_column_privilege`
/// already account for internally. Without this, explain's grant listing
/// only found grants made to the persona's literal role name and silently
/// missed anything inherited via role membership -- the normal shape for a
/// custom application role in Supabase.
async fn persona_holds_role(tx: &RollbackTx, persona_role: &str, grantee: &str) -> bool {
    if grantee.eq_ignore_ascii_case("public") || grantee == persona_role {
        return true;
    }
    tx.query(
        "SELECT pg_has_role($1, $2, 'MEMBER')",
        &[&persona_role, &grantee],
    )
    .await
    .ok()
    .and_then(|rows| rows.first().map(|row| row.get(0)))
    .unwrap_or(false)
}

pub async fn run_explain(
    config: &RlsnapConfig,
    target_name: &str,
    persona_name: &str,
    table: &str,
    op: &str,
) -> Result<String> {
    let target = config
        .core
        .target(target_name)
        .ok_or_else(|| anyhow!("unknown target {target_name:?}"))?;
    let persona = config
        .personas
        .iter()
        .find(|p| p.name == persona_name)
        .ok_or_else(|| anyhow!("unknown persona {persona_name:?}"))?;
    let privilege = op_to_privilege(op)?;

    let client = target.connect().await.context("connect")?;
    let tx = RollbackTx::begin(client, target.statement_timeout_ms, target.lock_timeout_ms)
        .await
        .context("begin transaction")?;
    let catalog = catalog::snapshot(&tx, &config.schemas)
        .await
        .context("snapshot catalog")?;

    let info = catalog.tables.get(table).ok_or_else(|| {
        anyhow!(
            "unknown table {table:?} (checked schemas: {:?})",
            config.schemas
        )
    })?;

    let mut out = String::new();
    out.push_str(&format!(
        "persona {persona_name:?} (role {:?}), table {table:?}, op {op:?} ({privilege})\n",
        persona.role
    ));

    // The same table-level check the snapshot itself would record for this
    // cell, so the grant/policy listing below can be cross-checked against
    // the actual outcome instead of trusted on faith.
    persona
        .apply(&tx)
        .await
        .with_context(|| format!("apply persona {persona_name:?}"))?;
    let outcome = probe::has_table_priv(&tx, table, privilege)
        .await
        .context("probe table privilege")?;
    out.push_str(&format!("outcome: {outcome:?}\n\n"));

    out.push_str("table-level grants:\n");
    let mut any_table_grant = false;
    for (grantee, privs) in &info.table_grants.grants {
        if privs.contains(privilege) && persona_holds_role(&tx, &persona.role, grantee).await {
            out.push_str(&format!("  GRANT {privilege} ... TO {grantee}\n"));
            any_table_grant = true;
        }
    }
    if !any_table_grant {
        out.push_str("  (none)\n");
    }

    out.push_str("\ncolumn-level grants:\n");
    let mut any_col_grant = false;
    for (column, grants) in &info.column_grants {
        for (grantee, privs) in &grants.grants {
            if privs.contains(privilege) && persona_holds_role(&tx, &persona.role, grantee).await {
                out.push_str(&format!(
                    "  GRANT {privilege} ({column}) ... TO {grantee}\n"
                ));
                any_col_grant = true;
            }
        }
    }
    if !any_col_grant {
        out.push_str("  (none)\n");
    }

    out.push_str(&format!(
        "\nrow-level security: enabled={} forced={}\n",
        info.rls_enabled, info.rls_forced
    ));
    out.push_str("matching policies:\n");
    let mut any_policy = false;
    for (name, policy) in &info.policies {
        let cmd_matches = policy.cmd == "ALL" || policy.cmd == privilege;
        let mut role_matches = false;
        for r in &policy.roles {
            if persona_holds_role(&tx, &persona.role, r).await {
                role_matches = true;
                break;
            }
        }
        if cmd_matches && role_matches {
            out.push_str(&format!(
                "  {name}: cmd={} permissive={} roles={:?}\n    USING: {}\n    WITH CHECK: {}\n",
                policy.cmd,
                policy.permissive,
                policy.roles,
                policy.qual.clone().unwrap_or_else(|| "-".to_string()),
                policy.with_check.clone().unwrap_or_else(|| "-".to_string()),
            ));
            any_policy = true;
        }
    }
    if !any_policy {
        out.push_str("  (none)\n");
    }

    tx.finish().await.context("finish transaction")?;

    Ok(out)
}
