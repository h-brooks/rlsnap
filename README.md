# rlsnap

**Snapshot testing for Postgres access control.** Probe every persona × table × column × operation, commit the snapshot, diff it in CI.

[![CI](https://github.com/h-brooks/rlsnap/actions/workflows/ci.yml/badge.svg)](https://github.com/h-brooks/rlsnap/actions/workflows/ci.yml)

## Why this exists

If your security boundary lives in the database (grants, column privileges, row-level security) then it is invisible to every testing layer you already have. Unit tests don't reach it. Browser tests see a UI that looks fine. The database says nothing when a migration quietly widens or narrows access.

The same defects keep shipping, everywhere RLS is used:

- a migration adds a column, and **nobody re-grants the column-scoped UPDATE**, so the UI write breaks for exactly one role
- `INSERT` works but **`INSERT … RETURNING` fails**, because there's no SELECT policy, so the save button errors after the row is in
- a default-grant pattern hands a **new permission to every role**
- a "visible rows" helper **drifts from the policy** it's supposed to mirror
- a function becomes **executable by `anon`** and nobody notices
- table privileges are checked **before** RLS, so a revoke blanks out a whole section

None of these fail a test. All of them fail a diff:

```
$ rlsnap diff before.json after.json
== table: public.widgets ==
persona          op        column     before      after
authenticated    update    id         allowed     denied_privilege
authenticated    update    price      -           denied_privilege
authenticated    update    tenant     allowed     denied_privilege
tenant_a         select    price      -           allowed

exit code 1
```

## How it works

`rlsnap snapshot` connects to any Postgres (Supabase-first: personas are a role plus `request.jwt.claims`), and probes the full access matrix:

- **catalog mode** (the default for prod targets): `has_table_privilege` / `has_column_privilege` / `has_function_privilege` per persona, plus the literal policy catalog from `pg_policies`. Zero DML and no user-supplied SQL, safe to point at production.
- **behavioural mode** (local/preview): real probes: `SELECT count(*)`, per-column `SELECT … LIMIT 0` and `UPDATE t SET col = col WHERE false`, `INSERT DEFAULT VALUES` and `INSERT … RETURNING`, each in its own savepoint, all inside a transaction that **always rolls back**. Outcomes are classified by SQLSTATE: `denied_privilege` vs `denied_rls` vs `constraint` (which proves the permission layer passed: Postgres evaluates RLS `WITH CHECK` before constraints).

The snapshot is deterministic JSON (sorted, timestamp-free, byte-identical across runs and pool sizes). Commit it. `rlsnap check` re-probes and diffs against the baseline; exit 1 on any change. `rlsnap accept` re-baselines after review. `rlsnap explain <persona> <table> <op>` shows the grants and policies behind one cell.

Custom checks generalise into `[[asserts]]`: a named SQL query that must return zero rows, optionally evaluated *inside each persona's impersonated transaction*: tenant disjointness, helper/policy parity, governance allow-lists.

## Install

```
cargo install --git https://github.com/h-brooks/rlsnap rlsnap
```

## Quick start

```
rlsnap init                      # writes rlsnap.toml (personas, targets, excludes)
export RLSNAP_LOCAL_URL=postgres://postgres:postgres@127.0.0.1:54322/postgres
rlsnap snapshot --target local --out rlsnap.snap.json   # commit this
rlsnap check --target local      # CI: exit 0 clean / 1 changes / 2 error
```

Connection strings come only from environment variables; a literal URL in the config is a hard error, so the file is safe to commit.

## Safety invariants

- The only transaction terminator this workspace ever sends is `ROLLBACK`, enforced by a workspace-wide test, and by an integration test proving a full behavioural run leaves the database byte-identical.
- `SET LOCAL statement_timeout` / `lock_timeout` on every probe transaction.
- Known residue: rolled-back `INSERT` probes still advance identity/serial sequences (Postgres doesn't undo that). Documented; `insert_probes = false` per target if it matters.

## Sibling: `rehearse`

Same workspace, same core: run a **migration** inside an always-rolled-back transaction, and get the report: per-statement timing, which relations would be locked and how hard, in-transaction write counts, the access-control catalog diff (tables, columns, grants, policies, function definitions: not sequences, types, or extensions), and the exact failing statement. It refuses `COMMIT`/`END` inside migration files, so a stray transaction terminator can't turn a rehearsal into a deploy, and it refuses statements that cannot run inside a transaction at all (`CREATE INDEX CONCURRENTLY`, `VACUUM`, `ALTER SYSTEM`, and friends) with a named error instead of a confusing mid-run failure.

Be clear about what a rehearsal is: the migration REALLY executes, and every lock it takes (including `ACCESS EXCLUSIVE` from most `ALTER TABLE` forms) is held until the rollback. On a database serving live traffic that blocks readers and writers for the duration of the rehearsal, even though nothing persists. Rehearse against a disposable branch, a restored clone, or a staging copy; point it at production only when you have decided that holding those locks briefly is acceptable. `lock_timeout` defaults to 5s so a rehearsal never queues behind traffic indefinitely.

`rehearse drift` diffs two databases' schemas: "which migrations are missing on prod" as one command (catalog reads only, no DML).

```
$ rehearse run 20260818_add_sku.sql --target staging
== Locks ==
public.widgets: AccessExclusiveLock  (would block reads/writes)
== Writes ==
public.widgets: ins=0 upd=3 del=0
== Schema changes ==
+ tables.public.widgets.columns.sku.data_type = text
+ tables.public.widgets.indexes.widgets_sku_idx = CREATE INDEX ...
```

## What it doesn't do

It doesn't tell you whether a grant is *correct*, only that it *changed*. You review the diff the way you review code. Detection is mechanical; judgment stays with you. Static lints (RLS off as a lint rule, `USING (true)`) are already in Supabase's dashboard advisors; rlsnap doesn't duplicate them; it tests behaviour.

The `WHERE false` probes exercise the privilege layer, not row-level policy evaluation: they prove who may attempt an operation, not which rows a persona can actually touch. For "tenant A cannot modify tenant B's rows" style guarantees on real data, pair rlsnap with targeted [pgTAP](https://pgtap.org/) tests (Supabase's recommended approach) or use rlsnap's `[[asserts]]` with `--with-rows` on a fixture database.

## As a gate for agent-generated migrations

rlsnap earns the most in pipelines where an agent writes the migration. The order that works:

1. agent writes the migration
2. `rehearse run` against a disposable database: timings, locks, catalog diff
3. apply to the disposable database
4. `rlsnap check`: the access-matrix diff is the review surface
5. targeted pgTAP for row-level isolation where it matters
6. a human reads the diff and runs `rlsnap accept`

Rule six is the point: `rlsnap accept` is a human act. An implementing agent that can re-baseline its own access-control changes has quietly removed the gate.

## License

MIT
