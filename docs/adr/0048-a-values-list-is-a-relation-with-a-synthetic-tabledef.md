# 0048 — A `VALUES` list is a relation with a synthetic `TableDef`

Status: accepted (phase 9, `b4-types`)

## Context

`VALUES (1),(2)` stands in two places: where a table goes (`FROM (VALUES …) AS t(a)`) and as a
whole statement. `ActiveRecord` writes both — the first in its inserts and in the `array_agg`
probes the schema dump uses, the second inside `ARRAY(VALUES …)` — and until now the crate refused
each with `0A000 a bare VALUES list is not supported`.

The rows are constants, so nothing has to be read to produce them. The question is what shape they
take inside the planner: three answers were open.

## Options

1. **Desugar into `SELECT 1 UNION ALL SELECT 2`.** No new node, and every rule above a set operation
   applies for free. But the crate has no `UNION ALL` either, the column names would have to be
   invented by the same code anyway, and a ragged list would become a set-operation arity error
   rather than PostgreSQL's `42601 VALUES lists must all be the same length`.
2. **A new expression that returns a table**, like a set-returning function. It is what
   `generate_subscripts` already is — but a function's rows come from evaluating *arguments*, and
   the whole point here is that there are no arguments, only rows.
3. **Its own plan node, and a synthetic `TableDef` above it.** Chosen.

## Decision

`Node::Values { list, columns }` is a source of rows beside `Node::OneRow`, and the `FROM` entry
that produced it carries a synthetic `TableDef` under `crate::catalog::DERIVED_TABLE_ID` — one
column per expression in the **first** row, named `column1`, `column2`, … unless an alias list
renames them.

That `TableDef` is what makes everything above it work without a second code path: `Scope` resolves
`t.a`, `SELECT *` expands, `EXPLAIN` prints the names, and the join machinery probes or
materialises the rows. It is the same trick `plan_derived` and `table_function_def` already turn,
which is the third use of it and the reason it is worth naming as a pattern rather than a
coincidence.

**The types are decided in `crate::exec::values`, not at lowering.** A column's type is its first
row's, the rest are read as it, and reading an expression's type needs a scope the parser does not
have. Lowering therefore raises only what is a *syntax* error on a real server — a ragged list
(`42601`) and an alias list longer than the columns (`42P10`) — and carries the rows up untyped.
Splitting it the other way would put half the answer in each place.

## Consequences

- `VALUES (1),('a')` is `22P02 invalid input syntax for type integer: "a"` — the second row read as
  the first row's type — and not a column of two types. Measured against PostgreSQL 19beta1;
  `tests/corpus/pg19_values_relation.txt` holds all 36 statements.
- A column of nothing but NULL is `text`, which is what an untyped NULL is everywhere in this crate.
- The rows keep the order they were written in. Nothing sorts them; an `ORDER BY` above does.
- ~~The standing constant-width divergence shows through every one of these columns: a bare integer
  constant is `int8` here and `int4` on a real server, so `VALUES (1)` declares `bigint`. Twenty-five
  statements in the corpus are listed for it, one fact each, so that fixing the width fails the test
  rather than passing quietly.~~ **Closed by
  [ADR 0085](0085-an-integer-literal-is-the-narrowest-type-that-holds-it.md)**, and the twenty-five
  entries did exactly what they were written to do: fixing the width failed this test and named all
  twenty-seven of them.
- A comma-separated `FROM` list is still refused, for `VALUES` as for every other relation. The
  `CROSS JOIN` spelling of the same statement answers.
