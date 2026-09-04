# 0063 — A user-defined range is a representation chosen by its subtype

*Status: accepted. 2026-09-04.*

## Context

`adapters/postgresql/range_test.rb` — 46 tests, the largest single file left on the board — begins
its `setup` with two statements this node parsed and could not then use:

```sql
CREATE TYPE floatrange  AS RANGE (subtype = float8, subtype_diff = float8mi);
CREATE TYPE stringrange AS RANGE (subtype = varchar);
```

followed by `add_column "postgresql_ranges", "float_range", "floatrange"`. Both types were already
in the catalog as [`TypeKind::Range`](0050-a-user-defined-type-is-a-value.md); what was missing was
a **column** of one, which `resolve_user_type` refused by name.

ADR 0050 settled the shape for an enum: the value is stored as its ordinal (`int2`), the column
carries the type's oid, and every name a client sees comes from the catalog. A range is the same
question with a different answer for the value, because a range's value is a range and not a
stand-in for one — and this node already has range values, six of them, stored as canonical text
with their subtype (`Datum::Range`).

The open question was what `ColumnType` a `floatrange` column holds. PostgreSQL has no built-in
range over `float8` or over `varchar`, so there was nothing to reuse for those two.

## Options

1. **One `ColumnType::UserRange`.** A single variant meaning "a range over whatever the catalog
   says". Rejected: `range_subtype` is a pure function of the `ColumnType` and the row codec is
   byte-opaque below `esker-keys` (invariant 7) — a decoder handed `UserRange` has no way to learn
   the subtype, so it could not build the `Datum::Range` the row holds.
2. **A `ColumnType` per user type.** Rejected on sight: the set is unbounded and it is a catalog
   fact, not a vocabulary one.
3. **A representation chosen by the subtype**, with the oid saying which type it actually is.
   Chosen.

## Decision

A user-defined range column's `ColumnType` is **the range representation whose bounds are its
subtype**, and its identity is `ColumnDef::user_type` — the oid — exactly as an enum's is.

Six subtypes map onto the range types this node already had (`timestamp` → `tsrange`, `int4` →
`int4range`, and so on). Two are new: `ColumnType::FloatRange` for a range over `float8` and
`ColumnType::VarcharRange` for one over `varchar`. A subtype with no representation is `0A000`
naming it, rather than a column typed as some *other* range that would read every value back wrong.

Two user types over one subtype **share a representation and stay two types**, which is
[ADR 0042](0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md)'s rule satisfied rather than bent: a
range's comparison is its bounds', which is a function of the subtype alone.

`text` is deliberately left without a representation even though `varchar` has one and the two
compare identically. `lower()` of a bound must answer `text` for one and `character varying` for
the other, and with a shared representation there would be nothing left to tell them apart — so a
range over `text` is a `0A000` until that is worth a representation of its own.

### The two new variants are not in `ColumnType::ALL`

`ALL` is the list of types with a **`pg_type` row of their own**, and everything derived from it
says so: the `pg_type` view, `'name'::regtype`, `type_by_oid` for a parameter's declared oid.
A `floatrange` already has a `pg_type` row — written by the `CREATE TYPE` that made it, under the
name that statement gave and with the oid it allocated. Putting the representation in `ALL` would
give it a **second** row, named after the subtype and carrying oid `0`, and would make
`'float8range'::regtype` resolve where a real server answers `42704`.

They are listed in `ColumnType::USER_RANGES` instead, and the codec round-trip properties iterate
`ALL` chained with it — because a type nothing generates is a type whose encoding is unchecked,
which is how `range_subtype` came to have two copies that disagreed.

Their `name()` is `float8range` and `varcharrange`: not PostgreSQL type names, because there are
none, and reachable only where an error is built from a `ColumnType` with no oid in hand. Two such
messages are declared divergences in `tests/corpus/pg19_floatrange.txt`.

## Consequences

* `pg_range` is no longer empty. `ActiveRecord`'s boot query is
  `pg_type LEFT JOIN pg_range ON oid = rngtypid`, and a range type whose `rngsubtype` comes back
  NULL is one it does not register — so the rows are what make a user range readable at all. Both
  the six built-ins and the tenant's own are listed, with the subtype's **own** oid: `int4range`
  reports `integer` even though every integer here is an `i64`.
* A cast to a user type ([ADR 0053](0053-a-cast-to-a-user-defined-type-is-resolved-once-per-statement.md))
  now has two halves: an enum folds to its ordinal, a range to its canonical text. The rule that
  ADR states — resolved once per statement, the label in a projection and the ordinal everywhere
  else — is unchanged; a range simply has one form rather than two.
* The enum lookup and the user-type lookup are now two functions. Everything that rewrites a
  *value* asks the narrow one (`Scope::enum_at`), and everything that tells a client a *name*
  asks the general one (`Scope::user_type_at`). They were one function, and it answered
  `floatrange` with `float8range`.
* A range column still **cannot be indexed here**, where a real server indexes one fine. That gap
  is now a refusal a client can read rather than an index that is built and then unwritable —
  `esker_keys::row::is_index_key` is the one list and a test makes the decoder agree with it.
