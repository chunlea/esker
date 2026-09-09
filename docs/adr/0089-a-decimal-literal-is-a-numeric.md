# 0089 — A decimal literal is a `numeric`, and a `CASE`'s branches fold to their common type

Status: accepted · 2026-09-09

## Context

r1's 349-probe wire sweep filed four rows under "`COALESCE`/`CASE` pick a common type wider than
PostgreSQL's". Two closed with the literal ladder's `int4` rung
([ADR 0087](0087-an-integer-literal-is-the-narrowest-type-that-holds-it.md)). The remaining two were
one fact and it was not about `CASE`: this crate read `1.5` as a `float8` where a real server reads
it as a `numeric`, so the common type of `1` and `1.5` was `double precision` here.

Unlike the `int4` rung this was **a wrong value, not only a wrong type**. A narrowed integer prints
the same characters; a float does not:

```text
                     node                   pg19
  SELECT 1.10        1.1                    1.10        a numeric keeps its scale
  SELECT 0.1 + 0.2   0.30000000000000004    0.3
  SELECT 10.0/3.0    3.3333333333333335     3.3333333333333333
```

`docs/plans/phase-9-rails.md` recorded the trade with its own counterexample — `0.1 =
'0.1000000000000000000001'` is `f` on a real server and was `t` here — and called it ADR 0031's
numeric backlog. The `numeric` type it was waiting for has existed since
[ADR 0045](0045-numeric-is-a-decimal-and-its-key-is-normalised.md); this is that backlog being paid.

## Decision

**1. An unadorned decimal literal is a `numeric`**, in its declared type *and* in its datum. Four
sites: `exec::query::literal_type`, `expr_type`, the row evaluator's `Literal::Decimal` arm, and
`exec::assign`. Every other site in the crate already said `numeric` — `plan::expr`'s `type_name`,
`comparable_with` and `resolve`, and `parse::lower`'s cast target — which is why the surface is four
lines rather than sixteen.

**A `float8` still wins over a `numeric`**: `CASE WHEN true THEN 1 ELSE 1.5::float8 END` is a
`double precision` on both. The promotion table is unchanged; only which type a bare `1.5` *is*
moved.

**2. A `CASE`'s branches fold to their common type**, through the same `unify` the declared type
already used. The `resolve` arm took the **first** branch that carried a type and only checked that
the rest were in its family, and the `ELSE` is walked first — so
`CASE WHEN true THEN 1.10 ELSE 2 END` settled on `integer` and then refused to assign `1.10` to it,
while `carried_type` had always answered `numeric` for the same expression. **Two rules for one
question, and they disagreed**; the value path now folds the way the declared type does.

That second half is the one worth keeping: it was invisible until the literal changed type, because
before this every decimal and every integer met in the same `float8`.

## Consequences

* The four rows r1 filed as group D answer PostgreSQL's OIDs over the wire, and so do
  `COALESCE(1::int4, 1::int8)`, `COALESCE(1::numeric, 1::float8)` and the two `CASE` forms beside
  them — the branch fold, not the literal.
* Nine declared divergences deleted and three moved to `types`: `'2020-01-01'::date + 1.5`,
  `'1.00'::money = 1.00`, `1.5 + 1.5`, `1.5 * 2`, `0.1 + 0.2`, `0.1 =
  '0.1000000000000000000001'`, `1.5 = 'x'`, and both halves of `date + 1.5`. The counterexample
  phase-9 recorded now answers `f`, as it does on a real server.
* Arithmetic over decimal literals is exact and carries `numeric`'s scale rules — multiplication
  **adds** the scales, division takes `div_scale`'s sixteen significant digits (ADR 0045) — which is
  what makes `sum` over `(1.10),(2.20)` print `3.30`.
* **What is left, and it is one word.** `SELECT 1.5 | 2` reads `numeric | numeric` where a real
  server reads `numeric | integer`: this crate resolves both operands to one type before deciding no
  operator exists, so a refusal has one type to name and PostgreSQL names the two it was given. The
  left half of that message was wrong before this and is right now. Listed in `tests/bitwise.rs`.
* `pg_typeof` still reads the *datum*, so it answers the chosen branch's type where the column is
  declared the common one. The `RowDescription` is PostgreSQL's, which is what a client reads; the
  same seam [ADR 0086](0086-a-folded-cast-keeps-the-type-it-named.md) named, and closing it is its
  own unit that would take ADR 0077's `regtype`/`text` half with it.
* `2.0 ^ 3` is a `double precision` here where a real server keeps it a `numeric` — the one
  exception `value::arith::result_type` writes down, and its own unit.
