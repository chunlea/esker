# 0033 — tier 1 of the type surface: the types that need no new storage

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

## What each of them actually is, measured

Against `esker-pg19` (19beta1), not from memory:

The three the ladder asks for first, side by side; the other four are in the decision's table.

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

**Option 2.** New `ColumnType` variants, with the typmod as an `i32` on `ColumnDef`, which is where
PostgreSQL keeps it too (`pg_attribute.atttypmod`).

**Scope: tier 1 is every type that needs no new storage.** The standing order this ADR serves is
that *this system supports the PostgreSQL types, it does not refuse them* — `0A000` naming a type
is the state between units and never a destination. What decides the tiers is therefore not
importance but **whether a type is a format addition**, and tier 1 is the set that is not:

| Tier-1 type | OID | `typlen` | stored as | its own error |
|---|---|---|---|---|
| `integer` / `int4` | 23 | 4 | `Datum::Int4(i32)` | `22003 integer out of range` |
| `smallint` / `int2` | 21 | 2 | `Datum::Int2(i16)` | `22003 smallint out of range` |
| `real` / `float4` | 700 | 4 | `Datum::Float4(f32)` | `22003 "…" is out of range for type real` — and this one **quotes the value** where the integers do not |
| `character varying(n)` | 1043 | -1 | `Datum::Text`, length-checked | `22001 value too long for type character varying(5)` |
| `character(n)` / `bpchar` | 1042 | -1 | `Datum::Text`, blank-padded to `n` | truncates on an explicit cast, raises `22001` on assignment |
| `timestamp(p)` without time zone | 1114 | 8 | `Datum::Timestamp(i64)` | `22008 timestamp out of range` |
| `serial` / `smallserial` | — | — | `int4` / `int2` plus an identity | — |

**`serial` is `integer` with an identity, and this ADR reverses the ruling that made it `0A000`.**
Phase 9 unit 2 refused `serial` by name, and its argument was entirely about the missing type: a
`serial` mapped onto `bigserial` would hand a client `int8` where a real server hands `int4`.
Measured, `serial` is not a type at all — `information_schema` reports the column as `integer`,
`NOT NULL`, with `DEFAULT nextval('t1_a_seq'::regclass)`, which is three things this node already
has separately. The refusal existed only because `int4` did not; with `int4` it has no argument
left, and keeping it would be refusing a type on the strength of a reason that has gone.

**Two typmod encodings, and they are not the same.** `varchar(5)` and `character(3)` store
`atttypmod` as the length **plus four** — measured: `9` and `7`, and `format_type('bpchar', 7)` is
`character(3)`. `timestamp(6)` stores the precision **directly**: `6`. An implementation that
assumed one rule would print `character(5)` for a `varchar(5)` or `timestamp(2)` for a
`timestamp(6)`, so both are captured rather than derived.

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

* `ColumnType::ALL` goes from six to twelve, and every `match` over it is a compile error until it
  handles the new ones. That is the point of it.
* **The order is the ladder's, not the table's**: `int4` → `varchar(n)` → `timestamp(6)` →
  `serial` → `int2`, `character(n)`, `float4`. Each is its own commit with its own capture, because
  each can be measured on its own — the first three are what `ActiveRecord`'s own migration emits,
  in the order it emits them.
* `pg_type` grows three rows **without being touched**, because
  `crates/esker-sql/src/catalog/pg_catalog.rs` derives its rows from `ColumnType::ALL`. The
  `typname`/`typinput` tables beside it are exhaustive matches and will not compile until the three
  are named: `int4`/`int4in`, `varchar`/`varcharin`, `timestamp`/`timestamp_in`, all measured.
* `'integer'::regtype::oid` becomes answerable, which is what rung 2 asks first. The cast is a
  second feature and is scoped with this one because neither moves the ladder alone.
* **`serial` and `smallserial` stop being `0A000`.** Phase 9 unit 2's refusal is withdrawn here,
  and the plan's §2 says so where it made the argument, so a reader of that unit is not left with a
  rule this ADR has taken away.
* **The row/column differential must stay green**: `esker-columnar`'s `Value` gains the same three
  and `tests/joint_gate.rs`'s fragment-against-row-scan comparison covers them.
* Three divergences are expected and each needs its own line in the plan's §6 when it is measured:
  `22003` naming or not naming the value, the cast-truncates / insert-raises asymmetry, and
  `timestamp` under a `TimeZone` this node refuses anyway.
## Roadmap: what tier 1 does *not* cover, and in what order it arrives

Refuse-by-name is "not yet", and a reader should be able to see the order rather than infer that a
missing type is a decision. Tier 1 is above; the two below are **format additions** and each is its
own ADR — a key-codec encoding for the indexable ones, a columnar column encoding, goldens, old
bytes still decoding, and the row/column differential green.

**Tier 2 — new physical types.** `date`, `time`, `numeric(p, s)`, `uuid`, `json` / `jsonb`,
`interval`, and **arrays** (`text[]` and `integer[]` columns *and* `= ANY($1)`, which
`ActiveRecord` uses for every `IN` with binds — four of its 36 boot statements). Two of them carry
the hard part: `numeric`'s text parity, because PostgreSQL prints the *declared* scale exactly and
this is the type ADR 0031 has been refusing on those grounds since unit 0; and `jsonb`'s stored
form, because it normalises key order and whitespace, so a capture decides what is stored rather
than a preference.

**Tier 3 — asked for explicitly, after arrays.** The range types
(`int4range`, `int8range`, `numrange`, `tsrange`, `tstzrange`, `daterange`) — whose canonical form
`[1,10)` is settled by capture and never guessed — then `enum` (`CREATE TYPE … AS ENUM`), `citext`,
`hstore`, `tsvector`, `bit` / `varbit`, `inet` / `cidr`, `money`, `xml`, `ltree`, and the geometric
types. Ranked by suite evidence once tiers 1 and 2 get `ActiveRecord` past `establish_connection`
and its migrations, which is the point at which the scoreboard can rank them at all.

Every type in every tier ships the same way: a PG19 capture corpus **first** under the C1/C2
contract, a `pg_type` row derived from `ColumnType::ALL` so the catalog cannot lie about what the
server has, text output byte-identical to the capture, and a regression per shape. ADR 0031's rule
still governs *how* — a type ships only where its observable text matches the capture for every
input that does not error, and is refused by name where it cannot.

**Split across lanes**: tier 1 is this ADR's units. Tiers 2 and 3 belong to a dedicated types lane
(`esker-keys`, `esker-columnar`, and the SQL type module), so that scoreboard iteration continues
beside them rather than behind them.
