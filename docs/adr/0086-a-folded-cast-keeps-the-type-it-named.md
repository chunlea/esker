# 0086 — A folded cast keeps the type it named, and `name` gets its array

Status: accepted · 2026-09-09

## Context

Run 106 lost ten tests to one alias. `ActiveRecord` reads an enum's labels with
`postgresql_adapter.rb:539`, which ends `array_agg(enum.enumlabel ORDER BY enum.enumsortorder)`.
`pg_enum.enumlabel` is a `name` column, so a real server declares that output column **`name[]`,
OID 1003**. This node had no array of `name`, so `array_agg` fell back to a *scalar* `text` — not
even `text[]` — and the bytes went out identical under OID 25. `ActiveRecord` decodes by the
declared OID: given an array OID it parses the literal into a Ruby array, given `text` it keeps the
string, and the dump read `create_enum "mood", "{sad,okay,happy}"` where
`["sad", "okay", "happy"]` was expected. One of the ten,
`test_schema_dump_keeps_enum_intact_if_it_contains_comma`, cannot be worked around client-side at
all: once the array is a string a label containing a comma is indistinguishable from a separator.

**This is the third instance of one pattern** — run 98's `->` over a `json` column (OID 25 for
114), run 105's enum column over the extended protocol (21 for the enum's own oid), this. Right
bytes, wrong declared type, invisible to `psql` and to `pg_typeof`, visible only in a
`RowDescription`. r1's 349-probe wire sweep found 26 such disagreements, of which nine were this
one type.

Underneath the missing array type sat a second, larger fact: **a folded cast threw its type away.**
`'x'::name` lowers to a constant, and `text`, `varchar`, `bpchar`, `name`, `json`, `jsonb` and
`xml` are all one `Datum::Text`, so the constant could not say which type it was. A *column* of any
of them reported correctly, because a column's type comes from the catalog; only a bare cast lost
it. That is why no corpus had ever caught it — `tests/json.rs` carried seventeen entries recording
exactly this and calling it "giving `jsonb` a `Datum` of its own closes it".

## Decision

**1. `name[]` is a type**, `ColumnType::NameArray`, oid 1003, `typname` `_name`, element 19,
delimiter `,`. Additive on both tag spaces, as [ADR 0084](0084-name-is-a-stored-type-and-its-tag-is-additive.md)
was: catalog record tag 95, columnar tag 96, no existing byte reinterpreted. Its comparison family
is its own and not `text[]`'s — measured, `'{a,b}'::name[] = '{a,b}'::text[]` is `42883` on a real
server even though `'x'::text = ANY('{x,y}'::name[])` is `t`.

**2. A folded cast keeps an `Expr::Cast` node when its value cannot speak for itself.** The datum
below it is already the target type's — the node is a no-op on the value — and it is what
`expr_type` reads. Kept only when `value.column_type() != Some(ty)`, so nothing changes for a cast
whose type its datum already carries.

**3. The session functions answer `name`**: `current_schema()`, `current_database()` and
`current_user` are `name` on a real server and `current_schemas(bool)` is `name[]`;
`current_setting()` is a `text` and stays one. `current_schemas` folds to a real `Datum::Array` of
`Name` rather than to the *text* of an array, and `current_schema()` under a `search_path` that
resolves to nothing is a **typed** NULL, because `pg_typeof` still says `name` there.

**4. `min`/`max` of a `name` is a `text`** — the one rule that runs away from the type. A real
server has no `min(name)`; it coerces the argument. Measured, and it is the rule
[ADR 0031](0031-rails-compatibility-is-measured.md) turned into a law after `bool`: the aggregate
set is per type and cannot be derived from whether the type is ordered.

## Consequences

* The enum-labels query declares `name[]`, and run 106's ten failures are addressed at the wire.
* **Decision 2 closed groups r1 had filed separately.** `tests/json.rs`'s seventeen entries went in
  one change and needed no new `Datum`; so did nine `xml` entries, `'…'::uuid::varchar`,
  `'…'::macaddr::varchar`, and the `array_agg(attname)` inside `generate_subscripts`' constraint
  query. 61 declared divergences deleted in total across sixteen files.
* `pg_type` gained the `_name` row, so `ActiveRecord`'s array-type query answers thirty-six rows
  rather than thirty-five, and `name` left `tests/array_delimiter.rs`'s list of base types with no
  array.
* `name[]`'s key ordering is byte order, so its capitals sort before its lower case where `text[]`'s
  do not — measured on the oracle and added to `tests/corpus/pg19_order.txt` rather than copied
  from `text[]`'s line, which has no capitals in it and could not have shown the difference.
* **What is left, and it is one fact**: `pg_typeof` reads the *datum*, so it answers `text` for
  every shape whose type lives in the expression — `pg_typeof('x'::name)`, `pg_typeof(unnest(…))`,
  `pg_typeof(coalesce('a'::name, 'b'::name))`. The `RowDescription` for each of those statements is
  correct, which is what a client reads. Closing it means resolving `pg_typeof` at plan time, where
  a scope exists; that would close the `regtype`/`text` half ([ADR 0077](0077-regtype-is-an-oid-that-prints-as-a-name.md))
  at the same time and is its own unit. Listed in `tests/name_array.rs`.
* `name`'s `typelem` is `"char"` (18) on a real server and 0 here, because this node has no `"char"`
  type for the pointer to name — the same call `box`'s `typelem` already makes, and the reason
  `tests/array_delimiter.rs` tells an array from a base type by `typinput` rather than `typelem`.
