# rlsnap

Snapshot testing for Postgres access control.

Probes every persona × table × column × operation on a Supabase/Postgres database, writes a deterministic snapshot, and diffs it in CI — so a migration that widens or narrows access shows up as a reviewable diff, not a client bug report.

Status: spec stage. See issue #1.
