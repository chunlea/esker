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

## The two seams, as ruled

Both were escalated rather than guessed, because getting either wrong is a rewrite of unit 1. Both
are now decided and recorded here with the reasoning that decided them.

### RULED-1: the row value codec moves down to `esker-keys`

The apply target must turn a committed row into `esker_columnar::Value`s. `esker-keys` gives the
store `split_table` and `split_ts` honestly; the row *value* was `esker_sql::row::decode_row`, and
`esker-sql` depends on `esker-store`, so the store calling it inverted the layering.

wy-c2 cut the question better than this plan's first draft did: the decoder needs **types**, which
can travel as *data*, and a **codec**, which is *code* and cannot. So the question was never which
port shape to use — it was where the codec lives.

**Ruled: it moves down to `esker-keys`**, which is the byte-meaning crate; the value format is the
other half of its one job. wy-c2 executes the move — a pure move plus re-export, goldens
byte-identical, with an ADR and the `CLAUDE.md` crate-table line. Two facts that made the
alternative unbuildable, both verified from the manifests: **no crate in this tree links
`esker-sql`** and it has no binary, so "inject from the composition root" had no *above*; and
placement will not carry schema, correctly, because PD carries *where* a replica lives and not
*what* a table looks like (the line phase 6e drew on invariant 7).

**This lane keeps building against the port** and swaps the constructor when the move lands:

```rust
trait RowDecoder { fn decode(&self, key: &[u8], value: &[u8]) -> Result<Vec<Value>>; }
```

Unit 1 takes one by injection; its tests use a hand-written fake. Nothing written against the trait
changes when the real implementor arrives.

### RULED-2: a version-aware scan mode, and this lane now owns all of `esker-columnar`

Decision 4 evaluates visibility **at read time**, so runs hold every version as committed. Applying
it means "the newest version of each key with `commit_ts <= ts`, unless that version is a delete" —
and M2's evaluator could not express it. `commit_ts <= ts` *is* a filter `Expr`; **newest-per-key is
an argmax**, and no `Expr` does that.

**Ruled**, and the lane expands to all of `esker-columnar` (its author retired; no other writer):

* **Runs are sorted `(pk, commit_ts DESC)`.** That is an ingestion and compaction decision, and it
  is what makes the read cheap: the newest visible version of a key is the *first* row for that key
  with `commit_ts <= ts`, so the scan is a single forward pass — no hashing, no buffering, no
  second sort.
* **The evaluator gains a parameter.** The visibility `ts` arrives from the **wire envelope**
  (§wire's field, Decision 4), and the **fragment payload does not change**. A fragment stays a
  description of *what to compute*; the timestamp is a property of *when to read*, and keeping them
  apart is what stops a fragment from being a different query at a different `ts`.
* **The differential's reference computes visibility by the same rule — and must implement it
  independently.** That distinction is the whole value of the harness: same *rule*, second
  *implementation*. A reference that called the same visibility code would make the comparison a
  tautology, which is the trap phase 7 named when it said the load-bearing unit is not the
  evaluator but the differential.

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

---

# §wire

*Owned by `wy-c2`. The fragment service, the catalog flag, and placement.*

## 0. What is already decided, and is not re-litigated here

The fragment *request payload* is a format that already exists, is versioned, and has goldens:
`esker_columnar::fragment::codec`, `FRAGMENT_FORMAT_VERSION = 1`. Milestone 2 built and fuzzed it
(402 million cases through the decoder). **This lane does not re-encode it and does not parse it.**

The *response* is the real gap. Milestone 2's report says so in as many words: the fragment
response is Rust types only — `FragmentResult { output, stats }` over `FragmentOutput::Rows` /
`Groups`, `Group`, `Partial` — and has no wire format at all. Defining one is unit 1's substance.

## 1. A service of its own: `Fragment`, `0x06`

Services `0x00`–`0x05` are taken (system, RawKv, TxnKv, Pd, Raft, Admin). The fragment gets
`SERVICE_FRAGMENT = 0x06` and one method, `FragmentEvaluate = 0x0601`.

Its own service rather than a seventh `TxnKv` method, for the reason `SERVICE_ADMIN` gives for
itself: *"they are not key-value work and must not be counted as it by anything watching request
rates."* A fragment is a plan evaluated on a node that may hold no voter at all. Anything counting
`TxnKv` rates, or routing on the service byte, must be able to tell the two apart without decoding
a body.

## 2. The request carries the fragment **opaquely**

```text
FragmentReq {
    fragment:         bytes,   // esker_columnar's format. Length-prefixed, never interpreted here.
    ts:               u64,     // ADR 0022 Decision 4, half one
    min_apply_index:  u64,     // ADR 0022 Decision 4, half two
}
```

**One definition of fragment bytes, and it is `esker-columnar`'s.** This crate carries them as a
length-prefixed byte string and has no opinion about their contents — the same treatment the lock
payload gets (`docs/adr/0016`, and `7ac5bb7` "the lock payload is opaque by decision, not for want
of a type"). A second decoder here would be a second definition of what a filter means, and two
nodes disagreeing about that is a wrong answer rather than a protocol error.

**`ts` and `min_apply_index` are two fields and neither is derived from the other.** Decision 4 has
two halves and they are different mechanisms: `ts` is MVCC visibility applied *during* evaluation,
`min_apply_index` is a catch-up bound satisfied by a `ReadIndex` round *before* it. A build that
inferred one from the other would answer from a state it had not reached.

**The epoch is not in the body.** `RequestHeader` already carries `{ region_id, epoch, peer }` on
every request, fragments included, so invariant 5 is satisfied by the field that is already there.
Adding a second epoch to the body would be a second source of truth about the same fact.

## 3. The response, which is the part that did not exist

```text
FragmentResp::Result  { result: bytes, stats: ScanStats }
FragmentResp::Refused { reason: RefusalReason, detail: string }
```

#### Refusal is a normal answer, never an error frame

ADR 0022 Decision 3's "refuse, never partially honour" rule crosses the wire as a **response
variant**. A node that does not implement an operator, or that cannot reach `min_apply_index`
inside its deadline, is answering correctly: the answer is *"fall back to a row scan"*, which is a
path the planner already has. An error frame would make every rolling upgrade look like a fault,
and would put a normal fallback on a caller's error path where retry logic and metrics live.

`RefusalReason` is an enum on the wire, not a string, because the planner branches on it:
`Unsupported` (this build cannot evaluate it — do not retry here), `TooFarBehind` (could not reach
the apply index — another replica may be closer), `NotColumnar` (this region has no columnar copy).
`detail` is for a human and is never matched on.

#### The result body is a format of its own

Version byte, hand-written little-endian body, CRC32C, golden — like everything else on this wire
(ADR 0002), and like the request it answers.

```text
result := version:u8=1 ++ kind:u8 ++ body ++ crc32c:u32     // crc covers version..body
kind    := 0 rows | 1 groups

rows   := ncols:varint ++ type:u8 * ncols
        ++ nrows:varint ++ (value * ncols) * nrows

groups := nkeys:varint ++ type:u8 * nkeys
        ++ naggs:varint ++ (agg_kind:u8 ++ agg_type:u8) * naggs
        ++ ngroups:varint ++ (value * nkeys ++ partial * naggs) * ngroups

value   := 0x00                      // NULL
         | type_tag:u8 ++ payload    // as esker_columnar::fragment::codec writes a literal
partial := count: varint | (sum|min|max): value
```

**Types are declared once in a header, not per value** — except for the null-or-type
tag each value already needs. The declaration is what makes a schema mismatch a typed error instead of a silent
misparse, and that distinction is one this project has already paid for: M2's format version 2
exists because *a checksum proves a block is intact, not that it is the block that was asked for.*
A result decoded against the wrong column list has exactly that shape — intact bytes, wrong
meaning — and a declared header turns it into a refusal to decode.

#### The type tags are copied, deliberately, and pinned

`1 Int8, 2 Text, 3 Bool, 4 Bytea, 5 TimestampTz, 6 Double`. These are already a frozen vocabulary
shared by two crates that do not link: `esker_sql::catalog::record`'s `TAG_*` constants are the
original, and `esker_columnar::value` copies them with a test named `tags_match_the_row_side`
saying so. This crate becomes the third, the same way, with the same kind of test.

Copied rather than linked because the alternative is worse in both directions: `esker-proto` cannot
depend on `esker-sql` (which sits above it) and must not depend on `esker-columnar` (a leaf whose
format versions would then be able to break the wire). The house answer to that is already on the
page — copy the constants and pin them with a test that names its source — and a third copy with a
third pinning test is consistent rather than novel.

#### `sum(double)` does not associate, and the response docs say so

Combining partial aggregates adds numbers in a different order than a single-level fold would, so a
`sum(double)` finished from many fragments may differ in its last bits from the same query run over
one. `esker_columnar::scan::group::Partial::combine` documents it at the point of combination; this
is the other place a reader meets it, because the wire is where "many fragments" becomes true. It
is inherent to two-level aggregation, not a defect, and it is the reason a fragment folds its own
answer in row order across every stripe rather than per stripe and combined.

#### `stats` is a typed field, not part of the format

`ScanStats` — five counters — rides beside the result rather than inside it, because it is not part
of the *answer* and a build that ignored it would still be correct. Carried now rather than at
milestone 4 because `EXPLAIN` is the named consumer and a field added later costs a version bump.

## 4. What this lane will **not** carry

**A schema.** Placement carries where a replica lives, not what a table looks like. A placement
operator naming a table's columns would make PD a carrier of SQL semantics, which is the exact line
`docs/plans/phase-6e.md` §10 drew when it took the schema-step drive away from PD on invariant 7 —
PD cannot read a table definition, let alone write one. Placement carries a table id and a desired
replica count: a number and an id.

The learner's decoder is therefore `cl-c1`'s seam and not a field of mine. The types it needs can
travel as data; the *codec* that walks a row's bytes cannot, and something below `esker-sql` has to
own it. That is an ADR-level question crossing both halves — recorded here as **OPEN**, and put to
the coordinator rather than settled between two lanes mid-flight.
