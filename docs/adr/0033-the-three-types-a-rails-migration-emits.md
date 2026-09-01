# 0033 — the three types a Rails migration emits: `integer`, `character varying`, `timestamp`

Status: **accepted**
Date: 2026-09-01
Supersedes nothing. Extends [ADR 0019](0019-row-value-encoding.md) and
[ADR 0030](0030-the-row-codec-moves-down.md); the compatibility rule it applies is
[ADR 0031](0031-rails-compatibility-is-measured.md).

## Context

`docs/plans/phase-9-rails.md` has counted, twice, what stands between this node and an
`ActiveRecord` that can open a connection. After `pg_type` and `pg_range` landed, the ladder's
rung 2 — `establish_connection` and one migration — stops here:

```
RUNG 2 FAIL  ActiveRecord::StatementInvalid: PG::FeatureNotSupported:
             ERROR: the expression 'integer'::regtype::oid is not supported
```

and the statement behind it is the migration itself:

```sql
CREATE TABLE "harness_widgets" ("id" bigserial primary key,   -- runs
  "name" character varying NOT NULL,                          -- 0A000 the type CHARACTER VARYING
  "count" integer DEFAULT 0,                                  -- 0A000 the type INT
  "live" boolean,                                             -- runs
  "created_at" timestamp(6) NOT NULL, …)                      -- 0A000
```

`t.string` compiles to `character varying`, `t.integer` to `integer`, `t.timestamps` to
`timestamp(6)`. This node has six stored types and none of the three is one of them
(`esker_keys::value::ColumnType`).

**The measurement, and why it outranks a bigger number.** Three of `ActiveRecord`'s 36 boot
statements are behind these types (13, and 14 + 20 behind it) and one more is behind the
`::regtype` cast that asks about them. A second `JOIN` is behind seven — more than twice as many —
and is *not* the next unit, because nothing on that list is on the path a migration takes: a client
that cannot run a migration never issues those seven statements in anger.
`docs/bench/rails-scoreboard.md` prints both rankings for that reason.

## What each of the three actually is, measured

Against `esker-pg19` (19beta1), not from memory:

| | `integer` | `character varying` | `timestamp` |
|---|---|---|---|
| OID | 23 | 1043 | 1114 |
| `typlen` | 4 | -1 | 8 |
| spellings | `int`, `int4`, `integer` | `varchar`, `character varying` | `timestamp`, `timestamp(n)` |
| out of range | `22003 integer out of range` — **the value is not quoted**, unlike `int8`'s | `22001 value too long for type character varying(3)` on `INSERT` | `22008 timestamp out of range: "294277-01-01"` |
| a length / precision | — | `atttypmod` is length + 4; `format_type(varchar, 259)` is `character varying(255)`; no typmod prints as `character varying` | `atttypmod` is the precision; `timestamp(3)` **rounds**, `timestamp(6)` is the default and prints the same as `timestamp` |
| the surprising one | `2147483647::integer + 1::integer` is `22003` — arithmetic overflows at the *declared* width | an explicit cast **truncates** (`'abcd'::varchar(3)` is `abc`) where an `INSERT` **raises** | `'2020-01-01 12:00:00'::timestamp = …::timestamptz` is `t`, by converting through the session `TimeZone` |

## Options

1. **Alias them onto the types this node has** — `integer` → `int8`, `character varying` → `text`,
   `timestamp` → `timestamptz`.
2. **Three new `ColumnType` variants**, with the length and precision carried by the *column*
   rather than by the type.
3. **A declared type on `ColumnDef` beside the stored one** — store an `int8`, tell the client
   `int4`.

## Decision

**Option 2.** Three new variants — `ColumnType::Int4`, `ColumnType::Varchar`,
`ColumnType::Timestamp` — with the typmod as an `i32` on `ColumnDef`, which is where PostgreSQL
keeps it too (`pg_attribute.atttypmod`).

**Why not (1).** It is the argument phase 9 unit 2 already made for refusing `serial`, and the
capture makes it concrete: an `integer` column mapped to `int8` accepts every value between 2^31
and 2^63 that a real server answers `22003` for, and `2147483647 + 1` returns a number where a real
server raises. That is a wrong answer, which is the outcome this project ranks worst. `timestamp`
onto `timestamptz` is the same shape one level up — a different OID at the client and a different
answer under any `TimeZone` that is not UTC.

**Why not (3).** A declared type that disagrees with the stored one has to be right in every place
a type is asked for — `RowDescription`, `format_type`, `pg_type`, an error message, a comparison,
an overflow check — and each of those is a place it can be forgotten. A type that *is* `int4` is
right in all of them by construction, and `ColumnType::ALL` is the list that makes a forgotten one
a compile error.

**Why the typmod is on the column and not in the type.** `ColumnType` is `Copy`, `Ord`, and has an
`ALL` array that half a dozen tests iterate; a payload variant would take all of that away, and it
would make `pg_type` list a `varchar(3)` row, which a real server does not — there is one `varchar`
in `pg_type` and the length lives in `pg_attribute`. Following PostgreSQL's own split costs one
field and keeps every other decision unchanged.

## What this is *not*: an on-disk format change

Checked rather than assumed, and it is the reason this is one unit rather than a migration:

* **The row codec carries no per-value type tag.** `esker_keys::row::encode_row` writes a NULL
  bitmap and then one value per column *in the schema's order and the schema's type*, so what a
  byte means is decided by the `RowSchema` a reader already has. `Varchar` writes exactly what
  `Text` writes and `Timestamp` exactly what `TimestampTz` writes; `Int4` writes four little-endian
  bytes because that is its width. **Every row written before this ADR decodes identically after
  it**, and the goldens say so rather than being regenerated.
* **The columnar file's type tags are append-only.** `esker_columnar::value::ColumnType::tag` is a
  frozen byte, 1 through 6, and `from_tag` answers corruption for anything else. The three take 7,
  8 and 9. An old file has no tag above 6 and reads unchanged; a new file is unreadable by an older
  reader, which is what that `from_tag` arm is for and is the direction this project already
  accepts for a format that carries its version.
* **The catalog's `ColumnDef` record does change** — it grows a typmod — and that record has a
  format version and a golden. It is the one place a version bump is owed, and an absent typmod
  reads as `-1`, which is "no length given" and is what every column written before this ADR meant.

`CLAUDE.md`'s "ask before doing" covers a format change with a golden. This one is authorised in
the brief that commissioned the unit, which grants `crates/esker-keys/**` and
`crates/esker-columnar/**` *for exactly these types* and requires this ADR first.

## Consequences

* `ColumnType::ALL` goes from six to nine, and every `match` over it is a compile error until it
  handles the three. That is the point of it.
* `pg_type` grows three rows **without being touched**, because
  `crates/esker-sql/src/catalog/pg_catalog.rs` derives its rows from `ColumnType::ALL`. The
  `typname`/`typinput` tables beside it are exhaustive matches and will not compile until the three
  are named: `int4`/`int4in`, `varchar`/`varcharin`, `timestamp`/`timestamp_in`, all measured.
* `'integer'::regtype::oid` becomes answerable, which is what rung 2 asks first. The cast is a
  second feature and is scoped with this one because neither moves the ladder alone.
* **The row/column differential must stay green**: `esker-columnar`'s `Value` gains the same three
  and `tests/joint_gate.rs`'s fragment-against-row-scan comparison covers them.
* Three divergences are expected and each needs its own line in the plan's §6 when it is measured:
  `22003` naming or not naming the value, the cast-truncates / insert-raises asymmetry, and
  `timestamp` under a `TimeZone` this node refuses anyway.
* **Not in scope**: `numeric` (ADR 0031's backlog), arrays (`= ANY(…)`, four statements, the same
  two crates and worth the same round), `char(n)`, `date`, `time`, and any integer width other than
  4 and 8.
