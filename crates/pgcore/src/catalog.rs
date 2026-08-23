//! `catalog::snapshot`: a serialisable, sorted, timestamp-free description
//! of the schema's access-control surface, and `Catalog::diff` to compare
//! two snapshots.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::tx::RollbackTx;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ColumnInfo {
    pub data_type: String,
    pub not_null: bool,
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PolicyInfo {
    pub cmd: String,
    pub permissive: bool,
    pub roles: Vec<String>,
    pub qual: Option<String>,
    pub with_check: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GrantSet {
    /// grantee -> sorted set of privileges
    pub grants: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TableInfo {
    pub rls_enabled: bool,
    pub rls_forced: bool,
    pub columns: BTreeMap<String, ColumnInfo>,
    pub indexes: BTreeMap<String, String>,
    pub constraints: BTreeMap<String, String>,
    pub policies: BTreeMap<String, PolicyInfo>,
    pub triggers: BTreeMap<String, String>,
    pub table_grants: GrantSet,
    pub column_grants: BTreeMap<String, GrantSet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FunctionInfo {
    pub arguments: String,
    pub returns: String,
    pub security_definer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ViewInfo {
    pub definition: String,
}

/// A full catalog snapshot. Keys are `"schema.name"`, sorted lexicographically
/// by virtue of `BTreeMap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Catalog {
    pub tables: BTreeMap<String, TableInfo>,
    pub views: BTreeMap<String, ViewInfo>,
    /// Key is `"schema.name(arguments)"` to disambiguate overloads.
    pub functions: BTreeMap<String, FunctionInfo>,
}

/// Collapse runs of whitespace to a single space and trim ends, so
/// insignificant formatting differences in `pg_get_*` output don't appear as
/// diffs.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn qualify(schema: &str, name: &str) -> String {
    format!("{schema}.{name}")
}

/// Snapshot the access-control surface of `schemas` as seen inside `tx`.
pub async fn snapshot(
    tx: &RollbackTx,
    schemas: &[String],
) -> Result<Catalog, tokio_postgres::Error> {
    let mut catalog = Catalog::default();

    // Tables: RLS flags.
    let rows = tx
        .query(
            "SELECT n.nspname, c.relname, c.relrowsecurity, c.relforcerowsecurity \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r','p') AND n.nspname = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let rls_enabled: bool = row.get(2);
        let rls_forced: bool = row.get(3);
        let entry = catalog.tables.entry(qualify(&schema, &name)).or_default();
        entry.rls_enabled = rls_enabled;
        entry.rls_forced = rls_forced;
    }

    // Columns.
    let rows = tx
        .query(
            "SELECT n.nspname, c.relname, a.attname, format_type(a.atttypid, a.atttypmod), \
                    a.attnotnull, pg_get_expr(ad.adbin, ad.adrelid) \
             FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_attrdef ad ON ad.adrelid = c.oid AND ad.adnum = a.attnum \
             WHERE a.attnum > 0 AND NOT a.attisdropped AND c.relkind IN ('r','p') \
                   AND n.nspname = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let column: String = row.get(2);
        let data_type: String = row.get(3);
        let not_null: bool = row.get(4);
        let default: Option<String> = row.get(5);
        let entry = catalog.tables.entry(qualify(&schema, &table)).or_default();
        entry.columns.insert(
            column,
            ColumnInfo {
                data_type,
                not_null,
                default: default.map(|d| normalize_ws(&d)),
            },
        );
    }

    // Indexes.
    let rows = tx
        .query(
            "SELECT n.nspname, t.relname, i.relname, pg_get_indexdef(ix.indexrelid) \
             FROM pg_index ix \
             JOIN pg_class i ON i.oid = ix.indexrelid \
             JOIN pg_class t ON t.oid = ix.indrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let index: String = row.get(2);
        let def: String = row.get(3);
        let entry = catalog.tables.entry(qualify(&schema, &table)).or_default();
        entry.indexes.insert(index, normalize_ws(&def));
    }

    // Constraints.
    let rows = tx
        .query(
            "SELECT n.nspname, cl.relname, con.conname, pg_get_constraintdef(con.oid) \
             FROM pg_constraint con \
             JOIN pg_class cl ON cl.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = cl.relnamespace \
             WHERE n.nspname = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let name: String = row.get(2);
        let def: String = row.get(3);
        let entry = catalog.tables.entry(qualify(&schema, &table)).or_default();
        entry.constraints.insert(name, normalize_ws(&def));
    }

    // Policies.
    let rows = tx
        .query(
            "SELECT schemaname, tablename, policyname, permissive, roles::text[], cmd, qual, with_check \
             FROM pg_policies \
             WHERE schemaname = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let name: String = row.get(2);
        let permissive: String = row.get(3);
        let roles: Vec<String> = row.get(4);
        let cmd: String = row.get(5);
        let qual: Option<String> = row.get(6);
        let with_check: Option<String> = row.get(7);
        let mut roles = roles;
        roles.sort();
        let entry = catalog.tables.entry(qualify(&schema, &table)).or_default();
        entry.policies.insert(
            name,
            PolicyInfo {
                cmd,
                permissive: permissive == "PERMISSIVE",
                roles,
                qual: qual.map(|q| normalize_ws(&q)),
                with_check: with_check.map(|w| normalize_ws(&w)),
            },
        );
    }

    // Triggers.
    let rows = tx
        .query(
            "SELECT n.nspname, c.relname, t.tgname, pg_get_triggerdef(t.oid) \
             FROM pg_trigger t \
             JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE NOT t.tgisinternal AND n.nspname = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let name: String = row.get(2);
        let def: String = row.get(3);
        let entry = catalog.tables.entry(qualify(&schema, &table)).or_default();
        entry.triggers.insert(name, normalize_ws(&def));
    }

    // Table-level ACLs.
    let rows = tx
        .query(
            "SELECT table_schema, table_name, grantee, privilege_type \
             FROM information_schema.role_table_grants \
             WHERE table_schema = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let grantee: String = row.get(2);
        let privilege: String = row.get(3);
        let entry = catalog.tables.entry(qualify(&schema, &table)).or_default();
        entry
            .table_grants
            .grants
            .entry(grantee)
            .or_default()
            .insert(privilege);
    }

    // Column-level ACLs.
    let rows = tx
        .query(
            "SELECT table_schema, table_name, column_name, grantee, privilege_type \
             FROM information_schema.role_column_grants \
             WHERE table_schema = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let column: String = row.get(2);
        let grantee: String = row.get(3);
        let privilege: String = row.get(4);
        let entry = catalog.tables.entry(qualify(&schema, &table)).or_default();
        entry
            .column_grants
            .entry(column)
            .or_default()
            .grants
            .entry(grantee)
            .or_default()
            .insert(privilege);
    }

    // Views.
    let rows = tx
        .query(
            "SELECT schemaname, viewname, definition FROM pg_views WHERE schemaname = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let definition: String = row.get(2);
        catalog.views.insert(
            qualify(&schema, &name),
            ViewInfo {
                definition: normalize_ws(&definition),
            },
        );
    }

    // Functions.
    let rows = tx
        .query(
            "SELECT n.nspname, p.proname, pg_get_function_identity_arguments(p.oid), \
                    pg_get_function_result(p.oid), p.prosecdef \
             FROM pg_proc p \
             JOIN pg_namespace n ON n.oid = p.pronamespace \
             WHERE n.nspname = ANY($1)",
            &[&schemas],
        )
        .await?;
    for row in rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let arguments: String = row.get(2);
        let returns: String = row.get(3);
        let security_definer: bool = row.get(4);
        let key = format!("{schema}.{name}({arguments})");
        catalog.functions.insert(
            key,
            FunctionInfo {
                arguments: normalize_ws(&arguments),
                returns,
                security_definer,
            },
        );
    }

    Ok(catalog)
}

/// A diff between two catalog snapshots, expressed as flattened dotted
/// paths so any nested change (a column's default, a policy's `qual`, a
/// grant) is named precisely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CatalogDiff {
    pub added: BTreeMap<String, String>,
    pub removed: BTreeMap<String, String>,
    pub changed: BTreeMap<String, (String, String)>,
}

impl CatalogDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// A stable, human-readable rendering, one line per change, sorted.
    pub fn render(&self) -> String {
        let mut lines = Vec::new();
        for (path, value) in &self.added {
            lines.push((path.clone(), format!("+ {path} = {value}")));
        }
        for (path, value) in &self.removed {
            lines.push((path.clone(), format!("- {path} = {value}")));
        }
        for (path, (before, after)) in &self.changed {
            lines.push((path.clone(), format!("~ {path}: {before} -> {after}")));
        }
        lines.sort();
        lines
            .into_iter()
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Catalog {
    pub fn diff(a: &Catalog, b: &Catalog) -> CatalogDiff {
        let flat_a = flatten_value(&serde_json::to_value(a).expect("Catalog always serializes"));
        let flat_b = flatten_value(&serde_json::to_value(b).expect("Catalog always serializes"));

        let mut diff = CatalogDiff::default();
        for (path, value_a) in &flat_a {
            match flat_b.get(path) {
                None => {
                    diff.removed.insert(path.clone(), value_a.clone());
                }
                Some(value_b) if value_b != value_a => {
                    diff.changed
                        .insert(path.clone(), (value_a.clone(), value_b.clone()));
                }
                _ => {}
            }
        }
        for (path, value_b) in &flat_b {
            if !flat_a.contains_key(path) {
                diff.added.insert(path.clone(), value_b.clone());
            }
        }
        diff
    }
}

/// Flatten a JSON value into dotted-path -> stringified-leaf pairs.
/// Objects use `.key`; arrays use `[index]`. Empty containers are recorded
/// at their own path so an added/removed empty map or list still shows up.
fn flatten_value(value: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    flatten_into("", value, &mut out);
    out
}

fn flatten_into(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
    match value {
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                out.insert(prefix.to_string(), "{}".to_string());
                return;
            }
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_into(&key, v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                out.insert(prefix.to_string(), "[]".to_string());
                return;
            }
            for (i, v) in arr.iter().enumerate() {
                let key = format!("{prefix}[{i}]");
                flatten_into(&key, v, out);
            }
        }
        serde_json::Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn normalize_ws_collapses_and_trims() {
        assert_eq!(normalize_ws("  a\n  b\tc  "), "a b c");
    }

    #[test]
    fn diff_identical_catalogs_is_empty() {
        let a = Catalog::default();
        let b = Catalog::default();
        assert!(Catalog::diff(&a, &b).is_empty());
    }

    #[test]
    fn diff_detects_added_table() {
        let a = Catalog::default();
        let mut b = Catalog::default();
        b.tables
            .insert("public.widgets".to_string(), TableInfo::default());
        let diff = Catalog::diff(&a, &b);
        assert!(!diff.is_empty());
        assert!(diff
            .added
            .keys()
            .any(|k| k.starts_with("tables.public.widgets")));
    }

    #[test]
    fn diff_detects_changed_column_type() {
        let mut a = Catalog::default();
        let mut table = TableInfo::default();
        table.columns.insert(
            "id".to_string(),
            ColumnInfo {
                data_type: "integer".to_string(),
                not_null: true,
                default: None,
            },
        );
        a.tables.insert("public.widgets".to_string(), table.clone());

        let mut b = Catalog::default();
        table.columns.get_mut("id").unwrap().data_type = "bigint".to_string();
        b.tables.insert("public.widgets".to_string(), table);

        let diff = Catalog::diff(&a, &b);
        let key = "tables.public.widgets.columns.id.data_type";
        assert_eq!(
            diff.changed.get(key),
            Some(&("integer".to_string(), "bigint".to_string()))
        );
    }
}
