# rlsnap

Snapshot testing for Postgres access control.

Probes every persona × table × column × operation on a Supabase/Postgres database, writes a deterministic snapshot, and diffs it in CI — so a migration that widens or narrows access shows up as a reviewable diff, not a client bug report.

Status: workspace scaffolding in place; `rlsnap` and `rehearse` are not implemented yet (they exit 2 with "not implemented"). See `docs/rlsnap-spec.md` for the full design. See issue #1.

## Workspace layout

This is a cargo workspace with three crates:

- **`crates/pgcore`** — the shared library. Config loading (`rlsnap.toml` / `pgkit.toml`), the always-rolls-back probe transaction (`RollbackTx`), persona impersonation, SQLSTATE outcome classification, catalog snapshotting + diffing, and a quote-and-comment-aware SQL statement splitter. This crate is frozen: `rlsnap` and `rehearse` build on it without editing it.
- **`crates/rlsnap`** — the snapshot/diff CLI described in the spec. Currently a stub.
- **`crates/rehearse`** — a sibling tool for migration rehearsal, sharing the same core. Currently a stub.

## Running tests

Integration tests drive real Postgres — nothing in this repo mocks the database. You need a Postgres 16 server reachable at the URL in `PG_TEST_URL` (default `postgres://postgres:postgres@127.0.0.1:54390/postgres`, superuser access required so tests can create/drop databases and roles).

Start one locally with:

```sh
scripts/test-db.sh start   # starts a postgres:16 container on port 54390
scripts/test-db.sh stop    # tears it down
```

Then:

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

Each integration test creates its own uniquely-named database on the shared server (`CREATE DATABASE`), bootstraps the Supabase-style `anon` / `authenticated` / `service_role` roles if they don't already exist cluster-wide, loads fixture SQL, runs the assertions, and drops the database again. Tests never rely on a fixed database name and run safely in parallel against the same server.

CI (`.github/workflows/ci.yml`) runs `fmt --check`, `clippy -D warnings`, and `cargo test --workspace` against a `postgres:16` service container on port 54390.

## The ROLLBACK-only invariant

`pgcore` never commits. `RollbackTx` issues `BEGIN`, sets `statement_timeout` and `lock_timeout` local to the transaction, and guarantees the transaction ends in `ROLLBACK` — via the documented `finish()` call, or as a best-effort fallback in `Drop` if a caller forgets. Every error path still rolls back. This is enforced both by convention (the only transaction-terminating statement anywhere in `src/` is `ROLLBACK` — grep for `COMMIT` and you'll only find it in the comment explaining this rule) and by an integration test that proves a full probe transaction leaves the target database byte-for-byte unchanged.

This is what makes it safe to point `rlsnap`/`rehearse` at a live database, including production: the tool can read anything and probe anything, but it can never write.
