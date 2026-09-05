# 0077 — `regtype` is an oid that prints as a name

**Status**: accepted · **Date**: 2026-09-05

## Context

Six tests in run 92 come from one statement, `ActiveRecord`'s
`can_perform_case_insensitive_comparison_for?` (`postgresql_adapter.rb:1081`):

```sql
SELECT exists(
  SELECT * FROM pg_proc
  WHERE proname = 'lower' AND proargtypes = ARRAY[$type::regtype]::oidvector
) OR exists(
  SELECT * FROM pg_proc INNER JOIN pg_cast ON ARRAY[casttarget]::oidvector = proargtypes
  WHERE proname = 'lower' AND castsource = $type::regtype
)
```

Closing the ARRAY constructor turned that one refusal into a three-gap census, and two of the
three are the same root:

```
ARRAY['character varying'::regtype]        ok
ARRAY[casttarget] FROM pg_cast             ok
ARRAY[…]::oidvector                        0A000 the type oidvector is not supported
castsource = 'character varying'::regtype  42883 operator does not exist: bigint = text
```

`'x'::regtype` lowers to the type's **name, as text**. That was a deliberate trade, written down
where it was made: this node had no `regtype`, the name is what a client sees, and
`SELECT 'int4'::regtype` answers `integer` — exactly right. The composed `::oid` was matched as a
pair rather than built, because `'integer'::oid` is `22P02` on a real server and a text-to-oid cast
would have allowed what PostgreSQL forbids.

## What was measured

`tests/captures/pg19_regtype.txt`, 19beta1, one rolled-back session. Five facts, and the first
decides the model:

```
'text'::regtype < 'int4'::regtype   ->  f      25 < 23; alphabetically int4 would sort first
'text'::regtype = 25                ->  t      it compares against a bare integer, uncast
23::regtype                         ->  integer
999999::regtype                     ->  999999 an oid that is not a type is not an error
'nosuchtype'::regtype               ->  42704  a name that is not a type is
ARRAY['text'::regtype]::oidvector   ->  25     digits, which is what proargtypes holds
```

**A `regtype` orders by its oid and not by its name.** No text-shaped model reproduces that, and no
amount of special-casing at the comparison would: the ordering is a property of the value.

The control matters as much as the cases. The probe is **true** for `character varying` and for
`text` and **false** for `integer` — `lower(integer)` does not exist and `integer` casts to nothing
`lower` takes. A model that answered `t` to everything would pass the two tests everybody looks at.

## Decision

**`regtype` becomes a type of its own: the oid is the value, the name is the output function.**

* `ColumnType::RegType`, with its array variant, and `Datum::RegType` carrying the oid **and** the
  name it renders as. Equality and ordering read the oid alone, which is what makes `= 25` and the
  `int4`/`text` ordering come out right.
* The name is carried rather than derived at render time because deriving it needs the catalog, and
  the value layer must not have one (invariant 7). It is resolved where the value is produced,
  which is the seam `CatalogFunc::UserRegType` already sits on — so `'color'::regtype` over a type
  a `CREATE TYPE` made keeps printing `color`, which it does today and must not lose.
* An oid with no type prints as the number, because that is what a real server does — not a
  fallback, a measured answer.
* `oidvector` is the same value in a list: `ARRAY[…]::oidvector` renders the elements' oids space
  separated, which is exactly what `pg_proc.proargtypes` holds here already.

`regtype` is not offered as a **column** type in this change. Nothing in the suite declares one,
and a stored `regtype` would need the row codec and the columnar reverse map to agree about a value
whose printed form depends on the catalog. Refused by name, which is the honest gap.

## Consequences

* The six ARRAY tests can answer, and they answer `false` for an integer column as well as `true`
  for the two text ones — the control is what says the answer is computed rather than assumed.
* **`pg_typeof('integer'::regtype)` becomes `regtype`** where it has been `text`. That is a
  declared divergence being deleted, not a new one: several corpora list it, and the ratchet will
  name each as it starts agreeing.
* A new `ColumnType` has its five places plus the columnar reverse map, which the compiler
  enumerates except for the last one — that is the one to check by hand.
* The alternative, comparing an `oid` against a `text` at the comparison site, is the shortcut this
  project retired: it answers the one statement that motivated it, leaves `::oidvector` unbuilt,
  and leaves `'text'::regtype < 'int4'::regtype` wrong for anybody who asks.
* **A user-defined type's `regtype` was still text, and is not any more.** This was written as the
  half not built, and run 97 found the cost within one round: the probe stopped failing on
  `oidvector` and started failing on `type "example_type" does not exist`, because it now *reached*
  a `CREATE DOMAIN` through `regtype`. `CatalogFunc::UserRegType` answers a `Datum::RegType` now —
  the catalog read that resolves the name has the oid in hand anyway — and two things followed for
  free: `pg_typeof('mood'::regtype)` is `regtype`, and `WHERE enumtypid = 'mood'::regtype`
  compares, which this file had recorded as the case position could not decide.
  **`pg_enum.enumtypid` moved to `oid` with it**, for the reason `pg_cast.castsource` did: it is
  what a `regtype` is compared against.

  The lesson is about the *shape* of a half-built decision, not this one: making the built-in half
  work moved the failure one step further along the same statement, where it looked like a new
  defect and was the recorded gap arriving. A declared not-built half should be expected to
  surface as somebody else's regression.
* `format_type` took an oid and had to learn that a `regtype` is one. Four corpora caught that in a
  single run, which is what one shared accessor with a missing arm looks like — and is why the
  arm sits beside `Datum::Oid`'s rather than in a second function.
