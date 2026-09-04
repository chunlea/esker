# 0064 — A materialized view is a table whose rows are recomputed

- Status: accepted
- Date: 2026-09-04
- Supersedes nothing. Extends [ADR 0031](0031-rails-compatibility-is-measured.md)'s rule that
  what gets built is what a capture shows.

## Context

`view_test.rb` includes the same nine-test module twice: once over `CREATE VIEW`, once over
`CREATE MATERIALIZED VIEW`. The second copy was nine of run 70's failures, under the shape
`CREATE MATERIALIZED VIEW is not supported`.

A view in this node is a stored `SELECT` **expanded where it is read**: `FROM v` is rewritten into
the derived table its definition stands for, and after that rewrite nothing in the planner can tell
a view from a sub-select somebody typed. That is the right implementation of a view and it is the
wrong implementation of a *materialized* view, which is defined by not doing it — a materialized
view answers from rows it computed earlier, and `REFRESH` is the statement that computes them again.

`captures/pg19_matview.txt` measures the difference in one line: an `INSERT` into the base table
leaves `count(*)` on the materialized view at 1, and the `REFRESH` after it makes the answer 2.

## What the capture forced

Four facts decided this more than any argument would have:

1. **`CREATE UNIQUE INDEX mv_ebooks_id ON mv_ebooks (id)` succeeds**, and `pg_matviews.hasindexes`
   becomes `t`. A materialized view is indexable.
2. **`REFRESH` works inside a transaction and rolls back with it** — so do
   `REFRESH … CONCURRENTLY` and the `CREATE` itself. Its rows are transactional exactly as a
   table's are.
3. **`pg_constraint` and `pg_index` are empty for a fresh one**: no primary key. `view_test.rb`'s
   `test_does_not_assume_id_column_as_primary_key` asserts precisely this.
4. **It is absent from `pg_views` and from `information_schema.tables`**, and present in
   `pg_matviews`. It is neither a view nor a base table to a client that asks the catalog.

Rows, indexes, transactionality and per-column types are the whole of what a table already is.

## Decision

**A materialized view is a `TableDef` that carries the `SELECT` it was built from.**

- `TableDef` gains `matview: Option<MatviewDef>`, holding the definition text and whether the
  relation has been populated. `Some` is what makes a table a materialized view; there is no second
  kind of relation and no second row store.
- The **name record stays `Relation::Table`**. A materialized view competes for names exactly as a
  table does — `CREATE MATERIALIZED VIEW` over an existing name is `42P07 relation "…" already
  exists`, measured — and every path that resolves a name to a table keeps working unchanged:
  reading it, indexing it, and the dependency check that makes `DROP TABLE` of its base `2BP01`.
- `relkind` is decided where the relation row is built, from `matview.is_some()`. So is
  `relispopulated`.
- `REFRESH` deletes the rows and re-runs the stored `SELECT` in the caller's transaction. Nothing
  else is needed for fact 2: transactionality is the storage layer's, not this statement's.

`CONCURRENTLY` is accepted and does the same work. PostgreSQL's version differs in *locking* —
it builds beside the old rows so readers are not blocked — and requires a unique index for it,
which this node checks and refuses without, with PostgreSQL's own message. The difference a client
can observe is concurrency, not results, and it is declared in the corpus rather than hidden.

## What this rules out, and why that is the point

A materialized view is **not** writable and not alterable: `INSERT`, `UPDATE` and `DELETE` are
`42809 cannot change materialized view "m"`, `TRUNCATE` is `42809 "m" is not a table`, and
`ALTER TABLE … ADD COLUMN` is `42809 … DETAIL: This operation is not supported for materialized
views.` Every one of those messages is copied from `captures/pg19_matview_guards.txt`, not written
from memory — the shape of the refusal is the whole of what a client sees, and three earlier units
in this lane refused things PostgreSQL accepts because the reason was reasoned about rather than
measured.

Because the relation *is* a table underneath, these guards are the only thing standing between a
client and a writable materialized view. They are therefore tested through the statements a user
sends and not through the function that writes them.

## Consequences

- Catalog record version **31**. A table stored at 30 or earlier reads back with `matview: None`,
  which is exactly what it was.
- `pg_class`, `pg_attribute` and `pg_index` answer for a materialized view with no new code: it is
  a table to all three, which is what PostgreSQL does too.
- `pg_views` and `information_schema.tables` need an explicit *exclusion*, because their filter was
  "every relation with a definition" and "every table" respectively.
- A `REFRESH` costs a full re-read of the definition and a full rewrite of the rows. Incremental
  maintenance is not attempted and is not planned; PostgreSQL does not do it either.
- The definition is stored as the parser renders it back, the same trade
  [`plan::CreateView`](../../crates/esker-sql/src/plan/ddl.rs) already documents for a view, and it
  carries the same declared divergence in `pg_matviews.definition`'s formatting.
