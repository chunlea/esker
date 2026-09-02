# 0040 — The engine a query runs on, and how a user sees it

Status: **accepted**, and built — [ADR 0022](0022-columnar-learner-replica.md) milestone 4.
`docs/plans/phase-10-routing.md` is the plan; `crates/esker-sql/src/plan/routing.rs` is the rule,
`exec/fragment.rs` the substitution, `exec/explain.rs` what a reader sees, and
`crates/esker-client/src/fragment.rs` the ask.

## Context

ADR 0022 decided that Esker gets a columnar copy of a table, that it is a Raft learner, that
push-down is a plan fragment, and — Decision 2 — that *"the planner chooses per query, by estimated
cost, with an override"* and that *"`EXPLAIN` must name the engine it chose."* Milestones 1 to 3
built the file format, the fragment protocol and the learner. Phase 8 closed with one line:

> **Nothing on a real cluster can ask a fragment.** No `esker` subcommand sends one and the planner
> does not choose a columnar replica.

This ADR records the decisions that closing that gap forced, in the order a reader will meet them.
Four of the six reverse or narrow something already written down, which is why they are here rather
than only in the plan.

## Decision 1: the override moves the threshold, never the correctness rules

ADR 0022 Decision 2 lists four rules and says of the fourth: *"A session variable overrides all of
it."* Read literally, `SET esker.engine = 'columnar'` would then be able to ask for an answer no
fragment can produce — a read of a transaction's own uncommitted writes, a point read a columnar
file cannot restrict to, an aggregate with a `DISTINCT` inside it.

**What the override overrides is rule 3, the estimate, and nothing else.** `'row'` forces rows,
which is always possible. `'columnar'` skips the ratio and skips nothing else: a shape no fragment
expresses is still planned on rows, and `EXPLAIN` still says which rule refused it.

The alternative — honouring the word "all" — is a setting that can produce a wrong answer, and a
session variable is exactly the wrong place to put one. A user reaching for this reaches for it
because the *estimate* is wrong; nobody reaches for it because they want a different answer.

## Decision 2: `esker.engine` is a namespaced GUC, and its surface was measured

`SET esker.engine = 'row' | 'columnar' | 'auto'`, `SHOW esker.engine`, `RESET esker.engine`. The
shape is [ADR 0021](0021-time-machine.md)'s: a namespaced custom parameter, which a real PostgreSQL
accepts and stores, so no grammar of our own is needed and every statement a user writes here is
one a real server takes.

The capture is `crates/esker-sql/tests/corpus/pg19_routing_engine.txt` and it settled four things,
each of which a plausible implementation gets wrong:

* **`SHOW` of a namespaced GUC is `42704` until the first `SET` of the session** — and after a
  `RESET` it is *not* `42704` again, it is the **empty string**. The "unrecognized" answer is
  reachable exactly once per session. No reading of the documentation produces that asymmetry.
* **PostgreSQL validates no custom parameter's value, ever.** `SET esker.engine = 'sideways'`
  succeeds there and `SHOW` hands `sideways` back.
* **the value keeps its case and the name does not.** `SET ESKER.ENGINE = 'AUTO'` sets
  `esker.engine`, and `SHOW` answers `AUTO`.
* an un-namespaced unknown name is `42704` on `SET`; any namespace at all is accepted.

**This node refuses a value outside the three**, with `22023` and the three spellings as a `HINT` —
PostgreSQL's own shape for an enum it *does* know. That is a declared divergence, and it is the
direction `crates/esker-sql/src/parameter.rs` takes for every row in its table: accepting a value
this node will not act on is the one answer the contract forbids, and here the parameter decides
which engine a query runs on. Six divergences are listed with their reasons and checked in both
directions.

## Decision 3: `EXPLAIN` names the engine, and two silences are deliberate

Every scan carries an `Engine:` line with the engine and the reason; a columnar plan is its own
node with its fragment count, its grouping keys and its aggregate calls. `EXPLAIN ANALYZE` runs the
statement and adds the `ScanStats` the fragment response has carried since phase 8 — carried then
rather than added now precisely because *"`EXPLAIN` is the named consumer"*.

```text
Aggregate on ledger
  Engine: rows  (columnar refused: too far behind)
  Fragments: 3 asked, 0 answered
  Seq Scan on ledger
```

**Two reasons print nothing at all**: a table nobody asked for a columnar copy of, and a node with
no way to ask a fragment. Both mean *there was no choice*, and a line about the engine on those
plans is a line on every plan of every ordinary table — which is how a line stops being read when
it matters. Every other reason describes a decision that was made.

PostgreSQL's plan text has no equivalent to any of this, because PostgreSQL has no second engine to
choose between. Under [ADR 0031](0031-rails-compatibility-is-measured.md) that makes it a
**declared** divergence rather than a parity gap: `pg19_routing_explain.txt` carries what PG19
prints for the same statements verbatim, names the four things that differ (costs,
`Aggregate` vs `HashAggregate`, the engine line, and what `ANALYZE` reports), and pins the half
that *is* comparable — which spellings each server accepts.

`EXPLAIN ANALYZE` is executed for a `SELECT` and stays `0A000` for everything that writes. An
`EXPLAIN` that inserts a row is a surprise a user cannot undo.

## Decision 4: exactly one plan shape is substituted, and a rows-output fragment is not

The only sub-plan replaced is `Aggregate { input: [Filter] { SeqScan } }`, by a node whose output
row is the grouping keys followed by the aggregate values — which is *precisely* what
`Node::Aggregate` produces and precisely what every expression above it was rewritten against. So
"a routing decision never changes an answer" is a property of the substitution rather than of care
taken around it, and `Project`, `Sort`, `Limit` and `Distinct` above are untouched.

`HAVING` moves out of the aggregate onto a `Filter` above the node, **on both paths**, so it is
applied exactly once whichever engine ran.

**A bare projection scan is not routed**, and the reason is memory rather than taste: a fragment
returns its whole answer in one framed message, so a rows-output fragment is a whole region
materialised on the SQL node where the row path streams a page at a time. An aggregate's answer is
one row per group, which is what makes it the shape that fits — and it is the shape ADR 0022 exists
for. Lifting this needs a paged or streamed fragment response, which is a wire change.

## Decision 5: a pushed-down comparison is type-checked, and a literal is the row evaluator's

A fragment's filter is evaluated by `esker_columnar::ValueRef::pg_cmp` — a **second
implementation** of this system's ordering. Two implementations agree about values of the same type
by construction and about values of different types only by luck.

So a literal is mapped exactly as `crate::exec::cursor`'s row evaluator maps it (a bare integer is
`int8`, a decimal is `float8`, a quoted string is `text`, anything the planner already resolved
arrives typed), and is then **refused unless it fits the column beside it**. A refusal costs a
fallback to rows; a mixed-type comparison could cost an answer.

The differential found this, and found it as *"this query was not answered by the columns"* rather
than as a wrong number — because the test asserts its own denominator.

## Decision 6: two things already written down are reversed, with their arguments

### `esker-sql` links `esker-columnar` at runtime

Its `Cargo.toml` said the opposite: *"the planner's columnar half is a later milestone, and a leaf
crate whose format versions could reach the SQL layer is what `esker-proto`'s result format exists
to prevent."* The first half expires at this milestone. The second is about `esker-proto`, which
still does not link `esker-columnar` and still carries the response in its own format.

What `esker-sql` gains is the ability to **encode a fragment request**, and there is one definition
of those bytes (`docs/plans/phase-8-learner.md` §wire). The alternatives are a second encoder in
`esker-proto` — forbidden by that same section — or a hand-rolled copy, which is the same thing
with worse odds. A workspace crate, so the dependency budget does not move.

### The `ReadIndex` round runs for every fragment

ADR 0022 Decision 4 states it unconditionally: *"The learner asks the leader for the current commit
index, waits until its own apply has reached it, and only then evaluates."* `serve_fragment`
guarded it with `min_apply_index > 0`, which made the **default value the unsafe one** — and a SQL
node is exactly the caller that passes zero, because it holds a snapshot `ts` and a region id and
the leader's commit index is not a number it can compute.

It does not need to. The round *is* the mechanism: a transaction the client has been told committed
was committed on the leader before this statement's `ts` was allocated, so the leader's commit index
when the fragment arrives is at or past that entry, and waiting for it covers every commit visible
at `ts`. `min_apply_index` stays as an additional floor for a caller that has one.

**No wire change and no golden change.** One round trip per fragment, amortised over a scan of
millions of rows — ADR 0022 prices it and calls the trade "not close".

## Consequences

* A query over a table with `columnar_replicas > 0` whose projection is at most half the table's
  width, and whose shape a fragment expresses, runs on the learner. Everything else runs on rows,
  and `EXPLAIN` says which and why.
* **An epoch change is not retried on the fragment path.** A fragment covers the whole of a
  region's columnar copy — the evaluator refuses a key range, because a columnar file records none
  — so re-routing after a split asks about a different set of rows and returns a partial answer
  that looks complete. It surfaces, and the caller reads rows in the same snapshot.
* **The region cache is confirmed once when it lists no learner.** The cache is repaired by the
  refusals it causes and a missing learner causes none, so a client would otherwise plan on rows
  for ever. Paid only by a table whose catalog record asks for a copy.
* **A region that split after its columnar copy was built is not routed.** Nothing prunes a
  parent's copy on a split, so a fragment to each half could count a row twice — the epoch pins it:
  shards carry the epoch the planner saw, a split bumps it, and the store refuses the stale one. A
  performance bound, not a wrong answer, and a split-aware columnar copy is `esker-store`'s.
* **A region a store holds the record for without replicating** has no peer to run a round with and
  answers from what it has. `TooFarBehind` would be the wrong word — it means *another replica may
  be closer* — and no reason on the wire means "not part of the group". Closing it needs a fourth
  `RefusalReason`, which is a wire change and an ADR of its own.
* MPP is still ADR 0022 milestone 5 and still last. Two-level aggregation finishes on the SQL node,
  which is what that ADR says covers the queries the feature exists for.
