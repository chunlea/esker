# 0092 — `void` is a type, and it is this vocabulary's first pseudo-type

Status: accepted · 2026-09-09

## Context

`pg_advisory_lock(1)` returns a `void` on a real server. This node had no such type, so the call
folded to an empty string and the `RowDescription` said `text` (25) where PostgreSQL says **2278**.
The *value* was already right — a void renders as zero characters and is **not** NULL, which
`AdvisoryCall::is_void` had recorded with its measurement — so what was missing was only the
declared type.

The queue reached it as "give the advisory functions a real type". The capture turned that into
three findings instead of one.

## Decision

**1. `ColumnType::Void` exists, oid 2278, and it is a *pseudo*-type.** `typtype` is `p` and
`typcategory` `P`, measured, and its `typarray` is 0. That pair is what separates it from every
other member of this vocabulary: it is in `ColumnType::ALL` because it has a `pg_type` row that a
client can read, not because anything is stored as one.

**2. No column may be declared one: `42P16 column "c" has pseudo-type void`.** Measured, and
PostgreSQL names the **column**, so the check is per column — `(c int, d void)` names `d` — and it
applies to `ALTER COLUMN … TYPE` and `ADD COLUMN` as well as to `CREATE TABLE`. This is a refusal
the unit had to *add*: giving the type a name made `CREATE TABLE v (c void)` succeed, which is the
regression a new type in a total vocabulary invites, and the reason `is_index_key`, both row-codec
generators and `tests/corpus/pg19_order.txt`'s exclusion list all name it too.

**3. Half the advisory family is a `boolean`.** Measured from `pg_proc.prorettype` for all eleven
advisory functions a real server has, not from the two probes that started the unit:

```text
  pg_advisory_lock            void      pg_advisory_unlock          boolean
  pg_advisory_lock_shared     void      pg_advisory_unlock_shared   boolean
  pg_advisory_unlock_all      void      pg_try_advisory_lock        boolean
  pg_advisory_xact_lock       void      pg_try_advisory_xact_lock   boolean
```

**The split is "is there anything to report".** The blocking acquires and `unlock_all` can only
succeed; the `try_` forms and the single-lock `unlock`s can fail to do what was asked. A rule that
made the whole family `void` would be wrong for four of the seven this node has, and
`connection_test.rb#test_get_and_release_advisory_lock` reads exactly the boolean half.
`AdvisoryCall::is_void` already drew that line for the *value*; it now draws it for the type.

**4. The folded call keeps a `Cast` node**, the mechanism
[ADR 0086](0086-a-folded-cast-keeps-the-type-it-named.md) introduced. An advisory call is executed
before the plan is built and replaced by its value, and the value is a `Datum::Text("")` — so a bare
literal would be declared `text`, and the *executed* path said `text` while `Describe` said `void`.
One expression with two answers is the shape this queue has now removed three times; the node that
carries the type is what keeps the two paths from being two rules.

## Consequences

* `pg_advisory_lock(1)` and `pg_advisory_unlock_all()` are declared 2278 on both protocol paths;
  `pg_advisory_unlock(1)` and `pg_try_advisory_lock(2)` stay 16.
* `pg_type` gained the `void` row and `format_type(2278, -1)` is `void`.
* Tags additive as ever: catalog record 101, columnar 102 — and **neither will ever appear in a
  record or a chunk**, because no column is a `void`. They exist because the vocabulary is total,
  which is the property that keeps the two tag spaces in step and makes a missing type a compile
  error rather than a silent default.
* `Datum` gained nothing: a void's value is a `Datum::Text("")`, which is also why `IS NULL` answers
  `f` — a NULL there would have been a wrong answer rather than a missing type.
* **What is still declared** is one fact, and it is the same one three units in a row have named:
  `pg_typeof` reads the *datum*, so it answers `text` for the two calls whose type is carried by the
  expression. The `RowDescription` is PostgreSQL's, which is what a client reads. Closing it means
  resolving `pg_typeof` against the declared type at plan time — its own unit, and it would take
  [ADR 0077](0077-regtype-is-an-oid-that-prints-as-a-name.md)'s `regtype`/`text` half with it.
* `pg_notify` is still `0A000` by name and `pg_proc` still has no `prorettype` column; both are in
  the corpus as declared divergences, the second because it is the *evidence* for decision 3.
