# rlsnap — snapshot testing for Postgres access control

## Problem Statement

Applications built on Supabase put the security boundary in the database: grants, column privileges, row-level security policies, and helper functions that policies call. That boundary is invisible to every testing layer the developer already has. Unit tests do not reach it, browser tests see a UI that looks fine, and the database reports nothing when a migration quietly widens or narrows access.

The same class of defect recurs across real projects: a new permission granted to every role by a default-grant pattern, a new column that no role can update because its column-scoped grant was never added, an INSERT whose `RETURNING` fails for lack of a SELECT policy, a "visible rows" helper that drifts from the policy function it is supposed to mirror, table privileges checked before RLS so a whole section is invisible, and reads silently truncated at the PostgREST row limit. Each one reaches users because nothing in the pipeline can see it, and each one costs a debugging pass to rediscover. On a multi-tenant platform this class is existential.

Today the only way to answer "who can do what, right now, on production" is to hand-inspect the schema, because migrations are often applied by hand and the migration ledger cannot be trusted.

## Solution

`rlsnap` is a single-binary CLI that connects to any Postgres database (local Supabase, preview, prod), impersonates a configured set of **personas** (role + JWT claims), and probes the full **access matrix**: every persona × every table × SELECT / INSERT / INSERT…RETURNING / UPDATE / DELETE, per column, plus EXECUTE on every function, plus the literal policy catalog. The result is a deterministic **snapshot** file committed to the repo. On every migration, CI re-snapshots and **diffs**: the PR shows exactly which cells changed, in a table a reviewer can read in ten seconds. Against prod it runs in a read-only "catalog" mode, so the same diff answers "which migrations are missing on prod".

Against a database with fixture data it additionally reports a **data layer**: visible row counts per persona, parity assertions between helper functions, and tables where a persona sees more rows than the PostgREST max so unpaginated reads would truncate.

It never commits. Every behavioural probe runs inside a transaction that ends in ROLLBACK, with statement and lock timeouts, so it is safe on a live database.

## User Stories

1. As an app developer, I want a one-command snapshot of who can do what on my database, so that I can review access changes the same way I review code changes.
2. As an app developer, I want the snapshot to be deterministic and stably ordered, so that committing it produces meaningful, minimal git diffs.
3. As an app developer, I want to define personas as a role plus JWT claims, so that `auth.uid()`, `auth.jwt()`, and my own helper functions see exactly what they would see for a real user.
4. As an app developer, I want a persona to optionally run setup SQL inside the probe transaction, so that I can model "a staff user who has just been granted permission X" without polluting the database.
5. As an app developer, I want built-in personas for `anon`, `authenticated`, and `service_role`, so that the Supabase defaults are covered without configuration.
6. As an app developer, I want per-column SELECT and UPDATE privilege results, so that a new column missing its column-scoped grant shows up as a denied cell instead of a broken UI.
7. As an app developer, I want INSERT probes to distinguish "denied by table privilege", "denied by RLS policy", and "passed permission checks but hit a constraint", so that I know which layer blocked the write.
8. As an app developer, I want INSERT…RETURNING probed separately from INSERT, so that a missing SELECT policy is caught before a user's save button fails.
9. As an app developer, I want the RLS policy catalog (name, command, roles, USING, WITH CHECK, enabled/forced flags) captured per table, so that a policy text change is visible in the diff even when outcomes happen not to change for my fixture personas.
10. As an app developer, I want EXECUTE privilege on every function in my app schemas captured per persona, so that a new RPC exposed to the wrong role is caught.
11. As an app developer, I want to mark a permission list as "governance" and be told when a newly created permission is granted to a role outside that list, so that the auto-grant-to-all-staff class cannot recur.
12. As an app developer, I want parity assertions between two SQL expressions evaluated per persona, so that a viewable-set helper and the policy function it mirrors cannot drift apart silently.
13. As an app developer, I want a warning when a persona can see more rows in a table than the PostgREST max-rows setting, so that I know which reads must be paginated.
14. As an app developer, I want a `check` command that snapshots and diffs against the committed baseline in one step with a non-zero exit code on change, so that CI fails the PR.
15. As an app developer, I want an `accept` command that overwrites the baseline, so that intended changes are a one-line commit.
16. As an app developer, I want a human-readable diff table and a machine-readable JSON diff, so that both reviewers and agents can consume it.
17. As an app developer, I want an `explain` command for a single cell that lists the grants and policies that produced it, so that a surprising diff is actionable without opening psql.
18. As an app developer, I want connection strings supplied via environment variables and never stored in config, so that the config file is safe to commit.
19. As an app developer, I want named targets (local, preview, prod) in config, so that `rlsnap check --target prod` is the whole command.
20. As an app developer, I want prod targets to default to catalog mode (no DML, privilege functions and catalog reads only), so that I can point the tool at production without a second thought.
21. As an app developer, I want to opt a target into behavioural mode explicitly, so that local and preview databases get the full probe set.
22. As an app developer, I want every behavioural probe wrapped in a transaction that always rolls back, with statement and lock timeouts set locally, so that the tool cannot mutate or block a live database.
23. As an app developer, I want the tool to refuse to run if it detects it is not inside a transaction it controls, so that a pooler or proxy misconfiguration cannot turn probes into writes.
24. As an app developer, I want to diff two snapshot files directly, so that I can compare preview against prod and see which migrations have not been applied.
25. As an app developer, I want to include or exclude schemas and tables by pattern, so that Supabase internal schemas and noisy tables stay out of the snapshot.
26. As an app developer, I want the snapshot to carry a schema version and the tool version, so that format changes are detected instead of producing a confusing diff.
27. As an app developer, I want probes run concurrently across personas with a bounded connection pool, so that a 60-table, 6-persona database snapshots in seconds.
28. As an app developer, I want an `init` command that writes a starter config with the Supabase default personas and sensible excludes, so that first use is under five minutes.
29. As an app developer, I want clear failure when a persona's role does not exist or claims are malformed, so that misconfiguration is not silently recorded as "denied everywhere".
30. As a reviewer, I want the diff to group changes by table and show persona, operation, column, before, after, so that I can judge an access change without reading SQL.
31. As a reviewer, I want data-layer changes (row counts, parity, cap warnings) reported separately from privilege-layer changes, so that fixture data churn does not hide a real grant change.
32. As a reviewer, I want unchanged cells omitted from the diff by default, so that a large matrix stays readable.
33. As a CI pipeline, I want a single static binary with no runtime dependencies, so that it installs with one download in any job.
34. As a CI pipeline, I want distinct exit codes for "no change", "changes found", and "tool or connection error", so that the job can fail for the right reason.
35. As a CI pipeline, I want the JSON diff to be stable and documented, so that it can be posted as a PR comment by a separate step.
36. As an AFK agent, I want `rlsnap check` to be cheap and deterministic, so that I can run it after every migration I write and read the diff as my review surface.
37. As an AFK agent, I want `rlsnap explain` output to name the policy or grant responsible, so that I can fix the migration rather than guess.
38. As a security reviewer, I want to snapshot prod in catalog mode on a schedule and diff against the repo baseline, so that out-of-band changes to grants or policies are detected.
39. As a multi-tenant platform builder, I want personas that differ only by tenant claim, so that a diff between them exposes cross-tenant visibility.
40. As a multi-tenant platform builder, I want a disjointness assertion of the form "rows visible to tenant A ∩ rows visible to tenant B = ∅" per table, so that tenant isolation is a committed, CI-checked property.
41. As a maintainer of the tool, I want every known incident class encoded as a fixture pair (schema before, migration after) with an expected diff, so that the test suite is the catalogue of what the tool catches.
42. As a maintainer of the tool, I want a test proving a full behavioural run leaves a database byte-identical (excluding sequence values), so that the no-mutation promise is verified, not asserted.
43. As a maintainer of the tool, I want a benchmark against a realistic schema size, so that the README's speed claim is reproducible.

## Implementation Decisions

**Language and shape.** Rust, single static binary, async Postgres client, TOML config, JSON snapshot.

**Workspace layout.** A cargo workspace from the first commit: a library crate holding the reusable core (target/connection handling from env, the always-ROLLBACK transaction wrapper with local timeouts, persona impersonation, the SQLSTATE outcome classifier) and a thin binary crate for `rlsnap` itself. Nothing Postgres-touching lives in the binary crate. This keeps the door open for sibling tools (migration rehearsal, a guarded read-only MCP server) to share the core later without committing to a kit-style product name now. Subcommands: `init`, `snapshot`, `diff`, `check`, `accept`, `explain`. Clap for CLI, serde for config and snapshot.

**Config (`rlsnap.toml`).** Committed to the repo. Contains: `targets` (name → mode, env var holding the connection string, PostgREST max-rows), `personas` (name → role, claims object, optional setup SQL), `schemas` include list (default `public`, plus any app schema), table include/exclude globs, `governance` permission list (optional), `asserts` (named pairs of SQL expressions evaluated per persona, or a tenant-disjointness assert naming a table list). Connection strings come only from the named environment variable; the tool refuses a literal URL in config.

**Persona impersonation.** Inside each probe transaction: `SET LOCAL ROLE <role>`, then `set_config('request.jwt.claims', <claims JSON>, true)` and the individual `request.jwt.claim.<key>` settings Supabase helpers read. Built-in personas `anon`, `authenticated`, `service_role` are always present unless disabled. Persona setup SQL runs after impersonation is configured but as the connecting superuser/owner role before `SET LOCAL ROLE`, so it can grant permissions; it is rolled back with everything else.

**Two probe modes per target.**
- *Catalog mode* (prod default): no DML. Uses `has_table_privilege`, `has_column_privilege`, `has_function_privilege` for each persona role, and reads `pg_policies`, `pg_class.relrowsecurity / relforcerowsecurity`. Safe under any pooler mode because it issues only reads.
- *Behavioural mode* (local/preview default): everything in catalog mode, plus per persona × table: `SELECT count(*)`; per column `SELECT col LIMIT 0`; per column `UPDATE t SET col = col WHERE false`; `DELETE WHERE false`; `INSERT DEFAULT VALUES` and `INSERT DEFAULT VALUES RETURNING *`, each in its own SAVEPOINT. Requires a session-mode or direct connection; the tool verifies it is inside its own transaction (checks `txid_current_if_assigned` / transaction status) and aborts otherwise.

**Outcome classification.** Each cell records one of: `allowed`, `denied_privilege` (SQLSTATE 42501, message names table/column permission), `denied_rls` (42501, message names row-level security), `constraint` (SQLSTATE class 23, meaning privilege and RLS WITH CHECK both passed because Postgres evaluates RLS before constraints on INSERT), `error` (anything else, with SQLSTATE). `WHERE false` probes exercise privilege checks only, never RLS row evaluation, and the snapshot labels them as such.

**Snapshot format.** JSON, one document, versioned (`format: 1`), with tool version and target mode recorded. Top-level sections: `privileges` (persona → table → operation → column? → outcome), `policies` (table → list of normalised policy records; USING/WITH CHECK stored verbatim, whitespace-normalised), `functions` (persona → function signature → execute outcome), `data` (present only in behavioural mode: row counts, assert results, cap warnings). All maps sorted by key; arrays sorted by a documented key. No timestamps inside the snapshot.

**Diff semantics.** Privilege, policy, and function sections diff cell-by-cell and are the basis of the exit code. The data section is reported but does not affect the exit code unless `--strict-data` is passed. Governance check and asserts are evaluated at snapshot time and recorded as findings; a finding appearing or disappearing is a diff. Exit codes: 0 no change, 1 changes or findings, 2 error.

**Safety invariants.** Behavioural probes run with `SET LOCAL statement_timeout` and `SET LOCAL lock_timeout` (configurable, short defaults). The tool never sends COMMIT; the only transaction terminator in the codebase is ROLLBACK, enforced by a test that greps the binary's SQL strings and by the byte-identical integration test. Known residue: identity/serial sequences advance on rolled-back INSERT probes; documented, and INSERT probes can be disabled per target.

**Concurrency.** One connection per persona from a bounded pool (default 4); tables probed sequentially within a persona's transaction. Results merged and sorted before serialisation so concurrency never affects output.

**Explain.** For a given persona/table/operation, re-queries the catalog: table and column ACLs relevant to the persona's role (including via role membership), policies whose `roles` include the persona role or `public` and whose `cmd` matches, and prints them with the cell's outcome.

**Prior-art boundaries.** Splinter (Supabase advisors) is static lint; pgTAP and supabase-test-helpers are hand-written per-policy tests. `rlsnap` is the behavioural matrix plus diff; it does not duplicate Splinter's rules.

## Testing Decisions

**What makes a good test here.** A test drives the real binary (or the single library entry `run(args, env)` that the binary is a thin wrapper around) against a real Postgres, and asserts on the snapshot JSON, the diff output, and the exit code. Postgres is never mocked: the entire value of the tool is its fidelity to Postgres's privilege and RLS semantics, and those semantics are the thing under test.

**Seam.** One seam: the CLI entry. Every test is "given this schema, these personas, this config, run `rlsnap <subcommand>` and assert on output". Pure functions (SQLSTATE classifier, diff algorithm, snapshot sort) get small unit tests because they are trivially isolated, but no test reaches into probe internals.

**Fixture catalogue.** An integration test directory where each case is a pair of SQL files (`before.sql`, `after.sql`), a personas config, and an expected diff. The initial catalogue is the incident list: governance auto-grant, missing column grant on a new column, INSERT allowed but RETURNING denied, helper/policy parity drift, table privilege blocking before RLS, persona sees > max-rows, tenant disjointness violated, policy text change with no outcome change, function newly executable by anon. Each runs on a fresh database (testcontainers Postgres with the Supabase `anon`/`authenticated`/`service_role` roles created in a bootstrap script; a local Supabase instance via `DATABASE_URL` is an accepted alternative for speed).

**No-mutation proof.** A test that dumps the fixture database, runs a full behavioural snapshot with every persona, dumps again, and asserts equality excluding sequence `last_value`. A second test asserts the only transaction-terminating statement issued on the wire is ROLLBACK (captured via a logging proxy or `log_statement=all` on the container).

**Determinism.** Run the same snapshot twice with different pool sizes and assert byte-identical output.

**Prior art.** There is no codebase yet. Conceptual prior art is the common production-verification technique of executing inside a DO block that ends in `raise exception`, which guarantees rollback while returning a result in the error message. The fixture catalogue plays the same role here as a migration regression suite.

## Out of Scope

- Executing function bodies (only EXECUTE privilege is probed). Calling RPCs with generated arguments is a later version.
- PostgREST HTTP-level probing (the max-rows warning is derived from SQL counts, not by calling the API).
- UI-layer verification (Playwright, persona logins through the app).
- Auto-fixing or generating migrations from a diff.
- Splinter-style static lints (RLS disabled, permissive `true` policies). Those are already in the Supabase dashboard.
- Non-Supabase JWT conventions beyond arbitrary `request.jwt.claims` JSON.
- Databases other than Postgres 15+.
- A hosted service, dashboard, or GitHub App; the PR-comment step is a separate action that consumes the JSON diff.
- Column-level RLS row evaluation for UPDATE/DELETE (the `WHERE false` probes test privilege only; row-level evaluation needs fixture rows and is a later version).

## Further Notes

- Name is provisional; confirmed free on crates.io and GitHub at spec time.
- Success criterion for v1: every incident class in the fixture catalogue, replayed as a migration against a realistic Supabase schema, produces a diff line that names the problem.
- A second v1 success criterion: snapshot a production database in catalog mode, diff it against a staging snapshot, and obtain the list of migrations not yet applied to production.
- Multi-tenant disjointness asserts are in v1 because they are cheap once parity asserts exist and they are the highest-value check for multi-tenant schemas.
- README should open with the incident list and the diff example, not with installation.
