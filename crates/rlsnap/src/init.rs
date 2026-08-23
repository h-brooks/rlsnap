//! `rlsnap init`: write a starter `rlsnap.toml`.

use std::path::Path;

use anyhow::{bail, Context, Result};

pub const STARTER_TOML: &str = r#"# rlsnap config. Connection strings come only from the named environment
# variable in `url_env` -- never put a literal URL here, this file is meant
# to be committed.
#
# NOTE: top-level keys (schemas, excludes, personas, asserts) must come
# before the first `[targets.*]` table -- TOML would otherwise fold a bare
# `key = value` line into whichever table header precedes it.

# Schemas to include in the snapshot.
schemas = ["public"]

# Tables/functions to leave out of the snapshot (glob patterns matched
# against "schema.name"). These are the usual Supabase-managed internals.
excludes = [
    "auth.*",
    "storage.*",
    "extensions.*",
    "graphql.*",
    "graphql_public.*",
    "pgbouncer.*",
    "pgsodium.*",
    "realtime.*",
    "supabase_functions.*",
    "vault.*",
]

[targets.local]
url_env = "RLSNAP_LOCAL_URL"
mode = "behavioural"

[targets.preview]
url_env = "RLSNAP_PREVIEW_URL"
mode = "behavioural"

[targets.prod]
url_env = "RLSNAP_PROD_URL"
# mode defaults to "catalog" (privilege + policy catalog only, no DML) --
# safe to point at production without a second thought. Opt in to
# "behavioural" explicitly once you're comfortable.

# `anon`, `authenticated`, and `service_role` personas are always included
# with no claims beyond Supabase's own default `role` claim. Add your own
# personas here -- each one probes the matrix as that role with those JWT
# claims set via `request.jwt.claims`.
#
# [[personas]]
# name = "staff"
# role = "authenticated"
# claims = { sub = "00000000-0000-0000-0000-000000000001", app_role = "staff" }
#
# [[personas]]
# name = "tenant_a"
# role = "authenticated"
# claims = { sub = "00000000-0000-0000-0000-0000000000a1", tenant_id = "tenant-a" }

# Named assertions, evaluated at snapshot time on any behavioural target
# (never in catalog mode). Each `sql` query must return zero rows to pass; a
# query returning any rows is a finding. `per_persona = true` runs the query
# inside each persona's own impersonated transaction (for e.g. tenant
# disjointness or a helper/policy parity check); `per_persona = false` runs
# it once, unimpersonated.
#
# [[asserts]]
# name = "tenant_disjointness"
# sql = "SELECT 1 FROM widgets WHERE tenant_id <> current_setting('request.jwt.claim.tenant_id', true)"
# per_persona = true

# `insert_probes` (per target, default true): behavioural mode's INSERT and
# INSERT...RETURNING probes always roll back, but a rolled-back insert still
# advances any identity/serial sequence it touched -- Postgres does not undo
# that on ROLLBACK. Table data and privileges are unaffected, only sequence
# values move. Set `insert_probes = false` on a target if that residue is a
# problem (e.g. a preview database whose sequence values are asserted on
# elsewhere).
#
# [targets.preview]
# url_env = "RLSNAP_PREVIEW_URL"
# mode = "behavioural"
# insert_probes = false
"#;

pub fn run_init(dir: &Path) -> Result<()> {
    let path = dir.join("rlsnap.toml");
    if path.exists() {
        bail!("{} already exists, refusing to overwrite", path.display());
    }
    std::fs::write(&path, STARTER_TOML)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}
