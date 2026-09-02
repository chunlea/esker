# 0042 — A subquery is a plan node, run once or per outer row

Status: **accepted**, and built — `docs/plans/phase-12-subquery.md`, units 1–5.
`crates/esker-sql/src/plan/subquery.rs` and `plan/cte.rs` are the types,
`exec/subquery.rs` the three passes, and five captures are the specification:
`pg19_subquery_expr.txt`, `pg19_subquery_from.txt`, `pg19_cte.txt`,
`pg19_subquery_correlated.txt` and `pg19_subquery_activerecord.txt`.

## Context

`docs/plans/phase-9-rails.md` §5 measured what stands between `ActiveRecord` and a booting
connection and left one line about this phase:

> No window functions, no CTEs, no subqueries in FROM … a subquery is the next thing after this
> phase.

A subquery is four features wearing one word — a value (`(SELECT …)`, `EXISTS`, `IN`, `ANY`/`ALL`),
a relation (`FROM (SELECT …) AS t`), a name for a relation (`WITH a AS (…)`), and any of those
reading the row outside it — and the way they are usually built is four mechanisms. This ADR
records that they are one, what that one is, and the three places the capture said something a
reader would not have.

## Decision 1: the only question is how many times it runs

A subquery is a **plan node**, and everything else follows from one field.

**Uncorrelated — once, before the cursor opens.** Nothing in it depends on the outer row, so its
answer is a constant for the whole statement. `crate::exec::subquery::resolve` walks the plan
before `Cursor::open` and writes the values it produced into the node they came from. This is not
a new mechanism: it is the shape `exec::fragment::resolve` already uses to fill a `Node::Columnar`
from the fragments, and it is what the executor already does with a `nextval` — run it once,
substitute, and let the row evaluator see a constant. Running it *from* the evaluator instead would
run it per row: `WHERE id IN (SELECT a_id FROM b)` over a million-row table would open the inner
scan a million times for an answer that cannot change, and no cache in front of that is as simple
as not asking.

**Correlated — once per outer row.** The sub-plan is copied and every outer reference in it that
names this row is replaced by the value it names, so what a cursor is opened on has none left and
is an ordinary plan. That copy is what a nested loop costs and it is the honest price: the answer
really is a different answer per row.

An unresolved subquery reaching the row evaluator is a **bug that says so**, the way an unresolved
`Columnar` node does, rather than answering "no rows" — because no rows from a scalar subquery is a
**NULL**, and a NULL looks like an answer.

### Memory is a bound, not a hope

Every one of these buffers rows, so every one is bounded where `Node::Sort` already is (`53400`
past `SORT_LIMIT`). Two read fewer by construction rather than by luck: `EXISTS` stops at the
**first** row, because the only question asked of the result is whether it is empty; a scalar stops
at the **second**, because the second row *is* the `21000` and a third would be read for nobody.
There is no `Vec` in this phase that grows with the input without a limit above it.

## Decision 2: a derived table is a synthetic relation, and a CTE is a derived table

`FROM (SELECT …) AS t` becomes a **`TableDef` this crate builds** — one column per output column of
the sub-select, under the reserved id `catalog::DERIVED_TABLE_ID` — with the sub-select's own plan
where an access path would go. With that, `Scope` resolves a name, `SELECT *` expands, an alias
replaces the relation's name, `EXPLAIN` prints the names the user typed and the join machinery
materialises, all without learning that nothing stores these rows. The alternative is a second
name-resolution path for relations that are not tables, and every rule in the first one written
twice.

**A non-recursive CTE is then inlined at each reference**, which makes it one of these and costs no
executor at all. That is not an approximation: PostgreSQL 12+ inlines a single-reference CTE and
materialises a multi-reference one, `MATERIALIZED` / `NOT MATERIALIZED` ask for one or the other by
hand, and **all three return the same rows** — measured — because a non-recursive CTE over a
read-only statement has nothing in it that can run twice to different effect. What inlining chooses
is a *plan*, and [ADR 0031](0031-rails-compatibility-is-measured.md) matches observable behaviour.

The cost is stated rather than hidden: **a CTE referenced twice is read twice.**
`TODO(post-v1)`: materialise a multi-reference CTE once, which is a shared plan node and a lifetime
question this phase does not need to answer.

## Decision 3: an outer reference carries how far out it reaches

`Expr::Outer { level, at, … }`. A sub-plan inside a sub-plan has **two** rows outside it and both
can be named — measured, a three-level `EXISTS` chain where the innermost query names the middle
table and the outermost one — and both are `bigint`, so getting it wrong is a wrong number rather
than a type error. Substitution matches `level` against the depth it has descended to, which is why
nothing is ever renumbered: an `Outer { level: 2 }` is matched when the substitution for that row
reaches depth 2, and the `Outer { level: 1 }` beside it is left for the run that will supply it.

Two consequences that had to be refused rather than made to work:

* **A statement with a correlated subquery never swaps its join.** `drive_from` swaps a two-table
  inner join to reach a probe on the inner side's key, and a swap moves each table's columns to a
  different place in the joined row — which is exactly what an `Expr::Outer` is a position into,
  resolved against the written order before the swap decision exists. Swapping after that would
  read the wrong column, silently.
* **A correlated `LIMIT` or `OFFSET` is `42P10`**, with the clause's own name in the message.
  Measured, and it has to be: a limit that changed per row would not be a limit.

`LATERAL` — a `FROM` item that sees the ones *beside* it — is `0A000` naming itself. That is a
different thing from a `FROM` item seeing the ones *outside its statement*, which is ordinary
correlation and which this phase does: `FROM (SELECT … WHERE x = a.id) AS t, a` is `42P01` on both
servers, and a derived table inside a correlated subquery naming the outermost table runs on both.
Collapsing the two is a real bug and was one for half a day.

## What the captures decided that reading would not have

Five, each of which a plausible implementation gets wrong and none of which produces an error when
it does:

1. **An empty subquery beats a NULL left-hand side.** `NULL IN (SELECT … no rows)` is **false**,
   where `NULL IN (1)` is NULL — so emptiness is decided *before* the three-valued rule. The
   short-circuit `IN (list)` correctly does (PostgreSQL's grammar has no empty list) answers NULL
   here and drops a row.
2. **The inner scope shadows the outer, silently.** `SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b
   WHERE b.a_id = id)` returns **no rows**: `b` has an `id` of its own, so the condition is
   `b.a_id = b.id` and the statement is not correlated at all. The same statement with `a.id`
   returns two. Four lines of the capture pin it from four directions.
3. **An unreferenced CTE is still analysed.** `WITH t AS (SELECT nope FROM a) SELECT 1` is `42703`,
   and inlining alone never looks at a CTE nobody references. Every `WITH` item is therefore
   carried on the statement and planned, referenced or not, and the plan thrown away.
4. **A column alias list may be shorter than the target list.** `AS t (a)` over two columns renames
   the first and leaves the second alone; only a longer one is an error. Refusing a short one reads
   like the obvious symmetry and would refuse a statement a real server runs.
5. **The nouns and the sentences differ where a reader would expect one.** A two-column subquery is
   `subquery must return only one column` as a scalar and `subquery has too many columns` under
   `IN`; a CTE's wrong-length alias list is `WITH query "t" has …` where a derived table's is
   `table "t" has …`; a duplicate CTE name is `WITH query name …` where two `FROM` entries are
   `table name …`. Same SQLSTATEs, six sentences, and a client that greps the text sees all six.

`IN` is `= ANY` and `NOT IN` is `<> ALL`, measured side by side rather than reasoned, so the
asymmetric NULL rule has one implementation rather than two that drift.

## Consequences

* **A subquery never routes to the columnar engine**, and the refusal is by construction rather
  than by remembering: `exec::fragment::push_filter` has one arm per expression this crate has and
  a subquery's is a refusal, so a new expression is a compile error there. A fragment's filter
  language has no subquery in it and its answer arrives whole in one message
  ([ADR 0040](0040-the-engine-a-query-runs-on.md) Decision 4), so there is nothing for a replica to
  do. `Planned::engine` stays `None` on every plan this phase builds.
* **The access path inside a correlated subquery is planned once**, from a filter whose outer
  operand is a hole, so `WHERE b.a_id = <outer>` is a scan with a filter rather than the point read
  the same predicate gets against a constant. `TODO(post-v1)`: re-plan it per outer row. A
  performance shortfall, named here rather than discovered.
* **`plan::Expr` gained two variants and `plan::Select` one field**, and every `match` over the enum
  in the crate had to grow an arm. Every one of them is a refusal or a pass-through, so the failure
  mode of missing one is a compile error rather than a wrong answer.
* **The `ActiveRecord` counter did not move.** Three of the thirty-six boot statements carry a
  subquery and none reaches it: each stops on a catalog function first and needs `array_agg` or
  `ARRAY(SELECT …)` besides. `tests/activerecord_subquery.rs` measures their *shape* with the array
  pieces written as ones this node has, so the day the catalog unit lands the subquery half is
  already known to work. A feature that moves no number is what a counter asserted **exactly** is
  for.
* **What this phase does not do, each `0A000` naming itself and each checked by a test:**
  `WITH RECURSIVE` (a second evaluation model, with its own termination and its own memory bound —
  and the capture records that a real server *runs* `WITH RECURSIVE t AS (SELECT 1)`, because the
  body does not recurse, which is why the refusal is on the keyword); `LATERAL`; window functions;
  a data-modifying `WITH` item; `UNION`/`INTERSECT`/`EXCEPT`; a subquery in `INSERT`, `UPDATE` or
  `DELETE`; a row-valued subquery; and `ARRAY(SELECT …)`, which is a subquery wearing an array's
  clothes and belongs to the array unit.
