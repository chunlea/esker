# 0073 — A fragment expression node is added by tag, not by a version bump

Status: **accepted**, and built with the join fragment —
[`docs/plans/phase-16-mpp.md`](../plans/phase-16-mpp.md) §J4.
`crates/esker-columnar/src/fragment/expr.rs` is the node,
`crates/esker-columnar/src/fragment/codec.rs` its tag, and
`crates/esker-columnar/tests/fragment_golden.rs` the goldens that pin both.

## Context

[ADR 0022](0022-columnar-learner-replica.md) Decision 3 requires that *"the fragment is a format
with a version byte and a golden"*, and `CLAUDE.md` requires an ADR for any format change. The join
fragment needs a membership test — a semi-join pushes the inner side's key set down as a predicate
on the outer table — and the filter grammar has `Column`, `Literal`, `Compare`, `And`, `Or`, `Not`
and `IsNull`, tags 1 to 7, and nothing that expresses `x ∈ {…}`.

It can be *written* today, as an `Or` chain of `Eq`, and that is not a workable answer: a few
thousand keys against a few hundred thousand rows is hundreds of millions of `pg_cmp` calls, which
loses to the row-engine join the push-down exists to replace. The node has to exist.

So the question this ADR answers is not *whether* — it is what adding one costs a cluster that is
half upgraded.

## Decision 1: a new node is a new tag, and the format version does not move

`FRAGMENT_FORMAT_VERSION` stays at 1. `Expr::In` is tag 8.

The reason is a property the format already has and already documents. `docs/DESIGN.md` §16.2:

> **A fragment this build cannot evaluate is refused, and none of it is done.** An unknown version,
> expression node, comparison operator, aggregate kind or type tag … the caller falls back to a row
> scan.

An older evaluator meeting tag 8 therefore refuses the *whole* fragment, and a refusal is answered
by the row plan the routed plan already carries, at the same snapshot, with the answer the client
would have had anyway. There is no version of this that returns a wrong answer, and no round trip
is wasted that a version bump would have saved: bumping the version would make the same node
refuse for a less specific reason.

The alternative — bump the version for every added node — spends the version byte on a thing
refusal already handles, and a version byte that moves on every additive change stops being able
to say anything about a change that is *not* additive. It is kept for the case that needs it: a
change to what an existing tag **means**, which refusal cannot detect and which is the one thing a
reader of two versions' bytes could get silently wrong.

`tests/fragment_golden.rs::an_expression_node_this_build_does_not_know_refuses_the_whole_fragment`
asserts it, and asserts it *with the checksum recomputed* — because the CRC is verified before the
tag is read, so a patched byte left un-checksummed tests corruption instead, which is the other
branch and the wrong one.

## Decision 2: the list is strictly ascending, and that is checked rather than imposed

`Expr::In`'s values are required to be strictly ascending in `Value::pg_cmp` order, and a decoder
that meets a list which is not **refuses** it. It does not sort what it was given.

Three things follow, and the third is the one that matters:

* the evaluator gets a **binary search** for nothing — twelve comparisons a row at the 4,096-value
  bound, against four thousand for the `Or` chain, which is the whole point of the node;
* the wire form is **canonical**: one predicate has one encoding, which is what a golden can pin
  and what makes two nodes' bytes comparable;
* a producer that has lost its ordering is **caught**, where a sorting decoder would quietly
  correct it. That is the same instinct as `esker_sql::plan`'s "reject, do not ignore" and ADR
  0022's refuse-never-partially-honour: a decoder that repairs its input hides the bug in whatever
  wrote it.

Duplicates fall out of *strictly* ascending and are refused for the same reason.

## Decision 3: no NULL in the list, and one type in it

`x IN (NULL)` is unknown for every `x`, and `x IN (1, NULL)` is unknown wherever it is not true.
Both are expressible in three-valued logic and neither is a shape the only producer of this node
wants — a join key set has no NULLs, because `a.x = b.k` never matches one. A NULL in the list is
**refused** at decode rather than evaluated, so no reader of this format has to reason about a case
no writer produces.

The values must also all be the **operand's** type, checked by `fragment::check` exactly as
`Compare` is checked, and refused with the same `operator does not exist: text = integer` shape a
real server gives. [ADR 0040](0040-the-engine-a-query-runs-on.md) Decision 5 is why: this evaluator
is a *second* implementation of this system's ordering, and two implementations agree about values
of one type by construction and about values of two only by luck.

## Consequences

* A cluster mid-upgrade routes a join to a new learner and refuses it on an old one, and both
  answer correctly — one from the columns, one from the rows. The planner already treats a refusal
  as "read the rows"; nothing new is needed for the mixed case.
* `MAX_IN_VALUES` is 4,096 and is a bound on what arrives from the wire, not a preference. An
  `int8` list that long encodes to well under a tenth of `max_frame_size` (16 MiB, DESIGN §9). What
  moves it is a measurement.
* The columnar differential (`tests/differential.rs`) gained an `In` arm that walks the list
  **linearly**, on purpose: two implementations that shared the binary search would agree about a
  mis-sorted list by sharing the same mistake, and the ordering rule of Decision 2 is exactly what
  that arm is there to check.
* **The next additive node has a precedent and needs no ADR of its own** — this page is the general
  answer, and only a change to an existing tag's meaning has to come back here.
