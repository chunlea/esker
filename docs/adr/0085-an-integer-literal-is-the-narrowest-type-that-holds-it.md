# 0085 — An integer literal is the narrowest type that holds it, and only its *type* narrows

Status: accepted · 2026-09-09

## Context

An unadorned integer constant was an `int8` everywhere in this crate. On a real server it is an
`integer` when it fits one and a `bigint` when it does not, and that one word travelled further
than it looks:

* `SELECT 1` was described as `bigint`, so a driver probing a connection was told the wrong type
  by the first statement it sends.
* `1 + 1`, `12 | 10`, `COALESCE(NULL::integer, 0)`, `VALUES (1)`, `ARRAY[1,2,3]`,
  `generate_series(1,3)` and `unnest(ARRAY[1,2,3])` all inherited it.
* Every refusal that names its operands named `bigint` where a real server names `integer` —
  `invalid input syntax for type bigint: "x"`, `UNION types bigint and text cannot be matched`.
* `sum(1)` was `sum(bigint)`, which is a `numeric`, where a real server's `sum(integer)` is a
  `bigint`.

It was the single most-repeated declared divergence in the corpora: **115 entries across 22 test
files** said this and nothing else, and [ADR 0046](0046-arithmetic-is-its-own-node-and-postgresql-s-promotion-table.md)
and [ADR 0048](0048-a-values-list-is-a-relation-with-a-synthetic-tabledef.md) each recorded it as a
consequence they could not avoid.

## Decision

**An integer literal's declared type is `int4` when its value fits an `i32`, and `int8` otherwise.
Its *datum* is unchanged: still an `i64`.**

The split is the whole decision. Narrowing the value as well was tried first and broke function
resolution — `generate_subscripts(conkey, 1)` stopped resolving, because this node's signatures
take an `int8` and an `Int4` datum no longer matched one. Types climb; values do not.

Four sites carry it:

* `exec::query::literal_type` and `expr_type` — what a column of a literal is called.
* `exec::query::arithmetic_type` — two integer operands take the wider of their two widths, so
  `1 + 1` is an `integer` and `1 + 3000000000` a `bigint`.
* `parse::lower`'s folded `ARRAY[…]` constructor — the element type, with the wider integer
  winning across elements so `ARRAY[1,3000000000]` is still a `bigint[]`.
* `exec::cursor`'s `pg_typeof` guard — that function reads the *datum*, which this decision
  deliberately leaves alone, so a literal argument is answered from the literal.

Two consequences had to be built rather than inherited:

* **`sum` over an `int4`-declared value takes an `Int8` datum.** The accumulator is chosen from the
  declared type — `sum(int4)` is a `bigint`, which is what a real server answers — and the value
  arriving in it is still an `i64`. Checked rather than bare: the "a sum of `i32`s is bounded by
  the row count" argument does not cover an `i64`.
* **An integer value compares like an integer literal, whatever width it arrived as.**
  `Datum::fits` is the *assignment* rule and is right to refuse an `int4` into an `int8` column;
  comparison is a different question and PostgreSQL has `int84eq`. Without this,
  `id = ANY(ARRAY[1,3])` against a `bigint` key refused itself the moment the constructor started
  folding to `int4`.

## Consequences

* 115 divergence entries across 22 files deleted, and the `UNMEASURED` budget lowered 186 → 163.
  Every deletion was forced by the harness's second ratchet rule rather than chosen.
* `SELECT id FROM i4 WHERE n = 10::int8` answers its row: the two integer widths compare, which
  `tests/int4.rs` had been carrying a refusal as evidence for.
* `.slt` files' `I` covers oids 20, 21 and 23 — the letter is the shape of the printed value, and
  all three integer widths print the same characters.
* **What this does not close, and it is the same fact each time**: a refusal raised by the
  *evaluator* names the datum's width, because that is all it has. `SELECT 1 || 2` is
  `operator does not exist: bigint || bigint` and `'1.00'::money + 1` is `money + bigint`, where a
  real server says `integer` in both. `SELECT i4 || 2` shows the seam exactly — `integer || bigint`,
  the column named from its declaration and the literal from its datum. Closing it means narrowing
  the value, which is a change to what a bare integer *is* and needs an implicit `int4` → `int8`
  widening in function resolution first. Listed in `tests/concat.rs` and `tests/money.rs`.
