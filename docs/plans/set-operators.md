# `EXCEPT` and `INTERSECT` — the plan page debt #105 waits on

#105's own row says its size is **"unknown until the plan page is written"**, and the question it
names is the one below: how much of the `UNION` path serves the other two. This page answers that
from two captures and a complete read of the code, and it proposes a shape. **Nothing here has been
run**: no test has gone red or green against a build of this, so every "should" is a design and not
a measurement. The measurements are marked as such.

## What is already measured, on both sides

Two captures, taken eleven days apart for different reasons, and between them they cover both axes.

**`tests/captures/pg19_set_operations.txt`** (2026-09-05) covers the **typing** axis, all of it
under `UNION ALL`:

* `int + numeric → numeric`, `int4 + int8 → bigint`, an unknown literal takes the other arm's type,
  `NULL + int → integer`, `NULL + NULL → text`;
* a literal that cannot be read as the resolved type fails **as that type** — `SELECT 1 UNION ALL
  SELECT 'abc'` is `22P02`, not a mismatch — while a **column** of the wrong type is
  `42804 UNION types text and integer cannot be matched`, the names in the arms' order;
* a different column count is `42601 each UNION query must have the same number of columns`;
* **the output names come from the first arm only**, so a column can be called `i` and be a
  `numeric` because the other arm made it one.

> **That file's header is stale and should not be read as current.** It says "a set operation is
> `0A000` naming the operator (`parse::lower`)" — true when it was written, false now: `UNION` is
> implemented, and the refusal that remains lives in `exec::mod::set_arm_supported`, not in the
> parser. A recorded blocker is a dated hypothesis; this one decayed in eleven days.

**`esker-coord/s2-h105.out`** (2026-09-16, two passes, declared types, self-cleaning) covers the
**combination** axis, which nothing had measured:

| statement | answer |
|---|---|
| `a EXCEPT b` over `{1,2,2,3}` and `{2,3,3,4}` | `1` |
| `a EXCEPT ALL b` | `1 ; 2` |
| `a INTERSECT b` | `2 ; 3` |
| `a INTERSECT ALL b` | `2 ; 3` |
| `a UNION b` / `a UNION ALL b` | `1 2 3 4` / all eight rows |
| `int INTERSECT numeric` | `numeric` |
| `text EXCEPT <literal>` | `text` |
| `SELECT id … EXCEPT SELECT id, s …` | `42601 each EXCEPT query must have the same number of columns` |
| `int INTERSECT text` | `42804 INTERSECT types integer and text cannot be matched` |
| `a EXCEPT b INTERSECT b` | `1` |

Three of those are things reasoning gets wrong:

1. **`EXCEPT ALL` is multiset subtraction**, not "`EXCEPT` without the dedup": the left holds `2`
   twice and the right once, so one `2` survives. `INTERSECT ALL` is the same arithmetic from the
   other side — each value's count is the **minimum** of the two sides.
2. **`INTERSECT` binds tighter than `EXCEPT`**: `a EXCEPT b INTERSECT b` is `a EXCEPT (b INTERSECT
   b)`. On this data a left-to-right reading agrees by accident, which is exactly why precedence
   must come from the grammar rather than from an example.
3. **Both refusals name the operator.** This node's `SqlError::SetOperationTypes` hard-codes
   `UNION`, which is correct only while the other two never reach it.

## What this node already has — complete reads, not a sample

* **The plan can already say it.** `plan::SetOp` is `Union | Intersect | Except` with a `name()`
  for messages (`plan/query.rs:266`).
* **The parser already lowers all three.** `parse/set_operation.rs::operator` maps each one and
  refuses only `MINUS`; `keeps_duplicates(quantifier, op)` already exists.
* **The type unification is already shared and already operator-aware.** `Unifying::SetOperation`,
  `common_of`, `unify_user_type`, and the three `SetOperation*` error variants are reached by any
  operator; only the *word* in the sentence is fixed.
* **The refusal is one function, and it is deliberate.** `exec::mod::set_arm_supported` returns
  `Ok` for `Union` and `SqlError::unsupported(arm.op.name())` for the rest. Its own comment already
  names what is missing — "a materialised side and a multiplicity rule of their own (`INTERSECT
  ALL` is `min(count)` per row, `EXCEPT ALL` is the difference)" — and the capture above confirms
  both rules independently.
* **The row combination is what is absent.** `exec::query::combine` builds `Node::Append` and wraps
  a run of arms in `Node::Distinct` when the quantifier dedups. There is no node for intersection or
  difference, and `Append` cannot express either.

*(One guard that looks relevant and is not: `parse/lower.rs`'s `!matches!(op, SetOperator::Union)`
sits inside the **recursive CTE** shape check — `WITH RECURSIVE` must be `UNION [ALL]` on a real
server too. It is not a #105 chokepoint.)*

## The shape this suggests

Everything before the combination is reusable as it stands. What #105 has to add is a node and its
multiplicity rule:

1. **`Node::Intersect { left, right, all }` and `Node::Except { left, right, all }`**, each with a
   materialised right side — the operators are not streaming: a row of the left cannot be emitted
   until the right side is known in full.
2. **Multiplicity by count, not by membership.** Build a multiset of the right side keyed on the
   whole output row; then `INTERSECT ALL` emits `min(left_count, right_count)` of each value and
   `EXCEPT ALL` emits `left_count - right_count` where positive. The non-`ALL` forms are the same
   walk with both counts clamped to one, which is why they should share an implementation rather
   than being `Distinct` wrapped around the `ALL` form — wrapping would be wrong for `EXCEPT`,
   whose dedup happens **before** the subtraction.
3. **Precedence in the lowering**: `INTERSECT` binds tighter than `UNION` and `EXCEPT`, which are
   left-associative and equal. `parse/set_operation.rs` currently folds a flat list of arms; a flat
   list cannot express (2)'s grouping, so this is the part of the parser that does change.
4. **The messages stop hard-coding `UNION`** — `SetOperationTypes` and `SetOperationArity` take the
   operator's `name()`, which `plan::SetOp` already provides.

## Size, and the one thing that could move it

**Medium**, on the reading above: one plan node pair, one multiplicity walk, one precedence change
in the lowering, and a message that already has the word it needs. Nothing here touches storage or
the wire, so no ADR is implied.

**What could move it**: (3). If the arm list is flattened somewhere the grouping cannot be
recovered — `exec::query::combine` takes `Vec<(Option<(SetOp, bool)>, Planned)>`, a flat sequence —
then precedence is not a lowering change but a plan-shape change, and the arms of every existing
`UNION` query change shape with it. **That is the thing to measure first**, before any of the rest:
build the grouping question as a red test over `a EXCEPT b INTERSECT b` and see which layer has to
change to answer it.

## Step 1 is measured, and it settles the size (2026-09-16)

The page said to answer one question before building anything: `exec::query::combine` takes a
**flat** sequence of arms, so if the grouping `a EXCEPT b INTERSECT b` could not be recovered there,
precedence would stop being a lowering change and become a plan-shape change every `UNION` query
shares. Two measurements answered it, and both came back the way the page hoped.

**The precedence probe** (`tests/lowering.rs::a_mixed_set_operator_chain_keeps_its_grouping`,
committed in `7cb73410`): a same-precedence chain lowers **flat** — two top-level `Except` arms,
nothing nesting — and a mixed chain lowers **nested**, one `Except` arm whose own `set_arms` is
`[Intersect]`. The plan already expresses precedence.

**The refusal probe** (`scratchpad/h/h105_step1.py`, applied and restored, never committed): lifting
`exec::mod::set_arm_supported`'s refusal and changing nothing else makes the corpus fail with
**9 of 18 statements disagreeing, every one of them on rows** — and not one `0A000`:

```
line 34  EXCEPT        PostgreSQL 1          Esker 1 ; 2 ; 3 ; 4        <- UNION's rows
line 35  EXCEPT ALL    PostgreSQL 1 ; 2      Esker all eight
line 36  INTERSECT     PostgreSQL 2 ; 3      Esker 1 ; 2 ; 3 ; 4
line 40  pg_typeof(v)  PostgreSQL numeric    Esker numeric ; numeric ; numeric
```

So **the refusal was the only gate**: parsing, lowering, arm unification and type settlement all
work for the two new operators already — line 40 is the proof, where the *type* is right
(`numeric`, unified across an `integer` arm and a `numeric` one) and only the row count is wrong.
What is missing is exactly what this page said: the row combination.

**The probe must never be committed.** With the refusal lifted and nothing built, `INTERSECT` and
`EXCEPT` answer `UNION`'s rows — a wrong answer where an honest `0A000` used to be, which is the one
outcome worse than not having the feature. It exists to be applied, read and restored.

**One consequence for the build order below**: step 2 as written — land the variants with the
executor deliberately answering `Append`'s behaviour, so the failure moves from "refused" to "wrong
rows" — buys nothing now, because the probe has already shown the failure in that shape without a
line of code. The first slice is the combination itself.

## What exists already, for whoever builds it

* `crates/esker-sql/tests/corpus/pg19_set_operators.txt` — the capture above, replayable.
* `crates/esker-sql/tests/set_operators.rs` — its runner, `#[ignore]`d with #105 named, so the day
  the feature lands the attribute comes off and eighteen statements start being enforced at once.
* Two statements are already declared divergences in `tests/enum_unknown_literal.rs`, each carrying
  a `pg19_enum_unknown_literal.txt:<line>` pointer, so the ratchet says which half arrives first.
