# 0049 — A comment is a field of the object it is about

Status: accepted (phase 9, `b4-types`)

## Context

`COMMENT ON TABLE t IS '…'` was `0A000` naming itself, and `obj_description`/`col_description`
answered NULL for everything — right, but only because nothing could ever set a comment.
`ActiveRecord` writes both a table's and a column's comment into its schema dump and reads them
back through those two functions (boot statements 29 and 32), so the pair had to become real.

PostgreSQL keeps every comment in one catalog table, `pg_description`, keyed by
`(classoid, objoid, objsubid)` — the catalog the object lives in, the object, and a sub-object
number that is a column's attnum and `0` for the object itself. The rows are removed by the
dependency machinery when the object goes away.

## Options

1. **A `pg_description`-shaped record**, keyed the same way, in the `'m' ++ "sql"` space. It is what
   a real server does and it is what a `pg_description` *view* would read directly. The cost is
   that every rule about a comment's lifetime becomes code: a `DROP TABLE` has to delete a range of
   comment records, a `DROP COLUMN` one record, a `RENAME` none — and each of those is a place to
   forget one, leaving a comment that outlives its object and reappears on the next object to take
   that id.
2. **A field on the object.** The table record grows the table's comment, its primary key's, one per
   column and one per index. Chosen.

## Decision

Catalog record **version 21** appends the comments to the table record, at the end like every
section before it: the table's, the primary key's, then one per column and one per index, in that
order (`crate::catalog::record`).

**The field is the dependency.** A `RENAME` rewrites the record and the comment travels with it; a
`DROP COLUMN` removes the column and its comment with it; a `DROP TABLE` takes all of them. None of
that is written anywhere, and none of it can be forgotten — which was the whole argument against
option 1, whose correctness would have had to be re-established at every DDL verb.

The primary key's comment is the one that does not fit the pattern and is worth naming: there is no
index behind a primary key here — the row key *is* the key — and yet `t_pkey` is a relation a client
can name and comment on. Its comment lives on the table (`TableDef::primary_key_comment`) because
there is no other record to put it in.

**An empty string means no comment, and needs no flag.** `COMMENT ON … IS ''` deletes the
`pg_description` row on a real server exactly as `IS NULL` does, so a comment that *is* the empty
string cannot be observed there either. Measured. The section therefore costs one byte per object
on a table nobody has commented.

## Consequences

- `obj_description(oid)`, `obj_description(oid, catalog)` and `col_description(oid, attnum)` answer
  from the record. Nothing found is NULL and never an error — an uncommented object, an attnum out
  of range, a negative one, an oid that names nothing, an unknown catalog name — which is what makes
  a `LEFT JOIN` over them work. `col_description(oid, 0)` returns the **table's** comment, because
  `pg_description` keys it as `objsubid = 0` and a real server's function does not filter it out.
- **`pg_description` is not a view here**, and a query against it is `42P01`. The comments are
  fields rather than rows, so a view over them is a join of its own — a small unit, with its shape
  already captured in `tests/corpus/pg19_comment.txt`.
- An `EXCLUDE` constraint's index is synthesised from the constraint rather than stored
  (`RelKind::Exclusion`), so it has no record to keep a comment in and `COMMENT ON INDEX` over one
  is refused by name.
- `COMMENT ON SEQUENCE` and `COMMENT ON VIEW` are taken by the parser **so that the kind can be
  reported**: a real server resolves the name first, so `COMMENT ON SEQUENCE <a table>` is
  `42809 "t" is not a sequence` there and not a refusal of the statement. A comment on a real
  sequence is `0A000`: a sequence's record has no field for one.
