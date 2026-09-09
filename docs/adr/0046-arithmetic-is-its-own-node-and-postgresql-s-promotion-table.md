# 0046 — Arithmetic is its own expression node, and the promotion table is PostgreSQL's

## Context

Statement 741 of `postgresql_specific_schema.rb` is `CREATE TABLE "defaults" (… "random_number"
integer DEFAULT random() * 100, …)`, and it is the first statement in either schema file that asks
this node to do arithmetic at all. Measured against the node before this change: `2 + 3`, `2 - 3`,
`2 * 3`, `6 / 3`, `7 % 3`, `2 ^ 3` and `abs(-3)` were each `0A000 the operator … is not supported`.
Unary minus on a literal worked and `random()` worked. So the unit was never "float
multiplication"; it was every binary operator on every numeric type.

Two questions had to be answered together:

1. **Where does an operator that yields a value live?** `plan::Expr::Binary` carries
   `plan::BinaryOp`, which is `= <> < <= > >= AND OR` — every one of them yields a boolean. The
   crate has around seventy matches on that enum and each was written knowing the result is a
   boolean.
2. **What type does `left <op> right` have?** A client is told a column's OID in the row
   description before it is sent a single row, so the answer has to be computable at plan time and
   has to agree, exactly, with what the evaluator then produces.

## Options

**Extend `BinaryOp` with `+ - * / % ^`.** One node, one enum, no new vocabulary. It means auditing
every existing match for a case it was never written to have — the pushdown compiler, the index
key deparser, the `EXPLAIN` renderer, the predicate checker — and a match that compiles while
treating `+` as a comparison is a wrong answer rather than a failure.

**A separate `Expr::Arithmetic` node with its own `ArithOp`.** Every existing match stays correct
by construction, and the new variant makes the compiler enumerate exactly the places that have to
decide something about arithmetic. The cost is a second binary-shaped node and the arms that go
with it.

**Compute the result type at evaluation time from the operand values.** No plan-time table needed.
It cannot answer the row description before rows exist, and it makes the declared type and the
produced type two implementations of one rule.

## Decision

**A separate `Expr::Arithmetic { op, left, right, ty }` node with its own `ArithOp` enum, and one
promotion table in `crate::value::arith` used by both the planner and the evaluator.**

`ty` is `Option<ColumnType>`: `Some` once the expression has been resolved against a scope, and
`None` before that. It is a **state, not a default**. A `DEFAULT` expression is evaluated by the
DDL path without ever being resolved against a row, and the evaluator falls back there to the
operands' own types; a placeholder type in that field would make the two paths disagree silently.

The table is PostgreSQL's operator resolution written out, not a widening ladder:

| left | right | result | why a ladder gets it wrong |
|---|---|---|---|
| `int4` | `real` | `double precision` | a ladder ranks `real` above `int4` and answers `real`; there is no `int4 + float4` operator, so both sides go to `float8` |
| `real` | `real` | `real` | the only way to get a `real` back |
| `numeric` | `double precision` | `double precision` | exactness loses to the float, not the other way round |
| `int8` | `numeric` | `numeric` | an integer beside `numeric` stays exact |
| anything | anything | `double precision` for `^` | `2::int4 ^ 3::int4` is a float |

And three rules that are not about types at all:

* **strictness comes first.** `NULL::int4 / 0` is NULL, not `22012` — an operator with a NULL
  operand is never evaluated, so the divisor is never looked at;
* **overflow is at the declared width.** `32767::int2 + 1` is `22003 smallint out of range`, and
  so are `abs((-32768)::int2)`, `-((-2147483648)::int4)` and `int4 min / -1`;
* **an integer constant takes the other side's width when it fits in it**, which is why
  `-x` — lowered to `0 - x` — overflows at `x`'s type rather than promoting to `int8`.

`%` is defined for the integers and for `numeric` and **not** for the floats: `7::float8 %
2::float8` is `42883` on a real server, and this node raises the same. Where PostgreSQL *has* an
operator this node has not built — `date + interval`, `time * 2` — the refusal is `0A000` naming
the operator and its operand types, never `42883`, because claiming an operator does not exist is
a wrong statement about a real server rather than a missing feature (ADR 0031).

## Consequences

* Every match on `plan::Expr` had to grow one arm, which is how the compiler listed the places
  that had a decision to make: the pushdown compiler refuses arithmetic and keeps the filter on
  the row side, the deparser prints it parenthesised as it prints a comparison, and the two
  expression walkers descend into both operands.
* Nine test suites had arithmetic listed as a *declared refusal* and now answer. The parity
  harness fails a listed divergence that starts agreeing, so each entry had to be deleted — the
  ratchet doing its job, and the reason the change could not be quiet.
* `numeric` arithmetic is refused by name and is the next commit of this unit: its scales follow
  rules the floats have none of (`1.50 * 1.50` is `2.2500` — multiplication **adds** the scales),
  and rounding it into a float would answer where the answer is not PostgreSQL's.
* ~~A bare integer constant is `int8` here and `int4` on a real server, so an operator over two
  constants reports `bigint` where PostgreSQL reports `integer`.~~ **Closed by
  [ADR 0085](0085-an-integer-literal-is-the-narrowest-type-that-holds-it.md)**: a literal's declared
  type is now the narrowest that holds it, and `arithmetic_type` takes the wider of its two
  operands, so `1 + 1` is an `integer` and `1 + 3000000000` a `bigint`. The nine entries this
  paragraph explained are gone from `tests/arithmetic.rs`.
