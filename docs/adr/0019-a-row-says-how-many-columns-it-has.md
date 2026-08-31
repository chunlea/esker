# 0019 — A row says how many columns it has

Status: accepted (phase 6a, the ALTER continuation). Changes two formats that had golden tests, so
it was put to the project owner before it was written (`CLAUDE.md`, "ask before doing"), and both
options below were on the table. See `crates/esker-sql/src/row.rs`,
`crates/esker-sql/src/catalog/record.rs`, `docs/plans/phase-6a.md` §6.

## Context

`ALTER TABLE ADD COLUMN` is meant to be free: append the column to the catalog, rewrite no rows,
and let the rows written before it read back with the new column NULL — which is what PostgreSQL
shows for them anyway, and what the row value's leading version byte was put there to make
possible.

The row value could not do it. Its layout was

```text
version:u8=1 ++ null_bitmap:ceil(n/8) ++ non-NULL columns in column order
```

and `n` is the reader's column count, not the writer's. Nothing in the row says how wide the row
is. So after one `ADD COLUMN`:

* a two-column row read as three takes the third bitmap bit — unset, which means *not* NULL — and
  runs off the end of the value. An error, which is at least loud;
* an **eight**-column row read as nine is worse. `ceil(9/8)` is two bytes, so the reader takes the
  first column's leading byte as the second bitmap byte and then decodes the remaining bytes as
  columns. There is no truncation to notice. It is a wrong answer, delivered confidently, and for
  an `int8` first column it is a plausible one.

The version byte does not help, because it distinguishes *layouts* and both of those rows have the
same layout. What is missing is not a version, it is a **width**.

## Decision 1: the row value carries its column count

```text
version:u8=2 ++ columns:varint ++ null_bitmap:ceil(columns/8) ++ non-NULL columns in column order
```

The count comes before the bitmap because the bitmap's length is derived from it. Decoding reads
`columns`, decodes that many, and:

* **pads with NULL** when the table now has more. This is the `ADD COLUMN` case and it is why the
  `ALTER` rewrites nothing. The `ALTER` refuses `NOT NULL` (Decision 3), so NULL is a value every
  padded column is allowed to hold.
* **refuses** when the row claims more columns than the table has. That is corruption, not a case
  to tolerate: a row and the catalog are read at one snapshot, and the `ALTER` that widened a row
  committed before the row that used it, so a transaction that cannot see the schema cannot see
  the row either.

One varint per row, and for every table narrower than 128 columns it is one byte.

## Decision 2: version 1 is not read

Version 1 is refused by the same typed error as any other unknown version. There is no
compatibility path, because there is no version 1 data: `esker-sql` has never had a backend that
persists anything — `MemoryBackend` is in-process — so every version 1 row that has ever existed
lived inside a test that runs on the same commit as the code.

The alternative was to keep version 1 decodable as "exactly the reader's column count". That is
only sound for a table that has never been altered, because a version 1 row in an altered table is
precisely the row this ADR exists to read correctly; making it right needs the catalog to remember
each table's pre-`ALTER` column count forever. A compatibility path that nothing can exercise and
that is *wrong in the case that matters* is worse than a loud refusal.

## Decision 3: a count, not column identity — and what that does not buy

A count works because `ADD COLUMN` appends. It says nothing about *which* columns are present, so
it cannot survive a `DROP COLUMN`: dropping the second of three columns leaves rows whose count is
3 and whose second value belongs to a column that no longer exists.

`DROP COLUMN`, `RENAME COLUMN` and a type change all stay `0A000` (contract C2), and this is one of
the reasons. Closing them needs a row format that carries column *identity* — a column id per
value, which is what TiDB's row format v2 does — and that is a version 3, named here so that it
arrives as a planned format change rather than a surprise. It is not free: identity costs a varint
per non-NULL column where the count costs one per row.

## Decision 4: a table carries a schema version

The catalog's table record gains `schema_version: u64`, `1` as `CREATE TABLE` leaves it and one
more for each `ADD COLUMN`. `CATALOG_FORMAT_VERSION` goes to 2 for it, on the same
no-version-1-data argument as Decision 2.

Nothing needs it to read a row — that is what Decision 1 is for — and it is recorded as such in the
field's own documentation, because a field that looks load-bearing and is not is a trap for the
next reader. It is here for two reasons that are about the *table* rather than about any row:

* the cluster-wide `catalog_version` says that *something* changed and every cache must be
  discarded; it cannot say which table moved. A schema change is the table's own event.
* the staged online schema change in [ADR 0020](0020-online-schema-change.md) attaches a per-column
  state (absent → delete-only → write-only → public) to a version of the table, and its two-version
  invariant is stated over exactly this number. Adding the field now means that design extends a
  record rather than introducing one.

## What this makes true

* A row written before a column existed reads back with that column NULL, at any width, across any
  number of successive `ADD COLUMN`s — proptested against arbitrary appended column lists.
* The eight-to-nine column case, the one that used to answer wrongly rather than fail, has its own
  test asserting the padded answer.
* A row from a schema the reader cannot see is `DATA_CORRUPTED`, never a guess.
* Both formats still fail closed on an unknown version, and both still have a golden that spells
  out every byte.
