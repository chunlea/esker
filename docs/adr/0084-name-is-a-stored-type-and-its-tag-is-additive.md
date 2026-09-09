# 0084 — `name` is a stored type, and its tag is additive

## Context

`datatype_test.rb#test_name_column_type` sends `CREATE TABLE ex(data name)`, and two
`compatibility_test.rb` cases reach the same type through `information_schema.tables.table_name`,
which is the `sql_identifier` domain over `name`. Until now `name` was `0A000 the type name is not
supported`.

`name` is not a spelling of `text`. Measured on PostgreSQL 19beta1
(`crates/esker-sql/tests/captures/pg19_name_type.txt`):

* **`typlen` is 64 and positive**, where every other string type answers `-1`. It is the one
  fixed-width string type, and a client reads that width from the `RowDescription` and from
  `pg_attribute.attlen`.
* **It truncates at 63 bytes**, not 64 — the last byte is C's terminator — and the cut is on a
  character boundary: `('é' × 64)::name` is 31 characters and 62 octets, not 31 and a half.
  Truncation, not refusal: a value too long for a `varchar(n)` is `22001`, and this one is simply
  shorter.
* **Its collation is C** (950), so a `name` column sorts in byte order and every capital precedes
  every lower-case letter. A `text` column of the same values does not.
* **It takes no typmod at all**: `'x'::name(10)` is `42601 type modifier is not allowed for type
  "name"`.

The type surface directive says the type system supports the PostgreSQL types, and refuse-by-name
is a placeholder between units rather than a destination. A column declared `name` has to be
*stored*, so this is a new member of `ColumnType` and therefore a new tag in two on-disk spaces.

## Decision

`ColumnType::Name` is a stored column type. It is added:

* to `esker_keys::value::ColumnType` and its `ALL` table, which is what makes `pg_type` grow a row
  for it without anyone writing one;
* to the **row codec** (`esker_keys::row`) in the text-shaped group — a `name` is its bytes, as a
  `varchar` is — and to `Datum::fits`, so a `Datum::Text` is already a value of the type;
* to the **catalog record** (`esker_sql::catalog::record`) as `TAG_NAME = 93`, the first free tag;
* to the **columnar reverse map** (`esker_keys::columnar`) as tag `94`, the first free tag there.

Both tag spaces are **append-only**, which is what makes this decision cheap: 93 and 94 were free,
and every tag already written keeps exactly the meaning it had. No existing encoding changes, no
golden file changes, and a row written before this commit decodes to the same values after it. A
reader that meets tag 93 or 94 and does not know it refuses rather than guessing, which is the
direction a version skew has to fail in.

The truncation lives in the **cast** (`esker_sql::value::truncate_to_name`), not in the codec: the
character boundary is a property of the value's text, and by the time a datum reaches the row codec
it is already a value of its type. That is the same split `bpchar`'s padding has.

## Consequences

**The ordering is free.** A `name` column's C collation is byte order, and a memcomparable key is
already in byte order ([ADR 0076](0076-c-and-posix-are-the-collations-this-node-has.md)), so the
index key needs no rule of its own — the sort a `name` column gets is the sort PostgreSQL gives it.

**It is not a columnar type.** The columnar format has no `name` tag, and both sides of that seam
(`esker_sql::exec::fragment::column_type` and `esker_store::columnar::decode`) answer `None`, which
keeps a filter over a `name` column on the row side. Inventing a columnar representation for a
63-byte catalog identifier would be a format this node made up; `regtype` is refused there for the
same reason.

**No `_name`.** A real server pairs `name` with `_name` (1003). This node has no
`ColumnType::NameArray` for that row to describe, so `array_oid` answers `0` — `InvalidOid`, which
is what `typarray` holds for a type with no array — rather than pointing at a `pg_type` row that is
not there. An array of `name` is the next unit if anything needs one.

## What this deliberately does not do

* **Collation derivation.** A *cast* to `name` keeps the collation of what it was cast from, so on
  a real server `ORDER BY x::name` over a `text` column sorts in that column's collation and not in
  C. Only a `name` **column** gets 950 by itself. Measured, and left alone: that derivation belongs
  to collation resolution (ADR 0076's ground) and not to this type. This node sorts every string in
  byte order today, so the two agree on a `name` column and can differ on a cast.
* **`pg_typeof` over a bare cast.** `'x'::name` folds to a `Datum::Text`, and `pg_typeof` reads the
  datum — this crate has one representation for `text`, `varchar`, `bpchar` and `name`, and what
  tells them apart is the *column's* declared type. A `name` column reports `name`; a bare cast
  reports `text`. `'x'::varchar` has answered that way since the type surface began. Declared in
  `tests/name_type.rs`.
* **Switching the catalog's own columns to `name`.** `pg_type.typname`, `pg_attribute.attname` and
  `information_schema.columns.column_name` are `name` columns on a real server and are still
  declared `text` here. Every row agrees; what differs is what the catalog says its own columns
  are. That is a change to the catalog views, it is the next unit, and the three statements are
  listed as declared divergences so it cannot be forgotten.
* **`pg_attribute.attlen`.** The column does not exist on this node, so the fixed width is asserted
  through the `RowDescription` instead. Another catalog gap, listed with the ones above.
