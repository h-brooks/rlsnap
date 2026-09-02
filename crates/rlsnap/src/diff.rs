//! Cell-level diff between two snapshots, plus table and JSON rendering.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use pgcore::catalog::{FunctionInfo, PolicyInfo};
use pgcore::Outcome;
use serde::Serialize;
use thiserror::Error;

use crate::snapshot::{Finding, Snapshot, TablePolicies};

/// Why two snapshots cannot be diffed at all. Distinct from a `SnapshotDiff`
/// (which describes what changed between two *comparable* snapshots):
/// diffing an incompatible pair must be a loud error, not a diff that
/// silently reports "no changes" or, worse, a wall of privilege changes that
/// are really just an artifact of comparing catalog-mode outcomes against
/// behavioural-mode ones.
#[derive(Debug, Error)]
pub enum DiffError {
    #[error(
        "cannot diff snapshots with different format versions ({a} vs {b}) -- re-accept the baseline with the current rlsnap so both sides use the same schema version"
    )]
    FormatMismatch { a: u32, b: u32 },
    #[error(
        "cannot diff a {a:?}-mode snapshot against a {b:?}-mode snapshot -- probe outcomes are not comparable across modes"
    )]
    ModeMismatch { a: String, b: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct PrivChange {
    pub table: String,
    pub persona: String,
    pub op: String,
    pub column: Option<String>,
    pub before: Option<Outcome>,
    pub after: Option<Outcome>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PolicyChange {
    pub table: String,
    pub policy: String,
    pub field: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionChange {
    pub function: String,
    pub persona: String,
    pub before: Option<Outcome>,
    pub after: Option<Outcome>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDefChange {
    pub function: String,
    pub field: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingChange {
    pub name: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DataChange {
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SnapshotDiff {
    pub privilege_changes: Vec<PrivChange>,
    pub policy_changes: Vec<PolicyChange>,
    pub function_changes: Vec<FunctionChange>,
    pub function_def_changes: Vec<FunctionDefChange>,
    pub finding_changes: Vec<FindingChange>,
    pub data_changes: Vec<DataChange>,
}

impl SnapshotDiff {
    /// Whether this diff should fail CI, given `--strict-data`.
    pub fn exit_code(&self, strict_data: bool) -> i32 {
        let core_changed = !self.privilege_changes.is_empty()
            || !self.policy_changes.is_empty()
            || !self.function_changes.is_empty()
            || !self.function_def_changes.is_empty()
            || !self.finding_changes.is_empty();
        let data_changed = strict_data && !self.data_changes.is_empty();
        if core_changed || data_changed {
            1
        } else {
            0
        }
    }

    pub fn is_empty(&self) -> bool {
        self.privilege_changes.is_empty()
            && self.policy_changes.is_empty()
            && self.function_changes.is_empty()
            && self.function_def_changes.is_empty()
            && self.finding_changes.is_empty()
            && self.data_changes.is_empty()
    }
}

fn union_keys<'a, V>(
    a: &'a BTreeMap<String, V>,
    b: &'a BTreeMap<String, V>,
) -> BTreeSet<&'a String> {
    a.keys().chain(b.keys()).collect()
}

fn diff_privileges(a: &Snapshot, b: &Snapshot) -> Vec<PrivChange> {
    let mut out = Vec::new();
    for persona in union_keys(&a.privileges, &b.privileges) {
        let empty = BTreeMap::new();
        let ta = a.privileges.get(persona).unwrap_or(&empty);
        let tb = b.privileges.get(persona).unwrap_or(&empty);
        for table in union_keys(ta, tb) {
            let default_priv = Default::default();
            let pa = ta.get(table).unwrap_or(&default_priv);
            let pb = tb.get(table).unwrap_or(&default_priv);

            macro_rules! cmp_op {
                ($field:ident, $op:expr) => {
                    if pa.$field != pb.$field {
                        out.push(PrivChange {
                            table: table.to_string(),
                            persona: persona.to_string(),
                            op: $op.to_string(),
                            column: None,
                            before: pa.$field.clone(),
                            after: pb.$field.clone(),
                        });
                    }
                };
            }
            cmp_op!(select_count, "select_count");
            cmp_op!(insert, "insert");
            cmp_op!(insert_returning, "insert_returning");
            cmp_op!(delete, "delete");

            for col in union_keys(&pa.columns, &pb.columns) {
                let default_col = Default::default();
                let ca = pa.columns.get(col).unwrap_or(&default_col);
                let cb = pb.columns.get(col).unwrap_or(&default_col);
                if ca.select != cb.select {
                    out.push(PrivChange {
                        table: table.to_string(),
                        persona: persona.to_string(),
                        op: "select".to_string(),
                        column: Some(col.to_string()),
                        before: ca.select.clone(),
                        after: cb.select.clone(),
                    });
                }
                if ca.update != cb.update {
                    out.push(PrivChange {
                        table: table.to_string(),
                        persona: persona.to_string(),
                        op: "update".to_string(),
                        column: Some(col.to_string()),
                        before: ca.update.clone(),
                        after: cb.update.clone(),
                    });
                }
            }
        }
    }
    out.sort_by(|x, y| {
        (
            &x.table,
            &x.persona,
            &x.op,
            x.column.as_deref().unwrap_or(""),
        )
            .cmp(&(
                &y.table,
                &y.persona,
                &y.op,
                y.column.as_deref().unwrap_or(""),
            ))
    });
    out
}

fn policy_field(p: &PolicyInfo, field: &str) -> String {
    match field {
        "cmd" => p.cmd.clone(),
        "permissive" => p.permissive.to_string(),
        "roles" => p.roles.join(","),
        "qual" => p.qual.clone().unwrap_or_default(),
        "with_check" => p.with_check.clone().unwrap_or_default(),
        _ => unreachable!(),
    }
}

fn diff_policies(a: &Snapshot, b: &Snapshot) -> Vec<PolicyChange> {
    let mut out = Vec::new();
    for table in union_keys(&a.policies, &b.policies) {
        let empty = TablePolicies::default();
        let ta = a.policies.get(table).unwrap_or(&empty);
        let tb = b.policies.get(table).unwrap_or(&empty);

        // `ALTER TABLE ... {ENABLE,DISABLE} ROW LEVEL SECURITY` (and
        // `FORCE`/`NO FORCE`) never touch `pg_policies`, so a policy's own
        // fields can be byte-identical before and after while access
        // changed completely; these two flags must be compared alongside
        // the policy list itself, not just carried through unread.
        if ta.rls_enabled != tb.rls_enabled {
            out.push(PolicyChange {
                table: table.to_string(),
                policy: "(rls)".to_string(),
                field: "rls_enabled".to_string(),
                before: Some(ta.rls_enabled.to_string()),
                after: Some(tb.rls_enabled.to_string()),
            });
        }
        if ta.rls_forced != tb.rls_forced {
            out.push(PolicyChange {
                table: table.to_string(),
                policy: "(rls)".to_string(),
                field: "rls_forced".to_string(),
                before: Some(ta.rls_forced.to_string()),
                after: Some(tb.rls_forced.to_string()),
            });
        }

        let pa = &ta.policies;
        let pb = &tb.policies;
        for policy in union_keys(pa, pb) {
            match (pa.get(policy), pb.get(policy)) {
                (Some(x), Some(y)) => {
                    for field in ["cmd", "permissive", "roles", "qual", "with_check"] {
                        let fa = policy_field(x, field);
                        let fb = policy_field(y, field);
                        if fa != fb {
                            out.push(PolicyChange {
                                table: table.to_string(),
                                policy: policy.to_string(),
                                field: field.to_string(),
                                before: Some(fa),
                                after: Some(fb),
                            });
                        }
                    }
                }
                (Some(_), None) => out.push(PolicyChange {
                    table: table.to_string(),
                    policy: policy.to_string(),
                    field: "(policy)".to_string(),
                    before: Some("present".to_string()),
                    after: None,
                }),
                (None, Some(_)) => out.push(PolicyChange {
                    table: table.to_string(),
                    policy: policy.to_string(),
                    field: "(policy)".to_string(),
                    before: None,
                    after: Some("present".to_string()),
                }),
                (None, None) => unreachable!(),
            }
        }
    }
    out.sort_by(|x, y| (&x.table, &x.policy, &x.field).cmp(&(&y.table, &y.policy, &y.field)));
    out
}

fn diff_functions(a: &Snapshot, b: &Snapshot) -> Vec<FunctionChange> {
    let mut out = Vec::new();
    for persona in union_keys(&a.functions, &b.functions) {
        let empty = BTreeMap::new();
        let fa = a.functions.get(persona).unwrap_or(&empty);
        let fb = b.functions.get(persona).unwrap_or(&empty);
        for function in union_keys(fa, fb) {
            let before = fa.get(function).cloned();
            let after = fb.get(function).cloned();
            if before != after {
                out.push(FunctionChange {
                    function: function.to_string(),
                    persona: persona.to_string(),
                    before,
                    after,
                });
            }
        }
    }
    out.sort_by(|x, y| (&x.function, &x.persona).cmp(&(&y.function, &y.persona)));
    out
}

fn function_def_field(f: &FunctionInfo, field: &str) -> String {
    match field {
        "definition" => f.definition.clone().unwrap_or_default(),
        "owner" => f.owner.clone(),
        "volatility" => f.volatility.clone(),
        "config" => f.config.join(","),
        _ => unreachable!(),
    }
}

/// Diff function definitions (body, owner, volatility, config), which do not
/// show up in `function_changes` at all: that section only tracks EXECUTE
/// outcomes per persona, so a helper's body changing from restrictive to
/// permissive (with EXECUTE left untouched) would otherwise be invisible.
fn diff_function_defs(a: &Snapshot, b: &Snapshot) -> Vec<FunctionDefChange> {
    let mut out = Vec::new();
    for function in union_keys(&a.function_defs, &b.function_defs) {
        match (a.function_defs.get(function), b.function_defs.get(function)) {
            (Some(x), Some(y)) => {
                for field in ["definition", "owner", "volatility", "config"] {
                    let fa = function_def_field(x, field);
                    let fb = function_def_field(y, field);
                    if fa != fb {
                        out.push(FunctionDefChange {
                            function: function.to_string(),
                            field: field.to_string(),
                            before: Some(fa),
                            after: Some(fb),
                        });
                    }
                }
            }
            (Some(_), None) => out.push(FunctionDefChange {
                function: function.to_string(),
                field: "(function)".to_string(),
                before: Some("present".to_string()),
                after: None,
            }),
            (None, Some(_)) => out.push(FunctionDefChange {
                function: function.to_string(),
                field: "(function)".to_string(),
                before: None,
                after: Some("present".to_string()),
            }),
            (None, None) => unreachable!(),
        }
    }
    out.sort_by(|x, y| (&x.function, &x.field).cmp(&(&y.function, &y.field)));
    out
}

/// Group findings by name. A `BTreeMap<&str, &str>` here would silently
/// collapse two findings sharing a name (an easy copy-paste error in
/// `[[asserts]]` config, since global names are `assert:{name}` and
/// per-persona ones are `assert:{name}:{persona}`) to whichever one the
/// iteration order visited last, making one of them invisible to the diff.
fn findings_by_name(findings: &[Finding]) -> BTreeMap<&str, Vec<&str>> {
    let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for f in findings {
        map.entry(f.name.as_str())
            .or_default()
            .push(f.message.as_str());
    }
    for messages in map.values_mut() {
        messages.sort_unstable();
    }
    map
}

fn diff_findings(a: &Snapshot, b: &Snapshot) -> Vec<FindingChange> {
    let fa = findings_by_name(&a.findings);
    let fb = findings_by_name(&b.findings);
    let names: BTreeSet<&str> = fa.keys().chain(fb.keys()).copied().collect();
    let mut out = Vec::new();
    for name in names {
        let before = fa.get(name);
        let after = fb.get(name);
        if before != after {
            out.push(FindingChange {
                name: name.to_string(),
                before: before.map(|messages| messages.join("; ")),
                after: after.map(|messages| messages.join("; ")),
            });
        }
    }
    out.sort_by(|x, y| x.name.cmp(&y.name));
    out
}

fn diff_data(a: &Snapshot, b: &Snapshot) -> Vec<DataChange> {
    let mut out = Vec::new();
    let empty = BTreeMap::new();
    let ra = a.data.as_ref().map(|d| &d.row_counts).unwrap_or(&empty);
    let rb = b.data.as_ref().map(|d| &d.row_counts).unwrap_or(&empty);
    for persona in union_keys(ra, rb) {
        let empty_t = BTreeMap::new();
        let ta = ra.get(persona).unwrap_or(&empty_t);
        let tb = rb.get(persona).unwrap_or(&empty_t);
        for table in union_keys(ta, tb) {
            let before = ta.get(table).copied();
            let after = tb.get(table).copied();
            if before != after {
                out.push(DataChange {
                    description: format!(
                        "data.row_counts.{persona}.{table}: {} -> {}",
                        before
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                        after
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    ),
                });
            }
        }
    }
    out.sort_by(|x, y| x.description.cmp(&y.description));
    out
}

/// Compare two snapshots cell-by-cell. Rejects a pair that cannot be
/// meaningfully compared (mismatched format version, or one catalog-mode and
/// one behavioural-mode) rather than silently producing a confusing or empty
/// diff. `target` is intentionally not checked here: diffing snapshots from
/// two different targets (e.g. preview against prod) is a documented use.
pub fn diff(a: &Snapshot, b: &Snapshot) -> Result<SnapshotDiff, DiffError> {
    if a.format != b.format {
        return Err(DiffError::FormatMismatch {
            a: a.format,
            b: b.format,
        });
    }
    if a.mode != b.mode {
        return Err(DiffError::ModeMismatch {
            a: a.mode.clone(),
            b: b.mode.clone(),
        });
    }
    Ok(SnapshotDiff {
        privilege_changes: diff_privileges(a, b),
        policy_changes: diff_policies(a, b),
        function_changes: diff_functions(a, b),
        function_def_changes: diff_function_defs(a, b),
        finding_changes: diff_findings(a, b),
        data_changes: diff_data(a, b),
    })
}

/// Appended to `check`'s output when the diff's shape suggests
/// default-privilege drift between the environment a baseline was probed
/// against and the one it's being checked against, rather than a real
/// access-control regression.
pub const ENV_DRIFT_HINT: &str = "hint: default-privilege drift between environments can cause \
DeniedPrivilege/Allowed flips; baselines are best probed against a freshly migrated database \
(see docs)";

fn is_denied_privilege_allowed_flip(before: &Option<Outcome>, after: &Option<Outcome>) -> bool {
    matches!(
        (before, after),
        (Some(Outcome::DeniedPrivilege), Some(Outcome::Allowed))
            | (Some(Outcome::Allowed), Some(Outcome::DeniedPrivilege))
    )
}

/// Does this diff contain at least one cell that flipped between
/// `DeniedPrivilege` and `Allowed` (either direction)? Checked across
/// `privilege_changes` and `function_changes` -- the two sections whose
/// cells are EXECUTE/table/column outcomes a default-grant difference can
/// move. A single flip is enough: `check` prints [`ENV_DRIFT_HINT`] once no
/// matter how many entries flip.
pub fn has_env_drift_shape(d: &SnapshotDiff) -> bool {
    d.privilege_changes
        .iter()
        .any(|c| is_denied_privilege_allowed_flip(&c.before, &c.after))
        || d.function_changes
            .iter()
            .any(|c| is_denied_privilege_allowed_flip(&c.before, &c.after))
}

fn outcome_str(o: &Option<Outcome>) -> String {
    match o {
        None => "-".to_string(),
        Some(Outcome::Allowed) => "allowed".to_string(),
        Some(Outcome::DeniedPrivilege) => "denied_privilege".to_string(),
        Some(Outcome::DeniedRls) => "denied_rls".to_string(),
        Some(Outcome::Constraint { sqlstate }) => format!("constraint:{sqlstate}"),
        Some(Outcome::Error { sqlstate, message }) => format!("error:{sqlstate}:{message}"),
    }
}

/// Render the diff as a plain-text table, grouped by table (privileges),
/// then policies, functions, findings, and (if any) the data section.
pub fn render_table(d: &SnapshotDiff) -> String {
    let mut out = String::new();

    if !d.privilege_changes.is_empty() {
        let mut by_table: BTreeMap<&str, Vec<&PrivChange>> = BTreeMap::new();
        for c in &d.privilege_changes {
            by_table.entry(&c.table).or_default().push(c);
        }
        for (table, changes) in by_table {
            out.push_str(&format!("== table: {table} ==\n"));
            out.push_str(
                "persona          op                column          before             after\n",
            );
            for c in changes {
                out.push_str(&format!(
                    "{:<16} {:<17} {:<15} {:<18} {}\n",
                    c.persona,
                    c.op,
                    c.column.clone().unwrap_or_else(|| "-".to_string()),
                    outcome_str(&c.before),
                    outcome_str(&c.after),
                ));
            }
            out.push('\n');
        }
    }

    if !d.policy_changes.is_empty() {
        out.push_str("== policies ==\n");
        out.push_str("table                policy               field        before -> after\n");
        for c in &d.policy_changes {
            out.push_str(&format!(
                "{:<20} {:<20} {:<12} {} -> {}\n",
                c.table,
                c.policy,
                c.field,
                c.before.clone().unwrap_or_else(|| "-".to_string()),
                c.after.clone().unwrap_or_else(|| "-".to_string()),
            ));
        }
        out.push('\n');
    }

    if !d.function_changes.is_empty() {
        out.push_str("== functions ==\n");
        out.push_str(
            "persona          function                         before             after\n",
        );
        for c in &d.function_changes {
            out.push_str(&format!(
                "{:<16} {:<32} {:<18} {}\n",
                c.persona,
                c.function,
                outcome_str(&c.before),
                outcome_str(&c.after),
            ));
        }
        out.push('\n');
    }

    if !d.function_def_changes.is_empty() {
        out.push_str("== function definitions ==\n");
        out.push_str("function                         field        before -> after\n");
        for c in &d.function_def_changes {
            out.push_str(&format!(
                "{:<32} {:<12} {} -> {}\n",
                c.function,
                c.field,
                c.before.clone().unwrap_or_else(|| "-".to_string()),
                c.after.clone().unwrap_or_else(|| "-".to_string()),
            ));
        }
        out.push('\n');
    }

    if !d.finding_changes.is_empty() {
        out.push_str("== findings ==\n");
        for c in &d.finding_changes {
            match (&c.before, &c.after) {
                (None, Some(after)) => out.push_str(&format!("+ {}: {after}\n", c.name)),
                (Some(before), None) => out.push_str(&format!("- {}: {before}\n", c.name)),
                (Some(before), Some(after)) => {
                    out.push_str(&format!("~ {}: {before} -> {after}\n", c.name))
                }
                (None, None) => {}
            }
        }
        out.push('\n');
    }

    if !d.data_changes.is_empty() {
        out.push_str("== data ==\n");
        for c in &d.data_changes {
            out.push_str(&format!("{}\n", c.description));
        }
    }

    if out.is_empty() {
        out.push_str("no changes\n");
    }
    out
}

pub fn render_json(d: &SnapshotDiff) -> String {
    serde_json::to_string_pretty(d).expect("SnapshotDiff always serializes")
}
