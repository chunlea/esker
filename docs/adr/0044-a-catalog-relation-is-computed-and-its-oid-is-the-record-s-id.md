# 0044 — A catalog relation is computed, and its oid is the id the record already carries

## Context

`pg_type` and `pg_range` (`c88a932`), then `pg_class` and `pg_namespace` (`9de9519`), established that
a `pg_catalog` relation on this node is a **view over the catalog records** rather than a second copy
of them: rows materialised per query, no duplicated state, nothing to keep in step. Phase 13 finished
that surface — `pg_attribute`, `pg_attrdef`, `pg_index`, `pg_constraint`, `pg_collation`, five
`information_schema` views, and the functions that print a definition — and in doing so put the
design under a load the first four never did.

The load is **joins**. Every statement `ActiveRecord`'s schema dump sends is an oid join:

```sql
FROM pg_class t
INNER JOIN pg_index d ON t.oid = d.indrelid
INNER JOIN pg_class i ON d.indexrelid = i.oid
```

Four oids, three relations, one statement. Two more in `columns()`
(`a.attrelid = d.adrelid`), two in `foreign_keys()` (`c.conrelid = t1.oid`,
`c.confrelid = t2.oid`). None of the first four views was ever joined to another, so nothing had read
`pg_class.oid` at all before this phase — and the first thing that did found it wrong.

## Decision

### 1. A relation's oid is the id its catalog record already carries, from one function over one snapshot

| Relation | oid | Where it comes from |
|---|---|---|
| table | `table_id` | the name record |
| index | `index_id` | the name record |
| sequence | `SequenceDef::id` | the sequence record, keyed by the column it fills |
| primary key | `PRIMARY_KEY_OID_BASE + table_id` | derived — it has no record of its own |
| `NOT NULL` constraint | `NOT_NULL_OID_BASE + (table_id << 16) + attnum` | derived — it has no record at all |
| catalog view | its reserved `TableDef::id` | `pg_catalog::VIEW_ID_BASE` |

`crates/esker-sql/src/catalog/pg_relations.rs` reads the tenant's whole catalog once — one scan of the
name records, then one point read per table and per sequence — and every view in the phase reads *that*.
No view computes an oid for itself, because five functions agreeing is five chances to disagree.

**This fixed two collisions.** `9de9519` read the id out of the name record's key rather than out of
the relation it decoded, so a primary key and a sequence both reported the **table's** id: `t`,
`t_pkey` and `t_id_seq` were three rows of `pg_class` with one oid between them. Measured on 19beta1,
they are three distinct oids there and `pg_index.indexrelid` differs from `indrelid`. The sequence had
an id all along. The primary key did not, and getting it one would mean a field on the name record —
a `CATALOG_FORMAT_VERSION` bump for a number that can be derived — so it is derived, in a band a
per-tenant relation-id sequence starting at 1 cannot reach, and reversibly: `oid - BASE` is the table
whose key it is, which is what `pg_index.indrelid` and `pg_get_constraintdef` need on the way back.

`VIEW_ID_BASE` moved from `u64::MAX - 1023` to `i64::MAX - 1023` in the same change. `pg_class.oid` is
a `bigint`, so above `i64::MAX` every view clamped to the same value and three of them shared an oid —
the same failure one level up.

### 2. A type this node does not have is provided where the client's use of it is text-shaped, and refused where the use needs the type

Two columns of the new views are array-ish and this node has no array types. They get **opposite**
answers, and the rule is the client's use rather than the type:

* **`pg_index.indkey` is `text`.** An `int2vector` prints space-separated (`3 4`), and
  `ActiveRecord` reads it as `row[2].split(" ").map(&:to_i)` — a text operation on the rendered value.
  The same characters mean the same thing, so it is provided, and the declared type is the divergence.
* **`pg_constraint.conkey` is `42703`.** Every use `ActiveRecord` makes of it is a real array
  subscript — `c.conkey[idx]` under `generate_subscripts` — so a `text` column spelled `{1}` would
  answer `conkey[1]` with a brace rather than a column number. A wrong answer, where the refusal is a
  gap a capture closes.

`information_schema.key_column_usage` answers the question `conkey` was for, one row per key column,
which is why refusing it costs a client nothing it cannot get another way.

### 3. Every write to a computed relation is `42501`, one rule for all of them

Measured, and the two neighbouring schemas really do differ on a real server: `DROP TABLE
pg_catalog.pg_class` is `42501 permission denied: "pg_class" is a system catalog`, while `DROP TABLE
information_schema.tables` is `42809 "tables" is not a table` with `HINT: Use DROP VIEW to remove a
view.` — because `information_schema` there is built out of views and `pg_catalog` out of tables. Here
every catalog relation is computed and there is only one kind, so there is one rule: `42501`, naming
the relation. The `pg_catalog` half agrees exactly and the `information_schema` half is declared.

The **name resolution a write uses is the one a read uses**, which is not a detail: refusing the
schema qualifier first would answer `0A000` for `DROP TABLE pg_catalog.pg_class` and skip the guard
entirely.

### 4. A schema is part of a relation's name only where the bare name is not a relation

`information_schema.tables` is the view's name, qualifier included, because a bare `tables` is `42P01`
on a real server and answering it here would invent a relation. `pg_catalog.pg_class` is the mirror
case: `pg_class` *is* a relation on its own, so the qualifier is stripped. `public.t` stays refused by
name — answering it would be right when the qualifier is `public` and wrong when it is not.

### 5. What is refused, and named

Each of these is `0A000` or `42703` naming itself, counted under [ADR
0031](0031-rails-compatibility-is-measured.md) category (c):

* **`pg_depend`, `pg_am`, `pg_extension`, `pg_description`** — relations this node keeps nothing for.
* **`pg_collation` is empty and every `attcollation`/`typcollation` is 0.** What makes that safe is
  measured: `a.attcollation <> t.typcollation` is false for **every column on a real server too**, so
  `columns()`'s `LEFT JOIN pg_collation` yields NULL on both, for the same reason.
* **Comments.** `col_description` and `obj_description` would be NULL for every relation here, which
  is what a real server answers for one with no comment — but `COMMENT ON` is not a statement this
  node has, so the functions are refused by name rather than answering a constant.
* **`information_schema.table_catalog`.** A real server reports the database it is connected to; this
  node has no database concept — no `current_database()`, and the startup parameter never reaches the
  executor — so there is no name to report and a constant would be a value nobody measured.
* **`contype = 'u'`.** A `UNIQUE` constraint and a `CREATE UNIQUE INDEX` produce the *same*
  `IndexDef` here and PostgreSQL distinguishes them. Nothing in the catalog record says which
  statement wrote the index, so `pg_constraint` reports the shape it can prove — the unique index, in
  `pg_index`, which agrees exactly — and claims no constraint. A `u` row per unique index would report
  a constraint the user never declared. **Closing it is one bool on `IndexDef`, which is a catalog
  record format change and therefore a question for a human** (`CLAUDE.md`, "Ask before doing").
* **Foreign keys, `CHECK` and `EXCLUDE`.** All three are `0A000` in the DDL, so no rows is a *correct*
  answer about this catalog rather than a gap — and `information_schema.referential_constraints` is
  empty for the same reason.
* **`attnum <= 0`.** A real server has six system-column rows per relation (`ctid` -1 … `tableoid`
  -6); this node has none of those columns. No `ActiveRecord` statement can see it: every one of them
  says `attnum > 0`.

## Consequences

* **A catalog scan is a scan**, and is bounded: past `MAX_CATALOG_RELATIONS` the snapshot is `53400`
  rather than an unbounded allocation on a client's behalf — the rule `Sort`, the group table, a
  materialised join side and the savepoint block already follow. `pg_class` was unbounded before this
  phase and is not now.
* **A function of the catalog is snapshotted per cursor, not per row.** `'name'::regclass` has a
  literal argument and is resolved once per statement before the plan is built, where a sequence call
  is. `pg_get_indexdef(d.indexrelid)` has a *column* argument and genuinely answers differently per
  row, so the cursor holds one snapshot and every row reads it — otherwise a schema dump would be
  quadratic in the number of relations.
* **`attnum` has one definition.** `pg_attribute.attnum`, `pg_index.indkey`, `pg_constraint`'s oid and
  `information_schema.key_column_usage.ordinal_position` all number from the columns a *user* can see,
  because a table with no declared primary key hides an internal row id in slot 0. Numbering from the
  raw slot made an index on the first column of a keyless table report `2`, which would have made
  `a.attnum = ANY(i.indkey)` name the column after the indexed one. One function,
  `pg_relations::attnum_of`, for that reason.
* **`pg_type` grew a column** (`typcollation`), the first time one of the four original views did.
  It went **last**, because `SELECT *` expands in the declared order (`7be39ca`) and a column inserted
  anywhere else moves every one after it.
* **`ActiveRecord`'s boot counter moved 22 → 24** — `check_constraints()` and
  `exclusion_constraints()`, the two schema-dump statements that need no array. The other four need
  `array_agg`, `ARRAY(SELECT …)`, `generate_subscripts` or `= ANY` over an array value, and **the rows
  behind every one of them are here and agree**; what is missing is the array surface, which is
  another lane's.

## Alternatives considered

**Catalog tables as real tables in a reserved key space.** Rejected by `c88a932` and rejected again
here, harder: every `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE` and sequence allocation would owe a
second write forever, and a `pg_attribute` that disagreed with the `'m'` space would be wrong in a way
only a client notices. Nine relations' worth of that, kept in step by hand.

**A sequential oid per snapshot.** Unique and stable within a catalog version, and wrong across one:
a client that reads `c.oid` and calls `pg_get_constraintdef(c.oid)` after any DDL would get a
different constraint. The derived bands are stable for as long as the column is.

**Giving the primary key and the `NOT NULL` constraint real records.** Correct, and a
`CATALOG_FORMAT_VERSION` bump with readers on both sides of it — for numbers that are a function of
things already stored. Deriving costs nothing and cannot desynchronise.
