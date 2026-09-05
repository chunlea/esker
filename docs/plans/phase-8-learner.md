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

## The schema the decoder is built from, and the ordering it needs

RULED-1's port needs a schema pushed to it. Three things settled that shape, none of them obvious
from this side of the seam:

* **One current schema, never versioned by `ts`.** `DROP COLUMN` does not exist yet (`0A000`), and
  ADR 0019 Decision 3 says it arrives as row format **v3 with per-value column identity** — not a
  renumbering and not a rewrite. Position stops mattering, so current-schema decode stays sound in
  both directions and no schema history is needed.
* **The schema is a *pair*, not a type list.** `RowSchema` is `{ types, missing }`, and `missing`
  is PostgreSQL 11's `attmissingval` — what makes `ADD COLUMN ... DEFAULT <constant>` instant on a
  populated table. A decoder built from types alone pads `NULL` where the row store pads the
  default, so for a table that took `ADD COLUMN c int8 NOT NULL DEFAULT 42` **every row written
  before that ALTER reads 42 through the row store and NULL through the columnar copy** — silently,
  and only for the old rows. Found by wy-c2 reading `row.rs`, which says of itself that this is
  "the hardest case to notice".
* **The push must be ordered against the log, and lateness is absorbed as lag.** `decode_row`
  *refuses* a row wider than its schema — that is corruption, not a case to tolerate — so a stale
  schema does not read wrong, it stalls the apply path. Carrying the schema in the log does **not**
  fix this: the catalog record lives wherever its key falls and the table's rows live in the regions
  covering theirs, so they are different Raft logs with no ordering between them, and the ordering
  is only free in the single-region case that ends at the first split.

  So the apply path **waits** — and the wait must not block, because a driver worker holds many
  regions and parking one parks the rest (wave C). It is not a sleep: the region stops advancing its
  applied index until the schema arrives. The pleasant consequence is that this degrades into a
  state the system already handles: *a learner that cannot apply is a learner that is behind*, which
  the heartbeat reports, `min_apply_index` catches, and `RefusalReason::TooFarBehind` turns into a
  row-scan fallback. No retry policy, no backoff, no deadline.

The monotonic number is `TableDef::schema_version` (ADR 0019 Decision 4), which already increments
on exactly this event. A push carrying an older one is refused over a newer.

## Tests

* **Unit 1**: apply a known stream, read the run back, assert the cells and their `commit_ts`;
  seal-by-size and seal-by-age each produce the file they claim; a delete is a version, not a gap.
* **Unit 2**: run counts stay bounded under sustained apply; `kill -9` mid-merge and reopen — no
  acknowledged row is lost, no obsolete file is deleted that a live version names.
* **Unit 3**: epoch mismatch refuses; a lagging learner blocks on `ReadIndex` and then answers; a
  deadline refuses with the typed response and never an error frame.
* **The differential corpus must contain the case that hides.** An `ADD COLUMN` with a **non-NULL
  default** and rows written **before** it, because that is the shape the `missing`-value bug above
  produces and it is invisible to every other. A harness that generates schemas up front and then
  fills them will never produce it, and would pass while the columnar copy answered `NULL` where the
  row store answered `42`. Corpus generation therefore has to interleave DDL with writes rather than
  ordering them.
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

## 4. Placement: PD is **told**, and the invariant-7 argument for why

`brief-a2` offered two shapes and asked for the argument in writing: PD reads the catalog record
byte-level, or the SQL node reports desired counts. The first is not buildable, and the reason is
worth separating from the layering question it looks like.

**PD has no access path.** `esker-pd` links `esker-engine` — its *own* database — plus
`esker-proto` and `esker-base`. It does not link `esker-client`, and every method on its service is
inbound: `Bootstrap`, `StoreHeartbeat`, `RegionHeartbeat`, `GetRegion`, `AllocId`, `Tso`,
`SchemaLease`. Stores and SQL nodes call PD. **PD calls nobody.** The catalog record lives in the
cluster's key space, which PD reaches only by being a client of the cluster it places — a
dependency inversion, and a bootstrap problem besides, since PD has to work before any region is
servable.

Moving the record's codec to `esker_keys::columnar` (ADR 0030, applied twice) made those bytes
*parseable* from PD. It did not make them *obtainable*. Those are different problems and only the
first is closed by a crate move.

**So the SQL node reports, and what it reports is a key range.** `Pd::ReportColumnar` carries
`(start_key, end_key, replicas)`, and the choice of *range* over *table id* is where invariant 7 is
kept:

> Key semantics (tenant, table, MVCC suffix) live only in `esker-keys` and above.

A range is not a key semantic — it is PD's own vocabulary, the thing a region *is*, and PD already
reasons in nothing else. Told "the range `[a, b)` wants two columnar replicas", PD compares it
against every region's `[start_key, end_key)` and never learns that a table exists. Told "table 7
wants two", PD would have to know what a table is and where its rows live, which is exactly the
line `docs/plans/phase-6e.md` §10 drew when it took the schema-step drive away from PD: *PD is
byte-opaque and cannot read a table definition, let alone write one.*

Ranges also happen to be split-safe, which a table id would not have been: a table that splits into
four regions is still one range, and every overlapping region inherits the wish with nobody
re-reporting.

### The report is a full assertion

Never a delta. Every SQL node reads the same catalog, so every report has the same content and the
last writer is right whoever it was; a delta would need an ordering this service does not impose.
It also turns the re-report on lease refresh into anti-entropy rather than duplication — a report
lost to a restart is repaired by the next one, with no acknowledgement protocol.

PD persists it, unlike the operators it has in flight, and the difference is where the fact lives.
An operator can be recomputed from the next heartbeat because the cluster is the source of truth
about its own membership. This cannot be: a restart that forgot it would retire every columnar
replica in the cluster and wait for a SQL node to mention them again.

### Scheduling sits between repair and balance

After repair, because a region below its voter target is in trouble and a columnar copy is a
convenience — the one operator slot a region gets belongs to the repair. Before balance, because a
missing columnar copy means a query falling back to a row scan every time it runs, while an
unbalanced cluster is merely uneven.

### A note on a filter that was accidentally safe

`esker-store`'s `promote_caught_up_learners` selects `role == PeerRole::Learner`, *positively*, so
a third role is excluded from promotion by construction and the columnar variant needed no edit
there. Written as `!= PeerRole::Voter` it would have promoted every columnar replica the moment the
variant existed. `cl-c1` made it an exhaustive match rather than leave it resting on that, which is
right: accidentally safe is not safe, and a fourth role would fall into whichever default was
there.

## 5. What this lane will **not** carry

**A schema.** Placement carries where a replica lives, not what a table looks like. A placement
operator naming a table's columns would make PD a carrier of SQL semantics, which is the exact line
`docs/plans/phase-6e.md` §10 drew when it took the schema-step drive away from PD on invariant 7 —
PD cannot read a table definition, let alone write one. Placement carries a table id and a desired
replica count: a number and an id.

The learner's decoder is therefore `cl-c1`'s seam and not a field of mine. The types it needs can
travel as data; the *codec* that walks a row's bytes cannot, and something below `esker-sql` has to
own it. That is an ADR-level question crossing both halves — recorded here as **OPEN**, and put to
the coordinator rather than settled between two lanes mid-flight.

---

# §wire — handover state (lane wy-c2, end of wave A)

## THE ONE GAP: nothing calls `Pd::ReportColumnar` yet

The wire method, PD's durable record and the scheduling are all in and tested. **The caller is
not.** `esker-sql`'s `ALTER TABLE ... SET (columnar_replicas = N)` writes the catalog record and
does *not* report to PD, and the lease refresh does not re-assert. So today the flag is durable and
readable and PD never hears about it, which means no columnar learner is ever placed on a real
cluster.

What a fresh lane needs to add, both in `esker-sql`:

1. After the `ALTER` commits, scan `catalog::columnar_range(tenant)`, build one
   `esker_proto::pd::ColumnarWish` per table from `esker_keys::row::table_row_range(tenant, id)`
   and the replica count, and send `PdReq::ReportColumnar { wishes }`. It is a **full assertion**,
   so the scan is the message — do not send a delta.
2. Re-send the same on every schema-lease refresh, which is the anti-entropy sweep the design
   assumes. A report lost to a PD restart is repaired by the next one and there is no
   acknowledgement protocol precisely because of this.

The SQL node has no `PdClient` today — `esker-sql`'s binary takes store addresses, not PD's
(`connect`'s `TODO(phase-6a)`), which is the same reason the schema lease is unattached and the
re-driver ships inert. So this is one piece of work with the lease's, not two.

## What is done, with the numbers

* `Pd::ReportColumnar` (0x0308), goldens for request and response; `ColumnarWish` carries
  `(start_key, end_key, replicas)`.
* PD's `ColumnarRecord`, persisted under `'m' 'l'`, golden-pinned, loaded at open.
  `wanted_for(start, end)` takes the **maximum** over overlapping wishes.
* Scheduling in `pd::repair::plan`, between repair and balance; `schedule::columnar_for`.
* `Operator::AddLearner` (kind 4) + `EventKind::AddLearner`; `PeerRole::ColumnarLearner = 3`.
* Six tests in `crates/esker-pd/tests/columnar.rs`. Balance is **off** in that harness on purpose.

## Two masking bugs found, both the same shape

Both counted non-voters towards a voter target, and both read *healthy* on a cluster that was not.
Worth stating together because a third of the same family is plausible wherever a count is taken
over `peers` rather than over voters:

* `schedule::urgency_for` counted all live peers against `target_replicas`.
* `schedule::repair_for` counted a columnar learner as "a replacement already on its way", which it
  never is — it is never promoted.

## Leads left for somebody else, with evidence

* **`WalSyncMode::Never` may not be disabling what it names.** `docs/bench/columnar-learner.md`:
  2075 of 2114 sampled stacks in `DbInner::commit_group`, ~4.8 ms per single-row put, one writer,
  no contention — with sync **disabled**. So whatever it waits on is not an fsync. Recorded as a
  lead rather than a shrug; neither this lane nor `cl-c1` owns `esker-engine`.
* **Row format v3 (column identity) is what `DROP COLUMN` needs** — ADR 0019 Decision 3. When it
  lands, v2 rows still exist in a table that then takes a drop, and those are exactly the rows
  Decision 3 says cannot be read: v3 either rewrites surviving v2 rows or refuses a drop on a table
  that still has them. The columnar copy inherits whichever it is.
* **`esker_columnar::Value` and `esker_keys::value::Datum` are now the same six shapes with the
  same tag bytes**, defined twice. ADR 0030 makes converging them possible and deliberately does
  not do it.

---

# §wiring — the SQL node's PD connection (lane a5-wiring)

Three finished features are dormant in the real binary for one reason: **the SQL node has no PD
connection.** The schema lease is never fetched, so fail-closed never arms; the re-driver reads its
interval from the lease and so ticks against nothing; and `ALTER TABLE ... SET (columnar_replicas
= N)` writes a catalog record PD never hears about, so no columnar learner is ever placed. Nothing
below this line is a new feature. It is one cable and the proof that the circuit closes.

## Where the PD client lives, and why not in `esker-client`

`esker-store`'s `PdClient` trait carries the five **store** methods — bootstrap, alloc_id,
get_region, and the two heartbeats — and neither `SchemaLease` nor `ReportColumnar`. That is
correct and this lane does not touch it: those two are a *SQL node's* methods, and a store has no
business holding a lease it does not use.

So the SQL node needs a connection of its own, and it goes in **`esker-sql`**:

* it speaks exactly two methods, both of which only a SQL node sends — putting them in
  `esker-client` would make every raw-KV client carry a vocabulary it has no use for;
* `esker-sql` already depends on `esker-proto`, so it costs no new edge;
* `esker-client` stays untouched, which is the smaller blast radius on a shared tree.

If a second consumer ever appears the type moves down; one consumer is not an abstraction.

**`BlockingTransport`, not the async one.** The lease refresher is a thread, like the re-driver
beside it, for the reason that file already gives: a refresh sleeps between passes and would hold
a runtime worker for the whole of one. `BlockingTransport` is built for exactly that caller, and
every call it makes carries a deadline — *"a blocking call with no deadline is a hang"*, which this
codebase has already paid for once.

## `--pd <addr>`, and absent means absent

A new flag, and **nothing changes without it**. No default address, no discovery, no fallback. A
node started without `--pd` behaves exactly as it does today: no lease, `schema_step_interval()`
returns `None`, the re-driver logs that it has no interval and waits, and writes are unrestricted.
Every existing test runs without PD and must keep passing untouched — that is the check that this
flag is genuinely additive.

With it, the node builds the PD connection, fetches a lease before serving, and starts a refresher
thread.

## Refresh cadence and failure semantics — read, not invented

The numbers are PD's and the rules were argued in 6e/ADR 0028. This lane wires them; it does not
re-derive them.

* `PdResp::SchemaLease` carries `lease_ms`, `step_interval_ms` and `removal_extra_ms` **together**,
  because PD computes the interval *from* the lease. A node holding one without the other holds
  half an arithmetic, so the source answers `None` to both or neither.
* **Refresh well inside the lease**, so that a single lost round trip does not expire it. The
  cadence is a fraction of `lease_ms` and is derived from PD's number rather than configured — a
  tunable here is a way to be wrong independently of the bound it exists to respect.
* **Fail closed.** A source that cannot renew answers `None` to `remaining()`, and the backend
  refuses **writes** with 6e's typed error. Reads are untouched: a reader's snapshot already agrees
  with the rows it can see, so gating reads adds stalls and closes no hole (ADR 0020 as amended).
  That asymmetry is the design's, not a convenience.
* The `SchemaLease` trait stays the seam. It exists so *"the test that proves fail closed has to be
  able to stop answering"* — so the real source implements it and the lapse test drives it by
  stopping PD, not by swapping in a fake.

## `ReportColumnar`: a full assertion, twice

Per §wire's handover, and neither half is optional:

1. **After the `ALTER` commits** (`exec/ddl.rs`), scan `catalog::columnar_range(tenant)`, build one
   `ColumnarWish` per listed table from `esker_keys::row::table_row_range(tenant, id)` and its
   count, and send them all. **The scan is the message** — it is a full assertion, never a delta,
   so a report is complete by construction and a lost one costs nothing.
2. **On every lease refresh**, re-send the same. That is the anti-entropy sweep the design assumes,
   and it is why there is no acknowledgement protocol: a report lost to a PD restart is repaired by
   the next refresh rather than by a retry queue that would need its own durability.

Setting the flag to `0` clears the record, so the next assertion simply omits that table — removal
falls out of the full-assertion shape rather than needing a message of its own.

## The joint gate, which is the wave's definition of done

A real cluster, not a harness: `esker cluster start`, a SQL node with `--pd`, `CREATE TABLE`, rows,
`ALTER TABLE t SET (columnar_replicas = 1)`, then **wait for PD to schedule and the store to build
the learner**. Then the differential — fragment against row scan at one `ts` — run against a
learner that a DDL statement caused to exist. Then the flag to `0` and the learner retires; then
`kill -9` mid-catch-up and it recovers.

Everything it exercises is already tested in isolation. What it proves is that the pieces are
*connected*, which is the one thing no unit test in this phase can say.

## Not doing

No routing table from PD (`connect`'s `TODO(phase-6a)` stands — this lane adds a PD connection for
the lease and the report, not a region resolver). No changes to `esker-store`, `esker-columnar`,
`esker-pd`, `esker-proto`: all of it exists, is tested, and is forbidden to this lane. A defect
found there is reported with evidence, not fixed.

## The joint gate did NOT pass, and the blocker is probably this lane's

`esker-cli::cluster_chaos::a_sigkilled_leader_process_never_costs_an_acknowledged_write` ran for
**over an hour** and did not finish. Sampled before anything was killed (`sample` on the live pid):
the test thread is `cluster_chaos::battery` → `drive` → `esker_client::raw` → `router` →
`tcp`/`clock`, with backoff sleeps and parks. That is the client **retry** path.

The most likely cause is `aaffec7` — wave C's `retry::may_ask_again`, which re-asks a read whose
answer was lost instead of surfacing it. Before that change a lost read answer returned at once;
after it, the call can spend `max_retries` with backoff, bounded only by the 10 s call deadline. A
loop making many such calls while a leader is dead turns a fast failure into ten seconds, over and
over.

**The retry rule itself is defensible and this note is not a request to revert it.** A read may
always be re-asked, and refusing to was manufacturing refusals out of dropped packets — that is
what `retry.rs`'s own module doc had named and left. What is probably wrong is that
`cluster_chaos`'s loop was written against a client that failed fast, so the change turned a
bounded wait into a much longer one. The fix is more likely in that test's loop or its deadline
than in `may_ask_again`.

That is a hypothesis from **one sample**, and it should be confirmed before anything is changed.
The cheap confirmation: run that test with `aaffec7`'s `may_ask_again` branch disabled and see
whether it completes. `crates/esker-client/tests/chaos_linearizability.rs` is the in-process twin
of it and completes in ~5 s, which is itself evidence — whatever this is, it involves the real
process boundary rather than the rule alone.

Not fixed here: this lane reached the end of its context, and a hurried change to the retry path is
how a good fix becomes a bad one.

---

# §wiring — what was built (lane a5-wiring)

The cable is in and the circuit closes: on a real cluster, `ALTER TABLE t SET
(columnar_replicas = 1)` typed into `psql` now causes a columnar replica to be placed, and setting
it to `0` takes it away. `docs/bench/columnar-learner.md` carries the transcript.

## The five units, and what each one turned out to be

1. **The cable.** `esker-sql --pd HOST:PORT`: a `PdConn` speaking the two methods only a SQL node
   sends, a lease fetched before the node serves, and a refresher thread. Absent `--pd` changes
   nothing — no lease source, unrestricted writes, no interval — which is what every existing test
   in the crate keeps checking by carrying on unmodified. The lapse test stops the placement driver
   and watches writes turn into `25006` while the same session keeps reading.
2. **The re-driver, live.** Two SQL nodes over three stores; one abandons a `CREATE INDEX
   CONCURRENTLY` after one step and the other's `run()` loop finishes it on PD's cadence. The
   refresher is load-bearing there rather than incidental: the test runs for several lease terms
   and `ReDriver::pass` fail-closes on a lapsed lease, so a hang would mean a renewal stopped.
3. **The report.** After the `ALTER` commits and on every lease refresh, as a full assertion built
   from a scan of `catalog::columnar_range`. A rolled-back `ALTER` reports nothing; a block's
   commit reports at the commit; a table set to `0` is absent rather than zero.
4. **The gate.** A real `esker-pd`, four real stores, the real node wiring. Placement, retirement,
   and a learner killed and reopened that comes back to the same job.
5. **The smoke.** `esker cluster start --pd` now starts a placement driver and prints the
   `esker-sql` line that goes over it; `esker region ls` prints `C` for a columnar learner.

## Two defects found by running it, one fixed here and one not

**Fixed (this lane's own layer).** `esker-sql <listen> <store>...` panicked with *"Cannot start a
runtime from within a runtime"* on its first line, before it listened for anything: `connect`
builds a synchronous client, which owns a runtime, and it was called straight from
`#[tokio::main]`'s. It had been true since the binary learned to take store addresses; every test
built its client on a blocking thread, so the panicking path was the binary's own startup and
nothing walked it until a shell did.

**Not fixed, and not this lane's to fix.** `balance::region_balance` counts
`region.peers.len() > cluster.target_replicas` over **every** peer, so a healthy three-voter region
that gains a columnar learner is four against three and balance sheds a **voter**. PD's own
operator history, from `esker pd inspect` after the run:

```text
   1788233921975 ms  region 1  AddLearner  done       store 4  peer 5
   1788233921975 ms  region 1  RemovePeer  issued     store 0  peer 3
   1788233922074 ms  region 1  AddPeer     issued     store 1  peer 6
   1788234222175 ms  region 1  AddPeer     timed out  store 1  peer 6
```

Same millisecond, no store down. The region sat at two voters for the five minutes the replacement
took to time out, and every operator behind it — including the next `ALTER`'s removal — waited.
This is the **third** instance of the family §wire named at the end of wave A, after `urgency_for`
and `repair_for`: *a count taken over `peers` rather than over voters*, reading healthy on a
cluster that is not. Handed over as a red test rather than a paragraph:
`esker-sql/tests/joint_gate.rs::a_columnar_learner_does_not_cost_the_region_a_voter`, `#[ignore]`d,
three seconds to reproduce.

## What the gate does NOT prove, and the evidence

The brief's gate ends with the differential — a fragment evaluated against the learner against a
row scan at one `ts`. **It cannot run, and the reason is not in this lane.** `esker-store` has no
columnar apply target on a live region:

* `Store::serve_fragment` returns `Refused { NotColumnar }` unconditionally, and says of itself
  that *"until the apply target is placed on regions (phase 8 unit 3), this is the only answer this
  store has"*;
* `ColumnarApply::open` is called from no path in `esker-store/src` — only from that module's own
  tests, `tests/columnar_differential.rs` and `tests/bench_columnar.rs`.

So a columnar learner today is a learner whose *region record* says it is columnar, applying rows
like every other learner. §store units 1 and 2 built the apply target and the runs; unit 3 — the
fragment service on a live region — is what stands between here and the differential.
`joint_gate.rs::the_fragment_service_still_refuses` pins that as a fact rather than a claim, and is
the test to replace with the differential when unit 3 lands.

The brief's premise that "the store's apply target [and] the fragment service ... ALL exist and are
green" is true of the modules and not of the paths: both exist, neither is reachable from a running
store. Worth saying plainly, because a wave that reported the gate green on the placement half
would have left the next lane to discover the other half was never wired.

## Not done, deliberately

* **No literal `SIGKILL`** in the gate: a store there is an object in the test's process, so the
  crash is an abrupt stop and a reopen. The between-process question belongs to `esker-cli`'s
  chaos battery, which owns it for every store in the cluster.
* **No routing table from PD.** `connect`'s `TODO(phase-6a)` stands: this lane added a PD
  connection for the lease and the report, not a region resolver. The static route is corrected by
  the store's own `EpochNotMatch`, which is how a conf change reaches the client today.

## The hour-long chaos test did not reproduce — twice, at this HEAD

The section above this one records `cluster_chaos`'s sigkill test running for **over an hour**
without finishing, with `retry::may_ask_again` named as the likely cause from one sample. It did
not reproduce here, and the numbers are worth having beside the hypothesis rather than instead of
it:

```text
cargo nextest run --workspace --all-features
    Summary [141.869s] 2242 tests run: 2242 passed (17 slow), 34 skipped

cargo nextest run -p esker-cli --test cluster_chaos
    PASS [9.162s] a_sigkilled_leader_process_never_costs_an_acknowledged_write
```

Two clean runs at `1c66c08`, on a machine that was otherwise quiet. That does **not** refute the
observation — a nine-second test and an hour-long one can be the same code under different load,
and the original run was taken while several agents were compiling and running suites in this tree
at once. What it does say is that the hang is **not deterministic at this HEAD**, so the cheap
confirmation the note asks for (disable `may_ask_again` and see) would now be measuring a test that
already passes. Whoever picks this up should reproduce the hang *first* — under load, with a sample
taken while it is wedged — rather than changing the retry path against a green test.

`fifty_sigkills_of_the_leader_process`, the long form of the same battery, is `#[ignore]`d and was
not run.

---

# §store unit 3b — done, and where a resuming lane starts

The apply target is on live regions and the fragment service answers from it
(`crates/esker-store/src/columnar/region.rs`, `Store::serve_fragment`). What that commit does and
what it deliberately does not is in its message and in the module header; the short version is
that a columnar learner now **tees** its committed row versions into a per-table columnar copy,
rebuilds that copy from the region's own `write` records whenever it opens one, and serves
fragments over every live run at `ts` after satisfying `min_apply_index` with a learner
`ReadIndex` round.

Proven in `crates/esker-store/tests/columnar_region.rs`: built from history, extended by the
apply, read at three timestamps, refused for a table with no record, rebuilt on reopen.

## THE BLOCKER a resuming lane meets first

**A columnar learner PD places never receives the region's existing data.** Not a wiring fault and
not this lane's: it is the learner-creation path. From the store's own log on a four-node cluster,
with the SQL node driving:

```text
INFO a region arrived by snapshot region_id=1 index=0
WARN a snapshot did not arrive; the leader will offer it again region_id=1 index=0
     error=invalid request: peer 5 is not a member of region 1 and may not have a copy of it
```

Measured on the learner at the moment a fragment asked, and fifteen seconds later: **published
apply index 22, two versions in its `write` column family**, where the leader had ten. The ask in
`Store::receive_raft`'s unknown-region path races the conf change that placed the peer — the sender
checks the asking peer against a membership it has not applied — and the retry lands a region
record at index 0, which carries no data. A **row** learner survives this because the log catches
it up and promotion then waits on it; a columnar learner is never promoted, so nothing ever notices
it is empty.

So `the_learner_answers_fragments_that_agree_with_a_row_scan` in
`crates/esker-sql/tests/joint_gate.rs` is written, correct, and `#[ignore]`d. It is the first thing
to un-ignore, and it will pass when a placed learner receives what the region already holds.

## Item 2 — `esker-pd`'s peer counting, and its regression test

`crates/esker-pd/src/balance.rs`, in `region_balance`:

```rust
if region.region.peers.len() > cluster.target_replicas && !mid_repair {
```

`peers.len()` counts **every** peer, so a healthy three-voter region that gains a columnar learner
is four against a target of three and balance sheds a *voter*. Count voters for a shed decision —
`peers.iter().filter(|p| p.role == PeerRole::Voter).count()` — and pick the heaviest from the
voters too, since a columnar learner is not a replica this rule may give back (removing one is
`schedule::columnar_for`'s decision and nobody else's).

The regression is already written: un-`ignore`
`joint_gate.rs::a_columnar_learner_does_not_cost_the_region_a_voter`. It asserts what should be
true — the region keeps `VOTERS` voters after the `ALTER` — and it fails today in under three
seconds with `balance: true`. Evidence, including PD's own operator history, is in the section
above and in `docs/bench/columnar-learner.md`.

Watch for a fourth instance of the same family while in there: any count over `region.peers` that
means *voters*.

## Item 3 — the real differential joint gate

Nothing to write: the test exists, and its reference is a second implementation rather than a
second call (a row scan at the same `ts` through Percolator's records, decoded with the row codec).
It needs the blocker above fixed, and then:

1. un-`ignore` it and run `cargo test -p esker-sql --test joint_gate -- --ignored`;
2. replace `docs/bench/columnar-learner.md`'s "the story end to end" section's last paragraph — the
   one that says the fragment service refuses — with what a fragment now answers;
3. widen the corpus before calling it done. One update and one delete is enough to tell "every
   version" from "the visible version" and is **not** enough for the case §store names as the one
   that hides: an `ADD COLUMN` with a non-`NULL` default and rows written before it, which reads
   the default row-side and NULL columnar-side if the decoder is built from types alone. The
   schema push that would make that case reachable on a live region does not exist yet either
   (`region.rs` reads the published record from the catalog the region carries), so a table whose
   rows live in a region that does not cover `'m'` still has no copy at all — the third thing this
   milestone owes.

---

# §close — wave A closed (lane a6-close, 2026-09-01)

The three things the section above left are done, and the joint gate runs with **nothing
`#[ignore]`d**. What follows is what each one turned out to be, because in two of the three the
diagnosis in that section was the symptom rather than the cause.

## 1. The empty learner was a snapshot carrying one column family out of three

The blocker above reads the trace as a race: the ask in `Store::receive_raft`'s unknown-region path
against the conf change that placed the peer. That race is real and it is in the code's own words —
`stage_conf_change` says `applied_conf` is "a different value from the core's membership in force,
which moved when the entry was *appended*" — so a leader can and does refuse the first ask. It also
heals on the next append, and it is not why the learner was empty.

The cause is that `snapshot::read_pairs` walked `cf::DEFAULT` under the `'r'` namespace and nothing
else, while a region's data lives in `default`, `lock` and `write`, and everything above RawKV is
transactional and therefore under `'x'`. A snapshot carried a region's RawKV pairs and dropped
every Percolator record in it. [ADR 0032](../adr/0032-a-snapshot-carries-every-column-family.md) is
the decision; format version 2 names a column family per chunk and ships engine keys.

**Reproduced from the recorded trace before anything was changed**, in 0.7 s and with no cluster,
as `esker-store/tests/snapshot.rs::a_region_arrives_with_its_transactional_records` — and then, in
the trace's own shape, `a_placed_columnar_learner_holds_what_the_leader_holds`, which failed with
the learner at the leader's applied index holding only the versions committed *after* it was
placed. That is the "two versions where the leader had ten" of the section above, exactly.

The reason it was found here and not in phase 4 is worth keeping: a row learner is caught up by the
log when the leader has not compacted, and when it is caught up by snapshot instead, promotion
waits on `matched` — a number the transfer moves whether or not the bytes were complete. Nothing
reads its `write` records until it leads. A columnar learner is never promoted and its whole job is
to read those records. The user-facing shape needs no columnar anything, and now has its own test:
`a_voter_caught_up_by_snapshot_can_lead_and_answer_an_old_row`, which on version 1 answers
`Get { value: None }` for a committed row, from a leader. The ADR carries the greps showing why no
acceptance suite could have caught it.

## 2. Balance's third instance, and a fourth beside it

`balance::region_balance` counts voters now. The shed decision was the third instance of the family
after `urgency_for` and `repair_for`; `busiest`, in the same function's second half, was a fourth —
that rule can only *move* a voter, so measuring the imbalance over a columnar learner it may not
touch starts a move that does not relieve the store that triggered it. Confirmed both ways in a
detached worktree at HEAD (red at 2 voters of 3, green at 3), and then on the real binaries: six
operators, no `RemovePeer`, in `docs/bench/columnar-learner.md`.

## 3. The differential passed, then the widened corpus broke it

`the_learner_answers_fragments_that_agree_with_a_row_scan` passed as written once the snapshot fix
landed. Widening the corpus to the case §store named — `ADD COLUMN region text NOT NULL DEFAULT
'unknown'` over rows written before it — refused instead:

```text
Refused { reason: Unsupported,
          detail: "a run of 6 columns cannot be read as 5: it was written under a newer schema" }
```

`Store::serve_fragment` passed `widening: None`, so `evaluate_merged` took its target schema from
`readers[0]` — the *oldest* run. After an `ADD COLUMN` the runs are a mix of widths by design, and
the two ways the oldest run is the wrong target are the two halves of one defect: a wider run is
refused outright, and had the widest sorted first, the older ones would have been padded with NULL
where the row store pads with the column's DEFAULT — the silent disagreement.

The mechanism was already built **and already tested**: `columnar_differential.rs`'s widening test
constructed the `Widening` by hand, `Value::Int8(42)` and all, and passed. That is the same shape
as "`ColumnarApply::open` is called from no path in `esker-store/src`" one unit earlier — a module
that is right, and a caller that never uses it. The test now takes its widening from
`ColumnarApply::missing()`, which is what the store passes.

## What wave A does not have, for whoever picks up B

* **Nothing on a real cluster can ask a fragment.** No `esker` subcommand sends one and the planner
  does not choose a columnar replica, so the differential's evidence is the in-process gate — real
  PD, four real stores over real sockets, the real SQL node wiring, one process. That is the honest
  boundary of what has been shown end to end.
* **The schema push for a region that does not cover `'m'`** is still absent, as §store said:
  `region.rs` reads the published record from the catalog the region carries, so a table whose rows
  live elsewhere has no copy at all. The gate is single-region and does not reach it.
* **`esker cluster start --nodes N --pd` starts its stores before the driver is listening** and
  three of four die with "connection refused", silently, because the supervisor inherits their
  stdio and then blocks. `esker-cli` is another lane's; the transcript is in the bench doc.
* **The `receive_raft` ask still races the conf change.** It costs a retry and heals, and it is now
  the only part of the original blocker that is left. Worth fixing when someone is in `server.rs`
  with a reason: the sender could check the *core's* membership rather than the applied record.

## One observation this lane could not reproduce, recorded rather than dropped

The differential's own assertion failed **once**:

```text
thread 'the_learner_answers_fragments_that_agree_with_a_row_scan' panicked at
crates/esker-sql/tests/joint_gate.rs:729:5
```

Line 729 is `assert_eq!(columns, rows, "the columnar copy and the row store disagree at ts {ts}")`
— the two engines, not the reference. It happened on the first run after the gate's oracle changed
from `CountingOracle` to a physical-millisecond one, and **fourteen runs since have been clean**:
eight of that test alone and six of the whole file in parallel. The assertion's output was lost to
a `grep` in the command that ran it, so what differed is not known.

Two candidates, and no evidence separating them. Either it is a pre-existing race the counting
oracle's small timestamps happened to hide, or the physical oracle introduced it — a real TTL means
a lock **can** now be judged dead, so a transaction slow between prewrite and commit can have its
locks resolved underneath it, which the counting oracle made impossible. The second is the one to
look at first, because it is new.

### What was done about it, since a pass rate is not evidence

**1. The evidence cannot be lost again.** A `columns != rows` disagreement writes
`target/joint-gate-disagreement-<ts>.txt` before it panics and names the file in the panic: both
sides, the rows only one side has, the instant, the fragment's shape, and per store whether it
leads, what it has applied, whether it is the columnar learner, and for each row of the workload
how many `write` records it holds and what a direct read answers. Those last two are the diagnosis
— a version the learner does not *have* is a catch-up or a tee, one it has and does not show is
MVCC, one with a lock over it is the resolution path.

**2. The named candidate is constructed and ruled out.**
`a_lock_the_ttl_kills_resolves_the_same_way_on_both_engines` drives the interleaving rather than
waiting for it: a transaction prewrites two rows and commits only its primary, the wall clock
passes the lock's TTL, the row scan resolves the standing lock against a committed primary and
**rolls it forward**, and the columnar copy is asked about the same instant. That `write` record is
created by `ResolveLock` and not by `Commit`, so a tee watching commits alone would hold every
version except those a resolver produced — for ever, and only for transactions whose client died
at exactly the wrong moment. `peer::commits_of` covers it and the test says so from outside.

It is not a vacuous pass: with that arm of `commits_of` removed, the test fails and the dump reads
*only the row scan has* `[Int8(5), Text("barbara")]` while every store, learner included, shows
`id 5: 1 write records` — the version in the region and not in the copy, which is the sentence the
dump exists to be able to write.

**3. The soak.** 200 runs of the differential on the physical oracle with the dump armed: **200
passed, 0 failed, no artifact written.**

So the candidate the oracle change introduced is not the cause, and what was seen once is still
unexplained. What it is not, now, is unrecorded and undiagnosable: the next occurrence leaves a
file. A row missing entirely is a catch-up or a seal, a row with the wrong `region` value is the
widening, and a row visible on one side only is MVCC resolution.

---

# §store unit 4 — a region's columnar copy holds a region, not a table

Opened 2026-09-05 by lane h1-part, from the `mpp` lane's multi-region matrix on the fixed main:
the **columnar** path returns N× answers across regions — a bare `count(*)` was 4× with several
regions — while the row path is right. Each fragment is legitimately routed to its own region and
still reads too much.

## What it is, and it is one line

`crates/esker-store/src/columnar/region.rs`, the walk that builds a copy for a learner placed on a
region that already has data:

```rust
let (start, end) = esker_keys::row::table_row_range(tenant, table_id);
```

**The whole table's key range, on a store that holds several regions of that table.** A store's
`write` column family holds the rows of every region it hosts, so the walk feeds each region's
columnar copy every row of that table *on that store*. With four learners on two stores, two copies
each hold what two regions own, every fragment answers for more rows than its shard covers, and the
SQL node adds the shards up. The row path is unaffected because a row read goes through the region
that owns the key.

Everything around it is already region-scoped and that is what made this hard to see: the slot is
keyed by region id, its directory is `<data-dir>/columnar/<region-id>/`, and the live apply tee only
ever sees its own peer's entries. **Only the initial walk is scoped to the wrong thing**, and only
a multi-region table on a multi-region store can show it — which is a shape that did not exist
before [ADR 0073](../adr/0073-a-regions-size-is-the-data-families-it-spans.md) made a SQL table split
at all.

## Why the epoch guard does not cover it

[ADR 0040](../adr/0040-the-engine-a-query-runs-on.md) already names a neighbouring case — *"a region
that split after its columnar copy was built is not routed… the epoch pins it"* — and closes with
*"a split-aware columnar copy is `esker-store`'s."* That guard is real and it does not apply here:
the splits in this cluster happen **before** the learners are placed, so no shard is ever stale and
no epoch is ever wrong. The copies are built correctly-epoched and simply contain too much.

## The decision: both, and they are not alternatives

**1. Scope the walk to the region's range — the fix.** Intersect the table range with the region's
own `[start_key, end_key)`, read from the region record (`meta::load_regions`, by the slot's
`region_id`) at rebuild time rather than cached in the slot, because a split moves it. This stops
the copy ever being wrong and stops it being N times too large on disk.

**2. Honour a key range at scan time — the guarantee.** `Fragment::validate` today *refuses* any
bounded `KeyRange`, on the stated ground that "a columnar file records no key range". That is true
of the file's *metadata* and not of its contents: the run carries the row key as a column — the
visibility filter already reads it (`runs.visibility.0`) — so a row-level restriction is exactly a
predicate on that column. Implementing it makes the answer correct **whatever a run happens to
contain**, which is what covers the case (1) cannot: a parent's existing runs after a split, which
nothing prunes.

They are not alternatives because they fail differently. (1) alone leaves every copy built before it
wrong, and leaves ADR 0040's split case open. (2) alone leaves each store storing N times the data
it needs and paying to read and discard it on every fragment. Doing (1) without (2) would also mean
the correctness of an answer depends on a *build-time* decision, which is the shape that made this
defect invisible for a whole phase.

### Two things settled while reading, which narrow (2) before it is written

* **The restriction belongs to the store, not to the client.** It is a property of the *region*,
  not of the query, so the store applies it from its own region record and `Fragment.range` — the
  field a client could send — stays refused. That keeps ADR 0040's sentence true of the wire, needs
  no wire change, and makes the guarantee independent of a client sending the right bounds.
* **It cannot be a rewrite of the fragment's filter.** The tempting cheap version is
  `filter AND __key >= start AND __key < end`, and it does not work: `Expr::Column(n)` addresses a
  **projection slot**, not a column of the file, and the query this defect was measured on —
  `count(*)` — projects nothing at all. So the range has to be a `ScanOptions` field honoured where
  the key column is already read, which is beside `Visibility`; both evaluation paths
  (`evaluate_with` and `merged::evaluate`) route through that.

**What is not in this unit:** pruning a parent's runs on a split, and stripe-level range metadata so
a bounded range can skip stripes instead of filtering rows. The first is the storage half of ADR
0040's sentence and wants its own unit; the second is a performance change and this one is a
correctness change. Both get a row when this closes.

## How it will be proved

* `mpp`'s multi-region differential (real binaries, `Engine: columnar` asserted) is the acceptance
  and arrives on its branch — the shape that reproduces N× today.
* In `esker-store`, a unit test that places two regions of one table on **one** store and asserts
  each copy holds only its own region's rows: the assertion is the row count per copy, because that
  is the number that was N times too large.
* In `esker-columnar`, the range predicate asserted directly, including the two boundary readings an
  empty bound has — `start` empty means "from the beginning", `end` empty means "to the end of the
  key space" — which is the rule `clamp_end` in `esker-client` already had to get right twice.
* A counterfactual for each: revert (1) and the store test reddens; revert (2) and the range test
  reddens.
