# 0051 — a dropped column keeps its slot

## Context

`ALTER TABLE … DROP COLUMN` is 34 tests in 11 files of ActiveRecord's suite, and it is the first
DDL this node has met that appears to require touching rows that are already written.

The row codec is positional. A row value is
`version | varint(column count) | NULL bitmap | values`, in schema order, with **no attnum and no
column name in the bytes** (ADR 0019, moved down to `esker-keys` by ADR 0030). Two properties fall
out of that, and they are not symmetric:

* A row that carries **fewer** columns than the schema is padded, per column, from
  `RowSchema::missing` — PostgreSQL 11's `attmissingval`. This is what makes `ADD COLUMN … DEFAULT
  <constant>` instant on a populated table: the `ALTER` rewrites nothing.
* A row that carries **more** columns than the schema is **corruption** — `decode_row` returns an
  error rather than reading a prefix. That is deliberate: rows and the catalog are read at one
  snapshot, so a row from a schema the reader cannot see should not be visible to it either.

So the obvious implementation of `DROP COLUMN` — take the column out of the schema — makes the
second case the *normal* case. Every row written before the `ALTER` would decode as corrupt. The
only way to make a physical drop safe is to rewrite every row of the table inside the `ALTER`,
which is an unbounded amount of work under a lock in a statement a migration issues casually, and
is the opposite of what a log-structured engine is for.

It is also not what PostgreSQL does, and the whole of wave B is a bet that copying PostgreSQL is
cheaper than being clever (ADR 0031).

## What PostgreSQL actually does

Measured on `esker-pg19` (19beta1) on 2026-09-03, in one rolled-back session. Every claim below is
from that transcript, not from documentation.

A four-column table `dc (id bigint PRIMARY KEY, keep text, gone int, tail text)`, two rows, then
`ALTER TABLE dc DROP COLUMN gone`:

| | before | after |
|---|---|---|
| `attnum` | 3 | **3** — unchanged |
| `attname` | `gone` | `........pg.dropped.3........` |
| `atttypid` | 23 | **0** (`regtype` renders it `-`) |
| `attisdropped` | `f` | `t` |
| `attnotnull` | `f` | `f` — and a `NOT NULL` column's flag is **reset** |
| `attlen` | 4 | **4** — the width is retained |

* **`SELECT *` omits it** and `SELECT gone` is `42703 column "gone" does not exist`. Note the
  sentence: a `SELECT` says `column "gone" does not exist` while the `ALTER` that cannot find a
  column says `column "nosuch" of relation "dc" does not exist`. Two sentences, one code.
* **`information_schema.columns` omits it, and the gap stays visible**: `ordinal_position` for the
  three survivors is `1, 2, 4`. It is the attnum, not a rank.
* **A re-added column of the same name gets a new attnum.** `ALTER TABLE dc ADD COLUMN gone text`
  after the drop is attnum **5**, not 3. Attnums are never reused, and the dropped row stays in
  `pg_attribute` forever.
* **An `INSERT` written after the drop lists only the live columns** and the rows written before it
  keep answering — old and new rows coexist with no rewrite between them.
* **Everything on the table itself goes silently.** Both indexes mentioning the column vanish —
  including `dc_pair_idx`, a *multi-column* unique index where only one of the two columns was
  dropped — and so do the column's `CHECK`, its `NOT NULL`, its `pg_attrdef` default, and a foreign
  key declared on it. No `CASCADE` is required for any of them.
* **Something outside the table blocks.** A view over the column is
  `2BP01 cannot drop column gone of table dc because other objects depend on it`, with
  `DETAIL: view dcv depends on column gone of table dc` and PostgreSQL's `HINT: Use DROP ...
  CASCADE`. `DROP COLUMN gone CASCADE` drops the view and says `NOTICE: drop cascades to view dcv`.
* `DROP COLUMN nosuch` is `42703`; `DROP COLUMN IF EXISTS nosuch` is a `NOTICE … skipping` and
  succeeds.
* **Dropping every column is legal.** A table with zero live columns still answers `SELECT *`.

The shape of all of it: the column is not removed, it is **tombstoned**, and the row bytes are
never touched.

## Decision

**Copy the lazy drop. A dropped column keeps its slot in the row, and the row codec does not
change.**

1. **`ColumnDef` gains `dropped: bool`.** The column stays in `TableDef::columns`, in position, for
   the life of the table. Its ordinal is its attnum, and no later column moves.

2. **`esker-keys` is not touched.** This is the reason to prefer this design over every alternative
   below: `RowSchema` still has one type per slot, the dropped slot included, so `decode_row`
   consumes the old value's bytes at the right offset and every row written before the `ALTER`
   decodes exactly as it did. There is no row format version bump, no golden-test change, and
   nothing in the "ask before doing" list is touched.

3. **The SQL layer projects the tombstone away.** `TableDef` grows the accessor the rest of the
   crate uses — live columns, in order — and the raw list is what builds a `RowSchema` and nothing
   else. A dropped column is invisible to name resolution, to `SELECT *`, to `RowDescription`, to
   `INSERT` without a column list, and to `information_schema`.

4. **A new row writes NULL in the dropped slot**, which costs one bitmap bit and no value bytes.
   The row's column count therefore never shrinks.

5. **Rows are rewritten on their next write and never before.** `DROP COLUMN` is a catalog write
   and nothing else; there is no background rewrite job and no `VACUUM` analogue in this phase. An
   `UPDATE` re-encodes the row from the live schema, at which point the old value is gone from the
   new version — but the old MVCC versions still hold it until they are collected.

6. **Dependents on the table go with the column, silently; a dependent outside the table needs
   `CASCADE`.** Indexes mentioning the column (of any width), its `NOT NULL`, its `CHECK`, its
   default, and a foreign key declared on it are removed by the same catalog write. That is
   PostgreSQL's rule as measured, and it is also the only rule that is safe here: an index entry
   whose key includes a slot nothing will populate again cannot be maintained.

7. **The catalog record's column section becomes v24** (`CATALOG_FORMAT_VERSION` is 23 at the HEAD
   this ADR is written against). A record written by an older node decodes with every column
   `dropped: false`, which is exactly right — a node that never dropped a column has none.

## Options rejected

**Physically remove the column and rewrite the table.** Correct, and it makes `DROP COLUMN` an
O(table) statement holding a lock, in a migration that reads like a one-liner. It also needs a
crash-safe rewrite protocol — the `ALTER` cannot be atomic with the rewrite unless the whole thing
is one Raft-replicated batch, which is unbounded. Rejected on cost, and rejected again because it
diverges from the oracle in a way a client can time.

**Store the attnum beside each value in the row.** It would make a physical drop free and would
make the encoding self-describing. It also makes **every row of every table bigger forever** to
serve a statement most tables never see, and it is a row format change — the one thing this
project's constitution says to stop and ask about. The positional encoding is the reason a row is
small; paying for DDL in every row is the wrong trade.

**Keep a separate list of dropped positions beside `TableDef::columns`.** No record-format field on
`ColumnDef`, and the live list stays live. Rejected for the reason ADR 0030 gives for `RowSchema`
being one value rather than two arguments: two parallel lists have two chances to be built out of
step, and the symptom — a column reading the neighbouring column's bytes — is silent and
catastrophic. The flag travels with the column it is about.

**Refuse `DROP COLUMN` by name (`0A000`) and wait for a rewrite mechanism.** The honest placeholder,
and what this node does today — `plan::ddl::AlterTableAction` says so in as many words, and gives
this ADR's problem as its reason: "the row format carries a column *count* and not column
identity". The answer is that it does not need to carry identity, because the slot never moves. It fails 34 tests, and every one of them fails at *migration* time,
which means the suite never reaches the tests behind them. The measurement is what makes this worth
building now rather than later.

## Consequences

- **A dropped column is a permanent cost, and it is PostgreSQL's cost too.** One `pg_attribute` row
  and one NULL bitmap bit per row, forever; attnums are never reused, so a table that repeatedly
  adds and drops a column grows its slot count without bound. A real server caps it at 1600
  columns; whether this node needs that ceiling is a question for whoever meets it, not now.

- **`DROP COLUMN` does not erase data.** A row written before the drop still carries the dropped
  value on disk until that row is next written, and the old MVCC versions carry it until the
  safepoint passes them. This is exactly true of PostgreSQL and is worth writing down rather than
  discovering: it is not a way to remove a secret from a table.

- **The implementation risk is not the codec, it is the audit.** Every place that iterates a
  table's columns must now say whether it means all slots or only live ones, and the failure mode
  of getting it wrong is off-by-one column values — the same shape as the `RowSchema` hazard above,
  and just as quiet. The unit's test list must include a table read at *both* schemas: rows written
  before the drop and rows written after, in one `SELECT`.

- **The columnar replica is an open question.** A fragment (ADR 0022) is built from column data, and
  a dropped column's fragments are now unreachable through the catalog. Whether M4's routing skips
  them, or the learner rewrites, is a decision for the columnar lane; naming it here means it is
  found rather than discovered. Until it is settled, a columnar table is the case to test first.

- **The `2BP01` dependency check needs a dependency graph this node does not have.** Views are the
  only measured dependent, so the first implementation can find them by scanning view definitions
  for the column; if that is not possible, the honest answer is the refusal PostgreSQL gives, never
  a silent drop of a view's source column. This is the one part of the measured behaviour that may
  ship as a named refusal, and it must be recorded as a divergence if so.
