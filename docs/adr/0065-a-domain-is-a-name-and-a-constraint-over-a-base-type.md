# 0065 — A domain is a name and a constraint over a base type

- Status: accepted
- Date: 2026-09-04
- Extends [ADR 0050](0050-a-user-defined-type-is-a-value.md), whose mechanism this reuses, and
  follows [ADR 0031](0031-rails-compatibility-is-measured.md)'s rule that what gets built is what a
  capture shows.

## Context

Run 70 ranked `CREATE DOMAIN is not supported` at 8 tests over two files. The question a domain
raises is not whether to store one but **what a column of it is**, and `domain_test.rb` asks it in
one assertion pair:

```ruby
assert_equal :decimal,        column.type      # a numeric
assert_equal "custom_money",  column.sql_type  # a domain
```

Both, at once, of one column over `CREATE DOMAIN custom_money AS numeric(8,2)`. A node that
answered either one for both would pass half the file.

## Decision

**A domain is a fourth `TypeKind`, beside the range, the composite and the enum — and its value is
its base type's.**

`resolve_user_type` returns `(base, Some(oid))` for one, which is the same shape an enum returns
(`(Int2, oid)`) and a range returns (`(representation, oid)`). Nothing below that line can tell a
`custom_money` column from the `numeric(8,2)` it stands for; the name comes back out of the catalog,
which is invariant 7 kept rather than worked around, exactly as ADR 0050 arranged for enums.

So the whole of a domain lives in three places:

* the **record** — a fourth kind byte, version 32, holding the base type, its typmod and the
  constraints;
* the **catalog views** — `pg_type.typtype` `d` with `typbasetype`, `typnotnull` and `typdefault`
  beside it; a new `information_schema.domains`; and a `domain_name` column on
  `information_schema.columns`;
* the **write path** — the two constraints a domain can carry.

## What the capture decided

* **`data_type` and `udt_name` report the *base* type, not `USER-DEFINED`.** Measured: a
  `custom_money` column says `numeric` for both, and `domain_name` is the only column naming the
  domain. That is precisely what makes ActiveRecord read `:decimal` and `custom_money` at once, and
  it is the one place a domain differs from an enum, which *does* say `USER-DEFINED`.
* **The column takes the domain's typmod.** A value too wide for `custom_money` is
  `22003 numeric field overflow … precision 8, scale 2` — the **base type's** error, naming the base
  type's numbers and not the domain.
* **The two constraints name the domain and nothing else.** `23502 domain dm_pos does not allow
  null values` names neither the column nor the table, and `23514 value for domain dm_pos violates
  check constraint "dm_pos_check"` prints no row — both unlike the table-level errors of the same
  codes.
* **A domain's `DEFAULT` fills a column that declares none**, and is set as an *expression* so it
  goes through the same folding a written `DEFAULT` does.
* **A domain takes a schema.** `schema_test.rb` creates `schema_1.text`, so the name is stored
  qualified the way a relation's is and `typnamespace` reports where it lives.

## Consequences

- Catalog record version **32**. A record written earlier has one of the three older kind bytes and
  reads back unchanged; the kind byte already selected what followed it, so a fourth kind adds a
  case and rewrites nothing.
- `CHECK (VALUE …)` is stored as text and evaluated by rewriting `VALUE` to the column, which is the
  same evaluator a table's `CHECK` goes through. A domain constraint is checked **before** the
  table's, so a column of a `NOT NULL` domain reports the domain.
- `sqlparser` 0.62.0 reads a domain's `DEFAULT` and its `CHECK` and stops at `NOT NULL`, so those
  two words are cut out of the source and the fact travels on `Parsed` — the third clause in this
  crate to use that mechanism, after `UNLOGGED` and `WITH [NO] DATA`.
- **A domain does not shadow a built-in type's name**, and that is declared rather than hidden: with
  `search_path = schema_1, pg_catalog`, a column declared `text` is the domain on a real server and
  the built-in here. Type names are resolved before the catalog is consulted, so shadowing needs
  type resolution to walk the `search_path` ahead of the built-in vocabulary. Its own unit;
  `schema_test.rb` raises the domain shape zero times without it.
