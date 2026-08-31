# Phase 8 — the columnar learner

ADR 0022 Decision 1: *a columnar replica is a Raft **learner** whose apply writes columns instead
of rows.* This plan is written in two halves against that sentence. **§store** (lane cl-c1) is the
apply target, the run lifecycle, and the fragment service. **§wire** (lane wy-c2) is the fragment
request and response, and the placement plumbing that makes a store hold a region columnar.

The seam between them is one-directional by construction: §wire defines message types, §store
consumes them. No wire bytes are defined here.

---

# §store

## What is already true, checked rather than assumed

`esker-raft` grows **nothing**, which ADR 0022 asserts and this lane verified at the mechanism
before planning around it:

* **Learners exist.** `Config::learners`, `conf.is_learner`, and the leader's `Progress` carrying
  the flag. Wave C's `rebuild_progress` work is on the same field.
* **A learner can run a `ReadIndex` round today.** `Raft::read_index` on a non-leader forwards to
  the leader (`readonly.rs`); the leader's `answer_read` replies to *any* forwarder with no voter
  check. The only `is_voter` gate is in `record_read_ack`, which counts heartbeat acks toward a
  quorum and correctly excludes learners. Decision 4's second half therefore needs no raft change.

The one gap is **store-side and in this lane**: `PeerCore::read_index` short-circuits on
`role != Leader` and answers `NotLeader` before raft ever sees the request. That short-circuit is
right for a row read — only the leader serves those — and wrong for a learner serving a fragment.
Unit 3 splits the two rather than loosening the row path.

Dependency directions, which constrain the design more than anything else here:

```
esker-sql  ->  esker-store  ->  esker-engine
esker-columnar ->  esker-engine          (standalone; nothing depends on it yet)
```

This lane adds the first edge into the columnar crate: `esker-store -> esker-columnar`.

## OPEN — two questions this plan does not answer, and will not guess

Both are recorded here rather than decided, because getting either wrong is a rewrite of unit 1.

### OPEN-1: who decodes a row into typed columns — ESCALATED, and not blocking

The apply target must turn a committed row into `esker_columnar::Value`s. `esker-keys` gives the
store `split_table` and `split_ts` honestly — tenant, table, and the MVCC suffix. It does **not**
give the row *value*: that is `esker_sql::row::decode_row`, and `esker-sql` depends on
`esker-store`, so the store calling it inverts the layering.

wy-c2 decomposed this better than the first draft of this plan did. The decoder needs two things
and they have **different answers**:

* **the types, in order** — those can travel as **data**. The SQL node already has to publish a
  per-table record for Decision 5's flag, on ADR 0021's precedent: a record under its own kind
  byte, readable by a layer that does not link `esker-sql`. A schema record beside it is the same
  move.
* **the codec** — how to walk a row's bytes given those types. That is **code**, and cannot travel
  as data. Something below `esker-sql` has to own it.

So the real question is not which port shape to use; it is **whether the row value codec moves
down out of `esker-sql`**. That is an architecture decision crossing both lanes, it needs an ADR,
and it is on the coordinator's desk rather than settled between two lanes mid-flight.

Two facts that bear on it, both verified from the manifests rather than assumed:

* **`esker-cli` does not link `esker-sql`.** In fact *no crate in this tree does*, and `esker-sql`
  has no binary — it is a leaf library nothing consumes yet. So "the composition root injects an
  implementation from above" has no *above*: there is no process today holding both a `Store` and
  the SQL layer. Injection is not wrong in shape, it is unbuildable without giving every storage
  node the whole SQL layer as a dependency.
* **Placement will not carry schema**, and should not. PD carries *where* a replica lives, not
  *what* a table looks like; a placement operator naming a schema would make PD a carrier of SQL
  semantics, which is the line phase 6e drew when it moved the schema-step drive away from PD on
  invariant 7. §wire's plumbing carries table ids and replica counts — numbers.

**Why this does not block unit 1.** The port has the *same shape* whichever way the decision goes:

```rust
trait RowDecoder { fn decode(&self, key: &[u8], value: &[u8]) -> Result<Vec<Value>>; }
```

Only *who constructs it* changes. Unit 1 takes one by injection and its tests use a hand-written
fake, so the apply target, the run lifecycle and the differential harness are all writable now.
This lane is blocked on the **constructor**, not on the trait — so it builds against the trait and
the constructor question stays open in this file until it is ruled on.

### OPEN-2: where MVCC visibility is applied

Decision 4 evaluates visibility **at read time**, so runs hold every version as committed. Applying
it means "the newest version of each key with `commit_ts <= ts`, unless that version is a delete".

M2's evaluator cannot express that. `ScanOptions` carries one switch (`prune`); `evaluate_with`
takes a `Fragment` and evaluates every row in the file. `commit_ts <= ts` *is* expressible as a
filter `Expr` — but **"newest per key" is an argmax**, and no `Expr` in `fragment` does that.

| | |
|---|---|
| **(i)** express it in the fragment filter | impossible: argmax is not an `Expr` |
| **(ii)** resolve versions at compaction, keep one per key | breaks reading at an older `ts`, which Decision 4 requires |
| **(iii)** resolve in the store: read candidate rows out, fold there | the store re-implements aggregation, and materialises what pruning exists to avoid |
| **(iv)** a version-aware scan mode in `esker-columnar` | where the layout knowledge lives; one forward pass if the run is sorted for it |

**Proposed: (iv)**, with the ingestion side paying for it: if each run is sorted by
`(pk, commit_ts DESC)`, then the newest visible version of a key is the *first* row for that key
with `commit_ts <= ts`, and the scan is a single forward pass with no hashing and no buffering.
The sort is a compaction decision, which is squarely this lane; the scan mode is a small addition
to M2's evaluator, which the brief scopes *away* from this lane ("ingestion+compaction side").

**This needs a scope ruling before unit 3.** Either the evaluator addition is in this lane, or it
belongs to whoever owns M2's scan. It is a handful of lines against `evaluate_with`, but it is not
mine to assume.

## The apply target (unit 1)

A region a store holds as a columnar learner routes its committed entries to a second apply target
rather than `apply.rs`'s row writer. Everything else about the peer is unchanged — this is the
whole of Decision 1, and the reason it is a new *apply target* and not a new replication system.

* **Batched appends.** The apply loop is the hot path. Entries are decoded and appended through
  `Writer::append_row`, accumulating in the open stripe; nothing is written per row beyond what the
  writer already buffers. No vectorisation beyond what the bench in unit 5 justifies (house rule).
* **Sealed by size or age**, then `finish()` into an immutable file under the existing rename
  discipline (invariant 3): write, fsync, atomic rename, fsync the directory.
* **MVCC as committed.** `commit_ts` is a column. Deletes are a delete-mark column rather than an
  absence, because a delete is a version like any other and OPEN-2's pass has to see it.
* **Snapshot install for a brand-new learner goes through the row path first**, then converts. It
  is the honest simplest: the snapshot protocol, its refusals and its crash points are wave-C-tested
  as they stand, and a second streaming format would double that surface for a case that happens
  once per learner. The conversion is a bulk `append_row` walk of the ingested range, sealed as one
  run. Written down here because "reuse then convert" is a choice and the alternative — teaching
  the snapshot stream to carry columns — is the one a reader will ask about.

## Run lifecycle and compaction (unit 2)

Appending an apply stream column-wise produces small runs "exactly as a memtable flush does". So:

* a background merge to a bounded run count, on the same shape as the row engine's compaction;
* **crash-safe**: `kill -9` mid-merge loses nothing acknowledged, because the merge writes a new
  file and swaps a pointer — inputs are never mutated;
* the **two-instant race** the row engine learned in phase 2 applies unchanged: a file is obsolete
  only if no live version names it *and* it is not a pending output. Wave C's `retain` discipline
  in the SST tier is the same lesson and the sweep here is written against it, not rediscovered.

## The fragment service (unit 3)

Serves §wire's `FragmentReq` against the region's runs.

Three things about the request are **settled** by §wire and are not this lane's to reopen:

* **The epoch is in `RequestHeader`**, which carries `{ region_id, epoch, peer }` on every request
  already (`esker-proto/src/messages.rs`). A fragment gets invariant 5 from the field that is there
  rather than a second one in the fragment body — two epochs on one wire is two sources of truth.
  This lane checks it exactly as a row read does, and invents no bytes.
* **`ts` and `min_apply_index` are separate fields**, never one inferred from the other. They are
  Decision 4's two halves and they fail differently: `ts` is MVCC visibility applied *at*
  evaluation, `min_apply_index` is a catch-up bound satisfied by a `ReadIndex` round *before* it. A
  build that derived one from the other would answer from a state it had not reached.
* **Refusal is `FragmentResp::Refused { reason }`**, a variant of the response beside
  `FragmentResp::Result`. Never an error frame, never a `ProtoError`. A node that cannot reach
  `min_apply_index` inside its deadline — or does not implement a filter — is giving a *normal*
  answer meaning "fall back to a row scan". An error frame would make every rolling upgrade look
  like a fault, and the planner's fallback is not an error path.

So this lane's service:

* checks the header's epoch before anything else;
* satisfies `min_apply_index` by a `ReadIndex` round to the leader, waiting for its own apply to
  reach the returned index, and only then evaluates;
* applies visibility at `ts` (OPEN-2);
* **never partially honours anything** — it answers in full or returns `Refused`, because a
  partially-honoured fragment is a wrong answer with a plausible shape.
* The learner **never votes and never serves row reads.** Its applied index reports to PD like any
  learner; `heartbeat.rs` is untouched.

## Tests

* **Unit 1**: apply a known stream, read the run back, assert the cells and their `commit_ts`;
  seal-by-size and seal-by-age each produce the file they claim; a delete is a version, not a gap.
* **Unit 2**: run counts stay bounded under sustained apply; `kill -9` mid-merge and reopen — no
  acknowledged row is lost, no obsolete file is deleted that a live version names.
* **Unit 3**: epoch mismatch refuses; a lagging learner blocks on `ReadIndex` and then answers; a
  deadline refuses with the typed response and never an error frame.
* **Unit 4, the load-bearing one — the end-to-end differential.** A real region: three voters and
  one columnar learner, writes through Raft (mixed puts, deletes and rewrites across stripe
  boundaries), then **every generated fragment evaluated both ways** — columnar learner against a
  row scan at the same `ts` — asserting byte-equal partials. With: a lagging learner forced to
  catch up via `ReadIndex` before answering; a learner behind a snapshot; and `kill -9` on the
  learner mid-apply, re-verified after reopen.

That differential is the unit that decides whether this milestone is real. Everything above it is
machinery; it is the only test that can catch a columnar answer that is *plausible* and wrong.

## Not doing

No planner routing, no `EXPLAIN` naming the engine, no session override, no MPP, no tiering
rewrite. No catalog DDL for the per-table flag (Decision 5 is §wire's and later). No promotion of
a columnar learner to a voter — it is a learner that is never promoted, and nothing here should
make that reachable.
