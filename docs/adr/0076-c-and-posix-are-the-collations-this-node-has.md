# 0076 — `C` and `POSIX` are the collations this node has

**Status**: accepted · **Date**: 2026-09-05

## Context

`adapters/postgresql/collation_test.rb` declares `t.string :string_c, collation: "C"` and
`t.text :text_posix, collation: "POSIX"`, reads the collation back off each column, and expects the
schema dumper to print it. `unsafe_raw_sql_test.rb` adds a sixth use, in an expression:
`ORDER BY author_id, title COLLATE "C" DESC`. All six answered
`0A000 COLLATE "C" is not supported`.

A collation is an ordering for text. Accepting the *word* and ignoring it would be the worst
available answer: `ORDER BY` would return rows in an order the client did not ask for, silently and
correctly-looking, which is the failure mode ADR 0031 ranks above every gap.

So the question is not "can we parse `COLLATE`" but "which orderings does this node actually have".

## What was measured

On 19beta1, in one rolled-back session (`tests/captures/pg19_collation.txt`):

```
ORDER BY plain              ->  a, A, b, B     the database's default collation, a locale
ORDER BY plain COLLATE "C"  ->  A, B, a, b     byte order
```

**It is PostgreSQL's default that this node does not have, not `C`.** Keys here are
memcomparable — a text key sorts by its bytes — so `COLLATE "C"` asks for exactly the ordering
every index and every `ORDER BY` in this node already produces. `POSIX` is the same ordering under
a second name; `pg_collation` gives them oids 950 and 951, both `collprovider` `c` and both
`collisdeterministic`.

The catalog side, also measured: `typcollation` is 100 for `text`, `varchar`, `bpchar` **and their
array types** — `_text` is 100 and `text[] COLLATE "C"` is accepted, which follows, since comparing
two arrays compares their elements — and **0** for `int4`, `uuid` and `char`. A column with no
`COLLATE` inherits the type's 100, so `ActiveRecord`'s read-back —
`LEFT JOIN pg_collation c ON a.attcollation = c.oid AND a.attcollation <> t.typcollation` — reports
a name only for the columns that named one.

Three more facts that reasoning would have got wrong, each measured:

* **Two sqlstates, and the *name* is checked first.** `a integer COLLATE "nope"` is
  `42704 collation "nope" for encoding "UTF8" does not exist`, not the type's error, even though
  `integer` could not have taken `C` either. A name the server has on a non-collatable type is
  `42804 collations are not supported by type integer`.
* **`ALTER COLUMN … TYPE` with no `COLLATE` clears the one the column had.** The column takes its
  new type's collation, so the field is assigned and not merged.
* **`SELECT 'x' COLLATE "C"` is accepted.** An `unknown` literal takes the type it is used with, so
  there is nothing for the clause to refuse.

## Decision

**`C` and `POSIX` are accepted, and every other collation name is refused.**

They are accepted because they name the ordering this node has, not because the suite asks for
them. The column's collation is recorded (catalog format version 35), reported through
`pg_collation` and `attcollation`, and printed back by the schema dumper — and `COLLATE "C"` in an
expression is a no-op, because byte order is what the expression would have got anyway.

Any other name — `en_US.UTF-8`, `und-x-icu`, a database's own default under a different locale — is
`42704`, the same sqlstate a real server gives for a collation it does not have. That is honest:
this node genuinely does not have those orderings, and it will not have them until something links
a locale library, which the dependency policy does not allow.

## What it cost to build

Two places needed more than a field. `sqlparser` 0.62.0's `AlterColumnOperation::SetDataType` has
no collation, so `change_column … collation:` was a *syntax error* rather than a clause the
lowering declined — and the refusal table named it "ALTER TABLE ... ALTER COLUMN", which reads like
a missing feature and is not. The clause now comes off the source before the parse and travels on
`Parsed` (`strip_alter_column_collation`), which is this module's standing arrangement for a
construct the parser cannot read.

And `pg_type.typcollation` had to move off zero in the same change: with the type saying 0 and the
column saying 100, `attcollation <> typcollation` is true for a column that named nothing, so every
plain `text` column reported the collation `default`. The two numbers are one fact and only work
together.

## Consequences

* The six tests pass for the right reason: the collation is stored, reported and honoured, and the
  one that orders by it gets byte order because that is what `C` means.
* **A database whose default collation is a locale sorts differently here**, and that difference is
  not new — it is the pre-existing gap this decision names for the first time. `ORDER BY plain`
  gives `a, A, b, B` on the oracle and `A, B, a, b` here. It is declared with its capture line
  rather than left implicit, and it is the one thing in this area a reader should not be surprised
  by twice.
* Refusing every other name means a migration that asks for one fails loudly instead of ordering
  rows wrongly. A future locale collation is a dependency decision and an ADR of its own, not a
  quiet extension of this one.
* **One gap is declared rather than closed**: `COLLATE` on a *column* of a non-collatable type is
  accepted where PostgreSQL raises `42804`. A literal carries its type and is checked where the
  clause is lowered; a column has none until the executor resolves it against a scope, two layers
  further on. Closing it means a `plan::Expr` node that resolution either collapses or refuses —
  one that exists only to be deleted, in every match over that enum. Declared in
  `tests/corpus/pg19_collation.txt` with its capture line.
* `collisdeterministic` is `t` for both, and no non-deterministic collation is accepted — those
  change what *equality* means, not only ordering, and equality is what every index and every
  primary key in this node is built on.
