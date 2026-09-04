# 0050 — A user-defined type is a value

Status: accepted (phase 9, `b4-types`). Drafted at `3b1cd1a`, decided by the human on 2026-09-03:
**option 3, enum first**, with all three of the questions below answered. `docs/adr/0049` and
commit `43fe187` built the half that came first: `CREATE TYPE` and `DROP TYPE` exist, a type is a
catalog record keyed by name, and `pg_type` reports it. This ADR is the other half — what a value
of such a type *is* — and it is implemented in the order the decision sets, not all at once.

## Context

Run 46's row 4 is 51 tests across three files, and every one errors in `setup` on a `CREATE TYPE`.
That statement now runs. What the tests then do is **use** the type, and none of that works:

```sql
CREATE TABLE postgresql_ranges (id serial primary key, float_range floatrange);
INSERT INTO postgresql_ranges (float_range) VALUES (floatrange(0.5, 0.7));
SELECT * FROM postgresql_ranges WHERE float_range @> 0.6::float8;
```

The capture (`esker-rails-harness/captures/pg19_create_type.txt`) measures all three kinds:

- a **range** has a constructor named after the type, `@>`, `&&`, `lower`, `upper`, `isempty`, and
  prints `[1,5)`;
- a **composite** stores named fields, prints `(Paris,Rue)` with quoting only where needed, and its
  fields are read with `(address).city`;
- an **enum** is a label whose **declaration order is its sort order**: `'past' < 'future'` is true.

The obstacle is that `ColumnType` (`esker-keys/src/value.rs`) is a **closed, `Copy`, 24-variant
enum**, and every one of its variants is a type this node's key codec, row codec, columnar encoder
and `pg_type` know how to handle exhaustively. ADR 0047 already faced this once for arrays and
chose four flat variants over `Array(Box<ColumnType>)` — because a `Box` would cost at every one of
the hundreds of places the type is passed by value.

**A new type touches five places** (`docs/plans/phase-9-rails.md`, and the memory of the array
unit): the enum and its `ALL`, the record tag in `catalog::record`, the key encoding in
`esker-keys`, the row codec, and the columnar **reverse** mapping — which is the one the compiler
does not catch, because it is a match on the *other* direction.

## Options

### 1. A parameterised variant: `ColumnType::User(u32)`

The oid of the type, which the catalog can resolve to a `TypeDef`. Keeps `Copy`. But every
exhaustive match on `ColumnType` — the codecs, `pg_type`, `format_type`, the promotion table —
gains an arm that **cannot answer without a catalog lookup**, and those functions do not have one.
`Datum::column_type()` in `esker-keys` would have to return an oid it cannot name. The crate's
layering (ADR 0004: `esker-keys` knows no catalog) is what this breaks, and it breaks it everywhere
at once.

### 2. Store the underlying type, keep the identity on the column

`ColumnDef` grows `user_type: Option<u64>` — the way `comment` was added in ADR 0049 — and the
column's `ty` is what the value really is: `text` for an enum label, `text` for a range's printed
form, `text` for a composite's. Nothing in `esker-keys` changes; nothing in the codecs changes; the
catalog record grows one optional field per column.

What it buys: `CREATE TABLE t (mood custom_time_format)` stores and reads, `pg_attribute` reports
the user type's oid, `format_type` prints its name, and a value round-trips.

What it does not buy, and this is the honest part:

- **an enum's sort order is wrong.** Stored as its label, `'past' < 'future'` compares as text and
  is `false` where a real server says `true`. Storing the *ordinal* instead fixes ordering and
  breaks the printed form unless every read maps back through the catalog.
- a range's `@>` and a composite's `(x).field` still need real operators; the storage question is
  separate from the operator question.

### 3. Enum first, as a stored ordinal, and ranges as a second unit

The enum is the only one of the three whose value is a *scalar with an order*, and the order is the
whole feature. Store `int2` (the label's position), carry `user_type` on the column as in option 2,
and render through the catalog on the way out. Ordering, `=`, indexing and grouping all become the
`int2`'s, which is what PostgreSQL does internally too (`pg_enum.enumsortorder` is the sort key).

Ranges then need their own unit: this crate already has a range **value** (`crate::value::range`,
`daterange`, `isempty`, `&&`), so a user-defined range is that machinery pointed at a subtype from
the catalog — a bigger piece than the enum and the one 46 of the 51 tests want.

## Decision

**Option 3, and the three questions are answered as follows.**

1. **`user_type: Option<u64>` on `ColumnDef` is the shape**, not `ColumnType::User(oid)`. Invariant
   7 decides it: `esker-keys` is byte-opaque and knows no catalog, and option 1 breaks that in
   every exhaustive match at once — including `Datum::column_type()`, which would have to return an
   oid it cannot name. A column's `ty` stays what the value physically *is*; the identity of the
   user type rides beside it on the column, where the catalog is already in scope.
2. **An enum is stored as its ordinal**, an `int2` holding the label's position, and rendered
   through the catalog on the way out. Ordering, `=`, indexing and grouping all become the `int2`'s,
   which is what PostgreSQL does internally too — `pg_enum.enumsortorder` is its sort key. The
   objection is real and the rule that answers it is now part of this decision:

   > **An enum's sort order is assigned once and never reused.** A label's ordinal is fixed when
   > the label is created. Removing a label does not renumber the labels after it, and a new label
   > never takes a retired label's number. This is PostgreSQL's own rule for `enumsortorder`, and
   > it is what makes a stored ordinal safe: the bytes on disk mean the same thing for the life of
   > the type. A statement that would break it — a relabel that renumbers, a `DROP TYPE` with rows
   > still storing its ordinals — is refused, not silently reinterpreted.

3. **Ranges are worth it, and they come after enums.** 46 of the 51 tests are the range file, so
   the ranking row does not move until they land; but the enum is the smaller, self-contained piece
   that proves `user_type` end to end, and doing it first means the range unit inherits a shape
   that has already been through the codecs and the catalog. Composites stay **deferred**: they are
   4 tests and they need a `pg_class` row per type, which is a different mechanism from either.

The order is therefore: **enum values, then `daterange` and the range values over
`crate::value::range`, then composites when something needs them.** `hstore` and `citext` — two
extension types the suite also wants — are their own units and their own captures; they are not
user-defined types and this ADR does not cover them.

## Consequences

- Catalog record version **24**: `ColumnDef` gains `user_type: Option<u64>`, appended like every
  field before it. (The draft said 23; `a4e65b4` took 23 for `UNLOGGED` while this ADR was waiting,
  which is the standing rule that a version is claimed at HEAD in the commit that uses it and not
  reserved in advance.)
- `pg_attribute.atttypid` reports the user type's oid, and `format_type` its name — both already
  read from the `ColumnDef`, so both follow.
- The enum's `22P02 invalid input value for enum <name>: "<label>"` on a bad label, measured —
  `22P02`, the input-syntax class, and not `42704`.
- An enum label's ordinal is `int2` and the label list may be **empty**: `CREATE TYPE e AS ENUM ()`
  is legal, so a column of that type can hold nothing but NULL and every literal is `22P02`.
- `DROP TYPE` with a dependent column becomes reachable, which is the `2BP01` the capture pins and
  `exec/typedef.rs` cannot raise today because no column can depend on a type.
