# ADR 0054 — A temporary table is a relation in a schema that belongs to one session

Status: accepted · Date: 2026-09-03 · Phase 9 (Rails compatibility), the temp-table unit

## Context

`CREATE TEMPORARY TABLE` has been `0A000` since the first slt file. The gap costs less in the
suite than it looks — run 56 has exactly **one** test stopped by it,
`UnloggedTablesTest#test_gracefully_handles_temporary_tables`, and the 16-test
`InFailedSqlTransaction` row beside it is caused by `tsvector` (51), `tsrange` (46), `DO` (34) and
`SET SESSION` (12), not by this. What it costs in *measurement* is larger and already written
down: `tests/relation_resolution.rs` asserts that **20 statements** of
`pg19_relation_resolution.txt` are swallowed by the transaction this refusal aborts, so a third of
that capture has never been compared with anything.

A temporary table is four rules, and only the first is about tables:

1. It is visible to the session that made it and to no other.
2. It is found **before** a permanent relation of the same name, and the permanent one is still
   there and still reachable by qualifying it.
3. It disappears when the session ends — including when the session ends abruptly.
4. It is never in a table list or a schema dump.

## Decision

**A temporary table is an ordinary relation in a schema named `pg_temp_<n>`, and nothing else is
new.** No key space, no second catalog, no change to `esker-keys` — the same shape ADR 0052 took
for databases, and for the same reason: the mechanism that already exists is the right one.

* **Its rows live where any table's rows live.** A temp table is allocated a table id like any
  other, so its rows are the ordinary `'t' ++ tenant ++ table_id ++ 'r'` range, written through
  Percolator, read through MVCC, encoded by the row codec. Dropping it drops them the ordinary
  way. The engine and Raft never learn that anything is temporary (invariant 7).
* **Its catalog records are ordinary records**, under the schema-qualified stored name
  `pg_temp_<n>` ++ NUL ++ `name` (`catalog::SCHEMA_SEPARATOR`). So it is transactional for free: a
  temp table created in a transaction that rolls back is gone, and one created under a savepoint
  that is rolled back is gone — both measured, and neither needs a line of code.
* **Its schema is a schema record**, written on demand the first time the session makes a temp
  relation. `pg_namespace` therefore reports it without being taught to, `DROP SCHEMA` is what
  reclaims it, and `CREATE SCHEMA pg_temp_4` is refused for its prefix like any other `pg_` name
  (`42939`, measured in the namespace unit).
* **`<n>` comes from the tenant's id allocator**, not from a process-local counter. Two
  `esker-sql` nodes serve one tenant; a per-process number would give `pg_temp_1` to a session on
  each of them and they would silently share a schema. The allocator's ids are cluster-unique and
  monotonic, so a temp schema name is never reused by anybody. That is stronger than PostgreSQL,
  whose `pg_temp_<backend id>` **is** reused — and the difference is the whole of what makes
  reclamation harder here, which the consequences say.
* **Resolution is the search path**, which already exists: the session's own temp schema is
  pushed to the front of `resolved_search_path`, which is where the shadowing rule comes from for
  free — a bare name finds the temp relation and `public.x` finds the permanent one, in the code
  that already resolves `search_path` entries. `current_schemas(true)` gains it and
  `current_schemas(false)` does not, which is exactly PostgreSQL's split and is what keeps a temp
  table out of `ActiveRecord`'s `tables()` and out of a schema dump **without a rule of its own**.
  Measured: with a temp table, the implicit path is 3 long and the explicit path is 1.
* **`pg_temp` with no number is the session's own**, resolved at lowering to the same stored
  qualifier, so `DROP TABLE pg_temp.x` and `FROM pg_temp.x` name what the session made.
* **`relpersistence` is `t`**, a third `catalog::Persistence` variant. `relkind` stays `r` — a
  temp table is a table (measured), which is why `ActiveRecord`'s `table_exists?` finds it and
  its `tables()` does not.
* **`ON COMMIT` is a field of the table record**, three values with `PRESERVE ROWS` the default.
  `DELETE ROWS` empties the table at every commit — including the implicit commit of a statement
  outside a transaction block, which is why a plain `INSERT` into such a table leaves **zero**
  rows behind (measured, and the fact this is easiest to get wrong). `DROP` drops the table at the
  same instant. `ON COMMIT` on a permanent table is `42P16`, measured.

## What a crash leaves behind, and how it is reclaimed

A session that ends normally drops its temp schema and everything in it, in one transaction, on
the way out. A session that ends abruptly — a killed connection, a killed node — cannot, so the
schema record, the relation records and the rows stay.

**They are unreachable, which is the property that makes this safe rather than merely untidy.** A
temp schema is in exactly one session's search path and that session is gone; the number is never
allocated again, so no future session can inherit it; and nothing else can name the relations,
because an unqualified name resolves along a path the schema is not in and a qualified one has to
spell a number nobody knows. What is left is space, not a wrong answer.

Reclaiming that space needs one fact this node does not have: **which sessions are live**. That is
the same registry `DROP DATABASE` wants in order to refuse a database another session is
connected to, and the same one `pg_stat_activity` wants in order to report more than the asking
backend (ADR 0052, and `tests/relation_resolution.rs`). So the sweeper is deliberately *not*
invented here — a sweep that guessed which schemas were dead would delete a live session's tables,
which is worse than leaking. Recorded as a debt with a measure: the count of `pg_temp%` rows in
`pg_namespace` with no live session, which is zero on a node that has never lost one.

PostgreSQL reclaims by reuse: a new backend takes an old backend's number and truncates whatever
it finds. That option was considered and rejected here for the reason the decision gives — with
several nodes on one tenant, a reusable number is a number two live sessions can hold at once, and
the failure mode is not a leak but two sessions sharing a table.

## Consequences

* **A `CREATE TEMP TABLE` bumps the catalog version**, which invalidates every session's cached
  `TableDef` on every node. That is the price of catalog records being one space, and it is paid
  by sessions that have nothing to do with the temp table. Acceptable at the rate a suite makes
  them; a node whose workload is temp tables would want a second catalog space, which is exactly
  the design this ADR did not take. Named so that the reversal has somewhere to start.
* **A temp table is durable.** Its rows go through the WAL and Raft like anything else, so this
  node pays full write cost for data defined to be throwaway, and a `kill -9` leaves the rows on
  disk until the debt above is paid. PostgreSQL keeps them in local buffers and unlinks the files.
  The saving is an engine decision, the same one `UNLOGGED` is waiting for (`catalog::Persistence`)
  — and skipping the log for one would be a durability change (invariant 1) rather than a catalog
  one.
* **A temp schema is visible in `pg_namespace` to every session**, as it is on a real server.
  `ActiveRecord`'s `schema_names` filters `nspname !~ '^pg_.*'`, so this costs nothing — and it is
  the same filter that made the `pg_catalog` unit free.
* **`information_schema.tables` lists a temp table under its temp schema** on a real server, and
  does not here, for the reason the namespace unit already declared: this node's version hardcodes
  `table_schema` to `public`.
* The number in `pg_temp_<n>` is an allocator id and not a backend id, so it is larger and
  sparser than a real server's. Nothing measured reads the number: every probe in the corpus asks
  `nspname LIKE 'pg_temp%'`, which is what a capture *must* do — the name carries session state
  and cannot be in a corpus.
