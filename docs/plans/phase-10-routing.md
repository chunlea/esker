# Phase 10 — planner routing: the SQL node chooses an engine, and says which

[ADR 0022](../adr/0022-columnar-learner-replica.md) **milestone 4**, and the last one before MPP.
Milestones 1–3 built a columnar file format, a fragment protocol evaluated against it, and a
learner fed by the Raft log that answers fragments over a real socket. What none of them built is a
*caller*: `docs/plans/phase-8-learner.md` §close says it in one line —

> **Nothing on a real cluster can ask a fragment.** No `esker` subcommand sends one and the planner
> does not choose a columnar replica, so the differential's evidence is the in-process gate.

That gap closes here. When this phase is done, `SELECT count(*), sum(amount) FROM ledger WHERE day
> …` typed into `psql` against a real cluster is answered by the columnar learner, `EXPLAIN` says
so, and `SET esker.engine = 'row'` makes it not.

## 0. What is already true, checked rather than assumed

Each of these was read at `8432814` before a line was planned against it.

* **The fragment request is a format that exists**, versioned and fuzzed:
  `esker_columnar::fragment::{Fragment, Output, Aggregate, encode, decode}`,
  `FRAGMENT_FORMAT_VERSION = 1`. This phase *builds* fragments; it does not define their bytes.
* **The fragment response is a format that exists**: `esker_proto::fragment::result`,
  `RESULT_FORMAT_VERSION = 1`, `Body::{Rows, Groups}`, `Group`, `Partial`, and `ScanStats` beside
  it — carried since phase 8 precisely because *"`EXPLAIN` is the named consumer"*.
* **A store answers `Request::Fragment`** (`esker_store::server::serve_fragment`): it routes by
  header and epoch, refuses a peer that is not a `ColumnarLearner` with `NotColumnar`, catches up to
  `min_apply_index`, fetches the schema it cannot read (ADR 0037), and evaluates.
* **PD places columnar learners** and `GetRegion` answers with a `Region` whose `peers` carry
  `PeerRole::ColumnarLearner`. `ALTER TABLE t SET (columnar_replicas = 1)` writes the catalog
  record and `Pd::ReportColumnar` asserts the whole set.
* **`esker-client` cannot ask a fragment.** `Body` is `Raw | Txn`; `Router::call` targets
  `Route::target()`, which is *the leader* — the one peer a fragment must not be sent to.
* **`esker-sql` cannot build one.** It does not link `esker-columnar` at runtime; its `Cargo.toml`
  says so and says why. §2 reverses that with the argument.
* **`EXPLAIN ANALYZE` is refused** (`parse/lower.rs`, `refuse_if(*analyze, "EXPLAIN ANALYZE")`).
* **The planner has no engine choice at all**: `exec::query::plan` builds one tree and
  `Node::explain` prints it.

## 1. The rules that decide everything below

Four, and every design choice in this file is one of them applied.

**R1 — a routing decision must never change an answer.** Not "rarely", not "except under
concurrency". Every substitution this phase makes must produce *exactly* the row space, in exactly
the row semantics, that the row plan it replaced would have produced. Where that cannot be
guaranteed by construction, the substitution is not made. U5's differential is the check, and it
exists before `auto` routes anything.

**R2 — a refusal is a normal answer.** `NotColumnar`, `TooFarBehind` and `Unsupported` mean *read
the rows*, and the fallback happens **inside the same transaction, at the same `start_ts`**, so the
answer is the one the snapshot always had. Silent to the client; visible in `EXPLAIN`.

**R3 — refuse, never partially honour** (ADR 0022 Decision 3). A fragment this planner cannot
express *in full* is not sent in part. An aggregate pushed down without the filter beneath it is a
wrong answer, and this is the one place in the whole feature where that mistake is cheap to make.

**R4 — the override moves the threshold, never the correctness rules.** `SET esker.engine =
'columnar'` skips the cost estimate. It cannot make a plan columnar that a fragment cannot express,
cannot read a transaction's own uncommitted writes, and cannot survive a refusal. ADR 0022 Decision
2 lists the override fourth and calls it "overrides all of it"; read literally that would let a
session ask for a wrong answer, so what it overrides is rule 3 — the estimate — and nothing else.
Recorded in this phase's ADR because a future reader will otherwise read the ADR literally.

## 2. Scope, and the two shared things this phase changes

### The lane is a worktree; new files where possible

New: `esker-client/src/fragment.rs`, `esker-sql/src/plan/routing.rs`,
`esker-sql/src/exec/fragment.rs`, `esker-sql/src/exec/explain.rs`, `esker-sql/src/fragment.rs`
(the seam), `esker-sql/tests/routing.rs`, `esker-sql/tests/routing_differential.rs`,
`esker-sql/tests/corpus/pg19_routing_engine.txt`, `esker-cli/tests/columnar_cluster.rs`.

Also new, and not in the original sketch: `PdConn` implements `RegionResolver`
(`esker-sql/src/pd.rs`). A SQL node routed from a static one-region table until this milestone,
which a fragment cannot use — the peer list is what carries a columnar learner, and a learner joins
through a conf change *after* any table a test or a binary wrote down. `GetRegion` is the only
routing question PD answers and now a SQL node asks it.

Edited, as little as possible: `esker-client/src/{lib,router}.rs` (three accessors and a re-export),
`esker-sql/src/{lib,parameter}.rs`, `esker-sql/src/plan/{mod,query,session}.rs`,
`esker-sql/src/exec/mod.rs`, `esker-sql/src/parse/lower.rs` (the GUC and `EXPLAIN ANALYZE`),
`esker-sql/src/error.rs`, `esker-store/src/server.rs` (§ReadIndex below), `docs/DESIGN.md` §16.

### Change 1: `esker-sql` links `esker-columnar` at runtime

Its `Cargo.toml` currently says the opposite, with a reason:

> Dev-only, and `esker-sql` deliberately does not link it at runtime: the planner's columnar half
> is a later milestone, and a leaf crate whose format versions could reach the SQL layer is what
> `esker-proto`'s result format exists to prevent.

The first half of that reason expires here — this *is* the later milestone. The second half is
about `esker-proto`, and stays true: the *response* still crosses the wire in `esker-proto`'s
format, decoded by `esker-proto`, and no columnar format version reaches the wire. What `esker-sql`
gains is the ability to **encode a fragment request**, and there is exactly one definition of those
bytes (`docs/plans/phase-8-learner.md` §wire: *"One definition of fragment bytes, and it is
`esker-columnar`'s"*). The alternatives are a second encoder in `esker-proto` — forbidden by that
same section — or a hand-rolled copy in `esker-sql`, which is the same thing with worse odds.

It is a workspace crate, so the dependency budget does not move (`crates/esker-cli/tests/
dep_budget.rs` counts **external** crates); `esker-columnar`'s own externals are `lz4_flex`,
`thiserror` and `tracing`, all already in this graph. The budget test is run to prove it.

### Change 2: the `ReadIndex` round runs for **every** fragment

ADR 0022 Decision 4 is unconditional: *"The learner asks the leader for the current commit index,
waits until its own apply has reached it, and only then evaluates."* `serve_fragment` today guards
that round with `if request.min_apply_index > 0`, so a caller that passes `0` gets no round at all
— and a SQL node has no way to compute a Raft index. It holds a `start_ts` from the TSO and a
region id; the leader's commit index is not a number it can learn.

It does not need to. The round *is* the mechanism: any transaction the client has been told
committed was committed on the leader **before** this statement's `start_ts` was allocated, so the
leader's commit index at the moment the fragment arrives is at or past that entry. Waiting for it
covers every commit visible at `ts`, which is the whole of Decision 4's second half.

So the guard is removed: the round always runs for a columnar learner, and `min_apply_index`
becomes an *additional* floor for a caller that has one (the joint gate does, and keeps passing
it). One round trip per fragment, amortised over a scan of millions of rows — ADR 0022 prices it
and calls the trade "not close". **No wire change, no golden change**: the field's type, encoding
and meaning are untouched.

The direction of the change is the safe one: a learner that cannot reach its leader now refuses
`TooFarBehind` where it used to answer from whatever it had.

## 3. Public API sketch

### `esker-client`: `fragment.rs`

```rust
/// A columnar replica's answer, with a refusal as a value rather than an error.
pub enum FragmentAnswer {
    Answered { result: Bytes, stats: ScanStats },
    Refused  { reason: RefusalReason, detail: String },
}

/// One region of a table, and whether anything in it can answer a fragment.
pub struct Shard { pub region_id: u64, pub start: Bytes, pub end: Bytes, pub columnar: Option<Peer> }

pub struct FragmentClient { /* an Arc<Router> */ }

impl FragmentClient {
    pub fn new(router: Arc<Router>) -> Self;
    /// Every region covering `[start, end)`, in key order, each with its columnar learner if it
    /// has one. Walks the resolver the way `esker-cli region ls` does: `GetRegion` is the only
    /// routing question PD answers.
    pub fn shards(&self, start: &[u8], end: &[u8]) -> Result<Vec<Shard>>;
    /// One fragment, to that shard's columnar learner.
    pub fn evaluate(&self, shard: &Shard, request: FragmentReq) -> Result<FragmentAnswer>;
}
```

Three things this API is shaped by:

* **It targets a learner, not a leader**, so it cannot reuse `Router::call`. It reuses everything
  else — the cache, the resolver, the clock, the in-flight gate, `retry::classify` — through three
  new accessors on `Router` rather than a second copy of the retry rules.
* **A refusal is not an `Err`.** `Result<FragmentAnswer>` has both: a transport failure is an
  error, a refusal is an answer.
* **A scan at a fixed `ts` is idempotent**, so a deadline or a leadership change *is* retryable
  here where it is not for a mutation — `may_ask_again` already draws that line by method, and
  `FragmentEvaluate` is a read. `Unsupported` and `NotColumnar` are never retried against the same
  peer: another replica running this build refuses identically.

### `esker-sql`: the seam, the rule, the plan node

```rust
// src/fragment.rs — the seam, shaped like `pd::ColumnarReport` and `backend::SchemaLease`.
pub trait FragmentSource: Debug + Send + Sync {
    fn shards(&self, start: &[u8], end: &[u8]) -> Result<Vec<Shard>>;
    fn evaluate(&self, shard: &Shard, fragment: &[u8], ts: u64, min_apply_index: u64)
        -> Result<FragmentAnswer>;
}

// src/plan/routing.rs — the decision, and only the decision.
pub enum Engine { Row, Columnar }
pub enum Setting { Row, Columnar, Auto }          // `esker.engine`
pub enum Reason {
    Override(Setting), Ratio { projected: usize, stored: usize },
    PointRead, NoColumnarCopy, WroteInThisTransaction, NotExpressible(&'static str), NoFragments,
}
pub struct Decision { pub engine: Engine, pub reason: Reason }
pub fn decide(shape: &Shape, setting: Setting, replicas: u8) -> Decision;

/// The columnar half of a plan: what one fragment asks for, and the row plan it falls back to.
pub struct Columnar {
    pub table_id: u64,
    pub fragment: esker_columnar::Fragment,   // built once, sent to every shard
    pub decision: Decision,
    pub row_space: RowSpace,                  // how a fragment answer becomes the rows above it
    pub fallback: Box<Node>,
}
```

`Node` gains **one** variant, `Node::Columnar(Box<Columnar>)`. It carries its own fallback, which
is what makes R2 structural: the node that fails is the node that knows what to run instead.

### One substitution, and how R1 is guaranteed by construction

**Exactly one sub-plan shape is replaced**: `Aggregate { input: [Filter] { SeqScan } }`, by a
`Node::Columnar` whose output row is the grouping keys followed by the aggregate values. That is
*precisely* what `Node::Aggregate` produces and precisely what every expression above it was
rewritten against, so the substitution is exact by construction rather than by care, and `Project`,
`Sort`, `Limit` and `Distinct` above it are untouched. The filter sits *below* the aggregate, so it
**must** be expressible or the whole substitution is declined (R3) — an aggregate pushed down
without its filter aggregates the wrong rows.

`HAVING` comes out of the aggregate and becomes a `Filter` above the substitution, **on both
paths**. The two are the same operator over the same row, and moving it on both paths is what stops
it being applied twice when the fallback runs.

**A bare projection scan is deliberately not routed**, and the reason is memory rather than taste:
a fragment returns its whole answer in one message, so a rows-output fragment is a whole region
materialised on the SQL node and framed as one response, where the row path streams a page at a
time. An aggregate's answer is one row per group, which is what makes it the shape that fits — and
it is the shape ADR 0022 exists for. It is in §6 with the rest of what this phase does not do.

## 4. The units

Each is one commit, test first, reported in the brief's format.

### U0 — this file.

### U1 — the client can ask a fragment

`esker-client/src/fragment.rs`, plus `Router::{transport, route_for, repair_for}` and the
`serve_fragment` change of §2.

Tests: `esker-client/tests/fragment.rs` against `FakeTransport` —
a fragment goes to the learner and not to the leader; `shards` walks a two-region table and returns
both; a region with no columnar peer comes back with `columnar: None` and is never asked; a
`Refused` is an `Ok`; a deadline is retried and a `NotColumnar` is not; `EpochNotMatch` repairs the
cache and retries. Then `esker-store/tests/fragment_readindex.rs`: a learner asked with
`min_apply_index = 0` still catches up, and refuses `TooFarBehind` when the leader is unreachable.

### U2 — the routing rule and `SET esker.engine`

`plan/routing.rs` (pure, no I/O, so it is unit-testable against a shape rather than a cluster),
the GUC in `parameter.rs` + `parse/lower.rs`, and the substitution in `exec/fragment.rs`.

The rule, in ADR 0022 Decision 2's order:

1. `PointGet` / `IndexLookup` / a `SeqScan` narrowed to a bounded key range → **rows**, always.
2. Not expressible, or the transaction has written, or `columnar_replicas == 0`, or no
   `FragmentSource` → **rows**.
3. **The ratio**: `projected × 2 ≤ stored`. Bytes read, not rows — a columnar scan reads one chunk
   per projected column per stripe where a row scan reads every column of every row, so the
   comparison is `projected/stored` against a fraction and not a row count. Half is the threshold
   because below it the columnar side reads strictly less than the row side even before
   compression, and it is written as a ratio so that a table growing a column moves the decision on
   its own — which is ADR 0022's stated reason for a ratio over a number. `count(*)` projects zero
   columns and is therefore always columnar.
4. `esker.engine`: `'row'` forces rows; `'columnar'` skips step 3 and nothing else (R4); `'auto'`
   is the default and is the three steps above.

Tests: `tests/routing.rs` — the rule table, one case per branch, on shapes; the GUC's SET/SHOW/RESET
against `tests/corpus/pg19_routing_engine.txt` (captured, §6); a `'columnar'` override over a point
read still plans a point read and says why.

### U3 — `EXPLAIN` names the engine

`exec/explain.rs`. Every scan line carries its engine, and a columnar one carries how many
fragments and why:

```text
Aggregate
  Output: count(*), sum(amount)
  Columnar Scan on ledger  (3 fragments)
    Engine: columnar  (2 of 6 columns projected)
    Filter: day > 20260101
```

`EXPLAIN ANALYZE` un-refuses **for a `SELECT` only** and adds the `ScanStats` line the response has
carried since phase 8:

```text
    Fragments: 3 asked, 3 answered, 0 refused
    Stripes: 41 of 96 read   Chunks: 82   Rows: 1204331 scanned, 88 matched
```

A fallback is visible, always:

```text
  Seq Scan on ledger
    Engine: rows  (columnar refused: too far behind)
```

PostgreSQL has no equivalent plan text, so this is a **declared divergence** under ADR 0031: the
corpus records what PG19 prints for the same statements, and the divergence list carries both
sides with the reason. It is not hidden and it is not claimed to be parity.

Tests: `tests/routing.rs` — golden plan text for each of {row, columnar, override, fallback}; the
`EXPLAIN ANALYZE` refusal still fires for everything that is not a `SELECT`.

### U4 — the finishing aggregate

`exec/fragment.rs`: `count`, `sum`, `min`, `max` merged across fragments **in region order**;
`avg` is `sum`/`count` finished here because a fragment has no `avg`; `GROUP BY` partials merged by
key with `pg_cmp` equality, which is the grouping rule everywhere else in this crate.

Region order is not decoration: `sum(double)` does not associate, and a deterministic fold order is
what makes the differential's comparison meaningful rather than flaky.

Tests: a proptest — **any** split of a row set into fragments folds to the same answer as one
fragment; the empty-input rules (`count` 0, everything else NULL; a grouped aggregate over no rows
is no rows); a NULL group key is one group.

### What U5 found, recorded here because it is a rule and not a bug

**A pushed-down comparison is type-checked, and a literal is mapped the row evaluator's way.**
`SELECT count(*) FROM t WHERE region = 'north'` was not routed at first: the planner leaves a
string compared against a `text` column as `Literal::String`, and the first version of the filter
push-down refused anything that was not already `Literal::Typed`. The fix is not to accept
everything — the far side compares with **a second implementation** of `pg_cmp`
(`esker_columnar::ValueRef::pg_cmp`), and two implementations agree about same-typed values by
construction and about mixed ones only by luck. So a literal is mapped exactly as
`crate::exec::cursor`'s row evaluator maps it (integer → `int8`, decimal → `float8`, string →
`text`), and then **refused unless it fits the column beside it**. A refusal costs a fallback; a
mixed-type comparison could cost an answer.

The differential is what surfaced it, and it surfaced it as *"this query was not answered by the
columns"* rather than as a wrong number — because the test asserts its own denominator.

### U5 — the differential

`tests/routing_differential.rs`. Every routed query is run **twice at the same `start_ts`** — once
routed, once with `esker.engine = 'row'` — and compared. Aggregates, `GROUP BY`, filters,
projections, and a concurrent writer so that `TooFarBehind` and the fallback are exercised rather
than assumed. A disagreement dumps both sides, the plan, the fragment and the shards, and fails.

This test exists **before** `auto` routes anything by default. ADR 0022 asks for it by name and
says why: *"the only defence is a differential test that runs every query both ways and compares,
which has to be built before the routing rule and not after."*

### U6 — the real cluster

`esker cluster start --pd` + `psql`: `ALTER TABLE t SET (columnar_replicas = 1)` → wait on an
**observable** (a fragment is answered), never a sleep → `EXPLAIN` prints `columnar` → `SELECT
count(*)` is served by the fragment. `#[ignore]`d in `esker-cli/tests/columnar_cluster.rs` with the
`--help` Gatekeeper warm-up `cluster_start` already uses, plus the in-process form in
`tests/routing_differential.rs` that runs in the gate.

### U7 — the ADR, and DESIGN.md

`docs/adr/00NN-the-engine-a-query-runs-on.md`: the session GUC and its measured PG19 surface, the
`EXPLAIN` shape as a declared divergence, R4's reading of "overrides all of it", and §2's two
changes. DESIGN.md §16's "Not built here" paragraph loses planner routing and `EXPLAIN`.

## 5. Risks

* **The finishing aggregate is where a wrong answer hides.** Two-level folding is the one place
  this phase can differ from the row engine *silently*: an `avg` finished from partials, a `min`
  over an empty partial, a `NULL` group merged with a real one. U4's proptest and U5's differential
  are aimed at exactly this and nothing else.
* **A refusal that is not handled is a hang or a panic, not a fallback.** Every call site that can
  receive `FragmentAnswer::Refused` is exhaustive by type, and the fallback is a field of the node
  rather than a branch somebody has to remember.
* **`EXPLAIN ANALYZE` runs the statement.** Un-refusing it for `SELECT` means `EXPLAIN ANALYZE
  SELECT` executes, which it must — but the refusal must stay for everything that writes, or
  `EXPLAIN ANALYZE INSERT` becomes an insert. It stays.
* **The differential can pass for the wrong reason.** A run where nothing routed columnar is a
  green test that proved nothing (phase 6b's lesson: *"a test that passed too easily hid an idle-CF
  WAL pin"*). Every differential asserts, per query, that the routed plan *was* columnar.
* **PD placement takes seconds.** Every wait is on an observable.
* **A region a store does not replicate has no `ReadIndex` round to run.** §2's change makes the
  round unconditional for a *running* peer; a store holding a region's record without a Raft peer
  for it has nothing to ask, and answers from whatever its copy holds. `TooFarBehind` is the wrong
  word for that state — it means *another replica may be closer* — and no reason on the wire means
  "not part of the group". A placed learner is never in that state for long, and what constructs it
  deliberately is a harness with no consensus in it (`esker-store/tests/schema_fetch.rs`). Closing
  it needs a fourth `RefusalReason`, which is a wire change and therefore an ADR of its own.
* **§2's `ReadIndex` change is pinned by U5, not by U1.** The assertion that would fail without it
  needs a real Raft group with a real columnar learner — the differential's harness — and a
  bespoke one in `esker-store` would be that harness written twice. U5 asks a fragment at
  `min_apply_index = 0` for a commit made microseconds earlier, which is exactly what the guarded
  version could miss.
* **A region that split after its columnar copy was built.** `esker-store`'s columnar slot is per
  region and nothing prunes it on a split, so a parent's copy may still hold rows the child now
  owns — which a fragment to each would count twice. The **epoch pins it**: shards carry the epoch
  the planner saw, a split bumps it, and the store refuses the stale one, so the query falls back
  to rows rather than double-counting. What that leaves is a table that stops being routable until
  the plan is rebuilt, which is a performance bound and not a wrong answer. A split-aware columnar
  copy is `esker-store`'s and is not in this milestone.
* **The rebase.** The type lane is editing `parse/lower.rs`, `plan/expr.rs` and `exec/query.rs` on
  `main`; this lane's hunks in those three files are kept to the smallest that will compile.

## 6. What this phase will NOT do

* **No MPP** (ADR 0022 milestone 5). No exchange, no shuffle, no spill. Two-level aggregation
  finishes on the SQL node, which is what the ADR says covers the motivating queries.
* **No cost model beyond the ratio.** No cardinality estimates, no statistics, no histograms, no
  join ordering by cost. Rule 3 is one comparison and stays one comparison.
* **No tiering** (ADR 0022's phase-6b synergy). The columnar copy this phase routes to is the
  learner and only the learner.
* **No columnar DDL beyond what exists.** `ALTER TABLE … SET (columnar_replicas = N)` is phase 8's
  and is untouched.
* **No join, `DISTINCT` or `ORDER BY` push-down.** A fragment's output is scan/filter/project/
  aggregate; everything else runs above it, on the SQL node, over the rows it returned.
* **No rows-output fragment**, so no bare `SELECT a, b FROM t` on columns — see the substitution
  above. It is a memory bound rather than a missing feature, and lifting it needs a streamed or
  paged fragment response, which is a wire change.
* **No `LIMIT` push-down.** A `Filter` between the limit and the scan makes it wrong, and there is
  no cardinality estimate to say when there is not one.
* **No routing of a statement that writes**, and none inside a transaction that has written — ADR
  0022 Decision 2 rule 2, and the one refusal that is about correctness rather than capability.
* **No second fragment format**, no change to `FragmentReq`, `FragmentResp` or the result format,
  and no new golden on any of them.
* **`esker-raft` is not touched.**

## 7. The measured PG19 surface for `esker.engine`

Captured against the `esker-pg19` container on `:55432`, every probe wrapped, and it settles four
things a plausible implementation gets wrong. The corpus is
`crates/esker-sql/tests/corpus/pg19_routing_engine.txt`; this is what it found.

* **`SHOW esker.engine` before any `SET` is `42704 unrecognized configuration parameter
  "esker.engine"`.** A namespaced GUC does not exist until it is set.
* **`SET` accepts anything.** `SET esker.engine = 'bogus'` succeeds; PostgreSQL validates no custom
  parameter's value, ever. This node **refuses** a value outside `row | columnar | auto`, which is a
  declared divergence in the same direction `crate::parameter` takes for every other setting:
  accepting a value this node will not act on is the one answer the contract forbids.
* **The value is read back exactly as written, case and all**: `SET ESKER.ENGINE = AUTO` then
  `SHOW esker.engine` answers `AUTO`, not `auto`. The *name* is folded; the *value* is not.
* **`RESET` does not restore "unrecognized" — it sets the empty string.** After `RESET
  esker.engine`, `SHOW esker.engine` answers `''` and does not error. So `42704` is reachable only
  before the first `SET` in a session, which is an asymmetry no reading of the documentation
  produces. `RESET esker.never_existed` is accepted, as ADR 0022's phase-8 amendment already found
  for storage parameters.
* `SET LOCAL` outside a transaction block is a **warning**, not an error, and sets nothing.

## 8. Definition of done

The acceptance list, in the order it is checked:

1. `SELECT count(*) FROM t` on a real cluster with a columnar learner is answered by a fragment,
   proved by `ScanStats` in `EXPLAIN ANALYZE` and by the learner's own counters.
2. `EXPLAIN` names the engine, the reason and the fragment count, for both engines and for a
   fallback.
3. `SET esker.engine = 'row' | 'columnar' | 'auto'` decides, `SHOW` reads it back, and every other
   value is refused by name.
4. `tests/routing_differential.rs` is green with a concurrent writer, and asserts per query that
   the routed plan was columnar.
5. `just check` green in the worktree; `m4-routing` rebased on `main` and fast-forwardable.
