# Phase 12 — subqueries and CTEs: a query whose FROM is a query

Status: **closed** — units 0–6 landed. The decisions are [ADR 0043](../adr/0043-a-subquery-is-a-plan-node-run-once-or-per-row.md); §6 records what each unit cost and what it changed about the plan.

`docs/plans/phase-9-rails.md` §5 left one sentence that this phase exists to delete:

> No window functions, no CTEs, no subqueries in FROM … a subquery is the next thing after this
> phase.

and §5's own measured table put a number on it: **4 of `ActiveRecord`'s 36 boot statements** need a
subquery of some shape, and every one of the four needs an *array* beside it. That second half is
another lane's and it is why §5 of this file says what this phase will not claim.

Design: `docs/DESIGN.md` §SQL. Constitution: `CLAUDE.md`. The compatibility contract inherited
whole from `docs/plans/phase-6a.md` §1 — **C1** every valid PG-19 statement parses, **C2**
parsed-but-unimplemented is `0A000` naming the construct, **C3** what we execute matches
PostgreSQL 19 exactly — and [ADR 0031](../adr/0031-rails-compatibility-is-measured.md)'s rule for
deciding between C2 and C3: *implement it when our behaviour reproduces PostgreSQL's for every
input that does not error; refuse it by name when it cannot.*

Lane: `crates/esker-sql/src/**`, `crates/esker-sql/tests/**` except `tests/joint_gate.rs` and
`tests/pd_wiring.rs`, this file, and the ADR unit 6 writes. Every capture is taken **before** the
code that answers it, on the `esker-pg19` container, inside one `BEGIN … ROLLBACK` so the shared
oracle is left as it was found.

## 1. The executor shape, which is one decision made twice

A subquery is a **plan node**, and the only question this phase has to answer about any of them is
*how many times it runs*. There are exactly two answers and the difference is whether anything
inside the subquery names a column of the row outside it.

**Uncorrelated — once, before the cursor opens.** Nothing in it depends on the outer row, so its
answer is a constant for the whole statement. It is run in a pass over the plan *before*
`Cursor::open`, and the values it produced are written into the node it came from. This is not a
new mechanism: it is exactly the shape `crate::exec::fragment::resolve` already uses to fill a
`Node::Columnar` from the fragments before the cursor sees it, and exactly what the executor
already does with `nextval` — run it once, substitute the value, and let the row evaluator see a
constant. An unresolved subquery reaching the row evaluator is a bug in this crate and says so,
the way an unresolved `Columnar` node does, rather than answering "no rows" — because "no rows"
from a scalar subquery is a **NULL**, which looks like an answer.

**Correlated — once per outer row.** Something in it names an outer column, so it is a different
query for every row and a nested loop is what it is. The outer references are marked in the
sub-plan when it is planned (`Expr::Outer`, a position in the *outer* row); before each run the
sub-plan is copied and every `Expr::Outer` in it is replaced by the value that row has there. So
the sub-plan a cursor is opened on never has an outer reference in it, and everything below the
substitution — the access path, the filter, the aggregate — is the machinery that was already
there.

That is the whole executor. Two entry points in one new file, `src/exec/subquery.rs`:

```rust
/// Runs every uncorrelated subquery in a plan and writes its answer into it. Before the cursor.
pub(super) fn resolve(node: &mut Node, txn: &dyn Txn, tenant: u64) -> Result<()>;
/// Runs one correlated subquery for one outer row. From the row evaluator.
pub(super) fn run(sub: &SubqueryExpr, outer: &[Datum], env: &Env<'_>) -> Result<Vec<Datum>>;
```

### Which shapes are which

| Shape | Runs | Answer |
|---|---|---|
| `x = (SELECT …)` scalar | once (uncorrelated) / per outer row | the one value; **no rows is NULL**, two rows is `21000` |
| `EXISTS (SELECT …)` | once / per outer row, **stopped at the first row** | a boolean; NULLs in the row are irrelevant |
| `x IN (SELECT …)` | once / per outer row | the three-valued rule `tests/corpus/pg19_in.txt` already pins |
| `x = ANY (SELECT …)`, `x > ALL (SELECT …)` | once / per outer row | three-valued, and `= ANY` is `IN` while `<> ALL` is `NOT IN` |
| `FROM (SELECT …) AS t` derived table | once, as the plan's own input | rows, under `t`'s names |
| `WITH a AS (…) SELECT … FROM a` | once **per reference** | rows; see §3 unit 3 for why per reference is not a divergence |

`EXISTS` stopping at the first row is not an optimisation loose enough to skip: it is what makes
`EXISTS` over a large table affordable per outer row, and it is observationally identical because
the only thing asked of the result is whether it is empty.

## 2. Memory, which is a bound and not a hope

Every one of these buffers rows, so every one of them is bounded the way `Node::Sort` and
`Node::Aggregate` already are (`crate::exec::cursor::SORT_LIMIT`, `53400` past it):

* a scalar subquery reads **at most two rows** — the second one is the error, so there is nothing
  to bound;
* `EXISTS` reads **at most one**;
* `IN`/`ANY`/`ALL` and a materialised derived table are bounded at `SORT_LIMIT` rows and answer
  `53400 configuration_limit_exceeded` past it, naming the shape;
* a correlated subquery is re-run per outer row and each run is bounded the same way; the *outer*
  side streams, so nothing accumulates across rows.

There is no `Vec` in this phase that grows with the input without a limit above it. `TODO(post-v1)`:
a hash semi-join would turn the per-outer-row `IN` into one pass; it is not in this phase because
it is a performance change and this phase is a correctness one.

## 3. Units, in order, each its own commit, each a capture first

### Unit 0 — this plan ✅

### Unit 1 — uncorrelated subqueries in expressions

Capture `tests/corpus/pg19_subquery_expr.txt` first. Scalar `(SELECT …)` in a target list, in a
`WHERE`, and on either side of a comparison; `EXISTS` / `NOT EXISTS`; `IN (SELECT …)` /
`NOT IN (SELECT …)`; `ANY` / `ALL` with a subquery and each of the six comparison operators. The
errors are part of the surface and are captured with the answers: more than one row from a scalar
subquery (`21000`), more than one column (`42601`), an aggregate over a subquery's value.

The three-valued rules are **not re-derived** — `pg19_in.txt` measured them for a list and the
capture checks that a subquery obeys the same ones, including the case that decides it:
`x NOT IN (SELECT … a NULL …)` matches nothing at all.

### Unit 2 — derived tables

`FROM (SELECT …) AS t`, `AS t(a, b)`, a join against one, an aggregate over one. Capture
`tests/corpus/pg19_subquery_from.txt`. Two things are measured rather than read: whether
PostgreSQL 19 still requires the alias (it was mandatory before 16), and what a column alias list
shorter or longer than the subquery's target list does.

A derived table becomes a **synthetic table definition** whose columns are the sub-select's output
columns, so the name resolution, the `SELECT *` expansion and the `EXPLAIN` naming that already
work for a real table work for this one unchanged — and the plan node under it is the sub-select's
own plan instead of an access path. Nothing in `Scope` changes shape.

### Unit 3 — non-recursive CTEs

`WITH a AS (…), b AS (…) SELECT …`, referenced once and many times, a CTE referencing an earlier
CTE, and a CTE name that shadows a real table. Capture `tests/corpus/pg19_cte.txt`.

**A CTE is inlined at each reference**, which makes it a derived table and gives unit 3 unit 2's
executor for free. PostgreSQL 12+ inlines a single-reference CTE and materialises a multi-reference
one; the difference between those is a *plan*, and what this project matches is observable
behaviour, not plans (ADR 0031). It is observationally identical here because a non-recursive CTE
over a read-only statement has nothing in it that can be run twice to different effect — this node
refuses every volatile function inside a subquery, which unit 3 asserts rather than assumes. The
cost is that a CTE referenced twice is *read* twice, and that is recorded as a performance
consequence in the ADR rather than hidden.

`WITH RECURSIVE` is `0A000` naming itself. `WITH … INSERT/UPDATE/DELETE … RETURNING` is `0A000`
naming itself unless a triage line asks for it.

### Unit 4 — correlated subqueries

`WHERE EXISTS (SELECT 1 FROM b WHERE b.a_id = a.id)`, a correlated scalar in a target list, a
correlated `IN`. Capture `tests/corpus/pg19_subquery_correlated.txt`, and the part of it that is
about *names* rather than about rows: an inner column **shadows** an outer one of the same name; an
unqualified name that two tables in the same level have is `42702`; a qualifier naming neither
level is `42P01`; and how deep the shadowing goes with three levels.

The access path inside a correlated subquery is planned **once**, from a filter whose outer operand
is a hole, so it is a scan with a filter rather than the point read the same predicate would get
against a constant. That is a performance shortfall and it is named here rather than discovered:
`TODO(post-v1)`, re-plan the access path per outer row.

### Unit 5 — the `ActiveRecord` shapes

Every statement in the harness's triage that names a subquery or a CTE, run with its expected rows;
the `tests/lowering.rs` gap entries that start working deleted, which is a commit; slt goldens.

### Unit 6 — the ADR, and DESIGN.md ✅

[ADR 0043](../adr/0043-a-subquery-is-a-plan-node-run-once-or-per-row.md): the executor shape, the
synthetic relation a derived table is, the inlining decision and its cost, the level an outer
reference carries, the five things the captures decided that reading would not have, and the list
of what is deferred. DESIGN.md §13's `esker-sql` paragraph carries the two-sentence version.

## 4. What this phase will NOT do

Named here so that a later reader can tell a decision from an omission. Each is `0A000` naming
itself, which is contract C2 and is checked by a test.

* **`WITH RECURSIVE`.** A second evaluation model — a working table iterated to a fixed point —
  with its own termination and its own memory bound. It is a phase, not a unit.
* **`LATERAL`.** A derived table that sees the row to its left. It is unit 4's correlation applied
  to unit 2's `FROM` item and it is deliberately not folded into either of them.
* **Window functions.** Not a subquery at all; §5 of phase 9 lists it separately and it stays there.
* **`UNION`/`INTERSECT`/`EXCEPT`**, inside a subquery or out of one. Already `0A000` by name and
  untouched here.
* **A subquery on the columnar engine.** `Planned.engine` ([ADR 0040](../adr/0040-the-engine-a-query-runs-on.md))
  is carried by every plan this phase builds, and a plan containing a subquery is never routed:
  a fragment's filter language has no subquery in it and its answer arrives in one message. The
  refusal is by construction — `push_filter` refuses the expression — and it is asserted, not
  assumed.
* **A subquery in `INSERT`, `UPDATE`, `DELETE`** — `INSERT … SELECT`, `UPDATE … WHERE id IN
  (SELECT …)`, `DELETE … WHERE EXISTS`. The read path is this phase; the write path needs the
  same expressions resolved against a statement that is already writing, and it is unit-shaped
  work for the phase after unless a triage line pulls it forward.
* **A subquery whose result is a row rather than a value** (`(a, b) = (SELECT x, y …)`), and
  `ARRAY(SELECT …)`, which is a subquery wearing an array's clothes and belongs to the type lane's
  array unit.

## 5. Risks

* **The shared enum.** `plan::Expr` gains two variants (`Subquery`, `Outer`) and every `match` over
  it in the crate — including `exec::fragment::push_filter`, which is a *different* lane's file —
  has to grow an arm. Every one of them is a refusal, so the failure mode of getting one wrong is a
  compile error rather than a wrong answer; the mitigation is that they are added in the same
  commit as the variant.
* **The type lane is editing the same crate.** `main` is merged into this branch at least every
  couple of hours and before every report, never rebased, and the crate gate is run against main's
  content after each merge.
* **The `ActiveRecord` shapes need arrays.** Unit 5 can only claim the statements whose *other*
  features exist. The ones that need `array_agg`, `ARRAY(SELECT …)` or `generate_subscripts` are
  blocked on the type lane and unit 5 says which, with the count, rather than reporting a number
  that hides them.

## 6. Progress

| Unit | State | What it cost |
|---|---|---|
| 0 — the plan | ✅ | `355cf84` |
| 1 — uncorrelated expressions | ✅ | `f5e738e`, `f85740b`; 124-statement capture |
| 2 — derived tables | ✅ | `468c1b6`; 65-statement capture, one new `Node` variant |
| 3 — CTEs | ✅ | `ee2fcc3`; 51-statement capture, no executor at all |
| 4 — correlated | ✅ | 34-statement capture; `Expr::Outer`, a scope chain, a nested loop |
| 5 — the `ActiveRecord` shapes | ✅ | 12-statement capture, an slt golden, **the counter moved by 0** |
| 6 — ADR and DESIGN.md | ✅ | [ADR 0043](../adr/0043-a-subquery-is-a-plan-node-run-once-or-per-row.md), DESIGN.md §13 |

### What each unit changed about the plan above

* **Unit 1** found the empty-subquery rule §1 does not mention and the capture header does:
  `NULL IN (SELECT … no rows)` is **false**, so emptiness is decided before the three-valued rule.
  And `LIMIT (SELECT …)` is the one subquery that cannot wait for the resolve pass — `Node::Limit`
  holds a `usize` — so it is folded where the planner has a transaction.
* **Unit 2** added `Node::Derived`, which computes nothing and exists for `EXPLAIN`: the plan text
  threads one table name down the whole tree, and without a node to change it at, a scan inside a
  derived table prints the wrong relation. It also needed `TableDef::row_id` to name the derived
  relation id, or `SELECT *` came back one column short.
* **Unit 3** needed no executor and one field: **an unreferenced CTE is still analysed**, which
  inlining alone never does.
* **Unit 5** moved `activerecord_surface.rs`'s counter by **nothing**, and the number is asserted
  exactly so that saying so is unavoidable. Three of the thirty-six boot statements carry a
  subquery and none reaches it: each stops on a catalog function first (`pg_get_indexdef`,
  `pg_get_constraintdef`, a `::text` cast) and needs `array_agg` or `ARRAY(SELECT …)` besides. What
  it did instead is measure their *shape* — a correlated scalar subquery whose own `FROM` is a
  derived table joined to a table — with the array pieces written as ones this node has, so the day
  the catalog unit lands the subquery half is already known to work. It also found a real bug doing
  it: a derived table inside a subquery was being planned with **no** enclosing scope, so it could
  not name the outer query's columns. A `FROM` item may not see the ones *beside* it (that is
  `LATERAL`) and may see the ones *outside* its statement, and those are two different things.
* **Unit 4** made an outer reference carry **how far out** it reaches, because a three-level
  `EXISTS` chain can name two rows outside itself; and it made a statement with a correlated
  subquery in it refuse to swap its join, because a swap moves the columns an `Expr::Outer` is a
  position into. A correlated `LIMIT` is `42P10`, measured.
