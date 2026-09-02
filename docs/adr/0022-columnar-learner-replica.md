# 0022 — A columnar learner replica

Status: **accepted, and built through milestone 4.** It was written as *design only* and stayed
that way for three phases; milestones 1 to 3 are `docs/plans/phase-7-columnar.md` and
`phase-8-learner.md`, and milestone 4 is `phase-10-routing.md` with
[ADR 0040](0040-the-engine-a-query-runs-on.md). Milestone 5 — MPP — is still design only and still
last. The amendments below are marked where the built thing differs from what this page assumed.
See `crates/esker-raft/src/config.rs` (learners),
`crates/esker-raft/src/readonly.rs` (`ReadIndex`), `crates/esker-engine/src/fs.rs` (the filesystem
seam), `docs/DESIGN.md` §13, [ADR 0021](0021-time-machine.md) (the catalog-flag pattern this
borrows).

## Context

Every read this system serves walks a row layout: a key range, and a value per key holding every
column. That is the right shape for the workload it was built for — a point read is "the first key
under a prefix" and a range scan is a range scan — and it is the wrong shape for the other one. A
`SELECT count(*), sum(amount) FROM ledger WHERE day > ...` over ten million rows reads every byte of
every column to use two of them, decompresses values nothing looks at, and materialises tuples only
to discard them.

The industrial answer is a second copy of the data in a columnar layout, kept current from the
same replication stream, and a planner that chooses between the two. TiFlash is that answer for
TiDB; this ADR is the same shape for Esker, sized honestly against what this project has actually
built.

## Decision 1: the columnar copy is a Raft **learner**, not a pipeline

A columnar node joins each region's Raft group as a **learner**: it receives the log, applies it,
and votes on nothing. `esker-raft` already has learners — `Config::learners`, `conf.is_learner`,
and the leader's `Progress` carrying the flag (`crates/esker-raft/src/election.rs`) — because
that is how a new replica catches up before it is promoted, and a columnar replica is a learner
that is never promoted.

This is the single most important choice here, and it is a choice about *where consistency comes
from*. The alternatives are a change-data-capture pipeline or a periodic export, and both make the
columnar copy a second system with its own lag, its own failure modes and its own idea of what is
true. A learner has none of those: it is the same log, so a fragment answered from it is answered
from the same history, and "how far behind is it" is a Raft index rather than a queue depth.

What differs from a voting replica is the **apply**, and only the apply. A follower's apply writes
the row into the engine (`crates/esker-store/src/apply.rs`); the learner's apply appends the row's
columns to per-column runs. Everything above and below that — the log, the snapshot protocol,
membership, the applied index reported to PD (`crates/esker-store/src/heartbeat.rs`) — is
unchanged, which is what makes this a new *apply target* rather than a new replication system.

## Decision 2: the planner chooses per query, by estimated cost, with an override

A columnar replica that the planner cannot choose is a data warehouse somebody has to remember to
query. The choice belongs in the planner and it has to be made per query, because the two engines
win at different things by orders of magnitude in both directions.

The rule, in the order it is evaluated:

1. **A point read or a small bounded range stays on a row replica.** Always, no estimate involved.
   Columnar loses badly here — a point read touches one key in a row store and one *stripe header
   per column* in a columnar one — and the shape is exactly detectable: an equality or a bounded
   range on a primary-key or index prefix.
2. **Anything the columnar side cannot answer stays on a row replica.** A transaction that has
   written and then reads its own writes cannot be answered from a learner at all, because the
   learner has not seen an uncommitted write. So: no columnar read inside a transaction that has
   written, no matter what it costs.
3. **Otherwise, estimated cost decides**, and the estimate is bytes read, not rows: `rows scanned ×
   columns projected` against `rows scanned × columns stored`. Columnar wins when the projection is
   narrow and the scan is wide, which is a ratio and not a row count, and stating it as a ratio is
   what stops the threshold from being a magic number somebody tunes once and forgets.
4. **A session variable overrides all of it** — `SET esker.engine = 'row' | 'columnar' | 'auto'`, a
   namespaced custom GUC of the kind ADR 0021 measured a real PostgreSQL to accept. It exists for
   the two cases every such system needs it for: a user who knows better than the estimate, and an
   operator bisecting a wrong answer.

`EXPLAIN` must name the engine it chose. A routing decision nobody can see is one nobody can
debug — and this is the feature most likely to produce "it was fast yesterday".

> **Amended (milestone 4): "overrides all of it" means the estimate, and only the estimate.**
> Rule 4 above is written as though a session could override rules 1 to 3 as well. Read that way
> it lets a user ask for an answer no fragment can produce — a read of a transaction's own
> uncommitted writes, a point read a columnar file cannot restrict to, an aggregate with a
> `DISTINCT` inside it. What the override overrides is **rule 3**: `'row'` forces rows, which is
> always possible, and `'columnar'` skips the estimate and skips nothing else.
> [ADR 0040](0040-the-engine-a-query-runs-on.md) Decision 1 carries the argument, and the measured
> PostgreSQL surface of the variable itself.

## Decision 3: push-down is a plan fragment, on the seam that already exists

Sending ten million rows to the SQL node to count them would waste the entire point. The columnar
node has to evaluate part of the plan and return the reduced result.

The seam for it is not new. `TxnKvReq::Scan { start, end, limit, ts, reverse }` is already a
*server-side* read: the request carries a range, a bound and a snapshot, and the server does work
and returns less than it touched. Push-down is that axis extended — the request carries a
**fragment of the plan** instead of a range, and returns rows the fragment produced instead of rows
it read. The engine below stays byte-opaque either way (`CLAUDE.md` invariant 7): the fragment is
interpreted by the columnar *node*, not by the storage engine.

The first fragment is deliberately small: **scan, filter, project, aggregate.** A serialized plan
subtree — a table id, a key range, a projection column list, a filter expression over those
columns, and a set of aggregates with an optional grouping. That covers the query shape this whole
ADR exists for, and it composes: the SQL node receives partial aggregates from every region and
finishes them, which is a two-level aggregate and not a distributed one.

Two rules keep the fragment honest, and both are lessons this project has already paid for
elsewhere:

* **A fragment the columnar node cannot evaluate is refused, never partially honoured.** An
  expression it does not implement means the fragment comes back unevaluated and the SQL node falls
  back to a row scan. Silently dropping a filter would return extra rows — the same defect class
  `crate::plan`'s "reject, do not ignore" rule exists to make impossible.
* **The fragment is a format with a version byte and a golden**, like everything else on this
  wire (ADR 0002). Two nodes on different versions disagreeing about what a filter means is a wrong
  answer, not a protocol error.

## Decision 4: consistency is `ReadIndex` plus the read timestamp

A learner is behind. Not by much and not unboundedly, but a fragment sent to it must not answer
from a state that is missing a transaction the client has already been told committed.

The rule has two halves and both mechanisms exist:

1. **The fragment carries the reading transaction's `ts`**, and the columnar node applies MVCC
   visibility at it exactly as a row read does — the learner has applied the same commit records,
   so "the newest version with `commit_ts ≤ ts`" means the same thing there.
2. **The fragment carries a minimum apply requirement**, satisfied by a Raft **`ReadIndex`** round
   against the region's leader. `esker-raft` already implements it (`Message::ReadIndex` /
   `ReadIndexResponse`, `crate::readonly::ReadOnly`) because a linearizable read on a follower needs
   the same thing. The learner asks the leader for the current commit index, waits until its own
   apply has reached it, and only then evaluates. If it cannot catch up within a deadline it
   **refuses**, and the planner falls back to a row replica — a slow correct answer beats a fast
   stale one, and the fallback path is one the planner already has for rule 2 above.

The cost is one round trip to the leader per fragment, amortised over a scan of millions of rows,
which is the trade TiFlash makes and it is not close.

> **Amended (milestone 4): the round is unconditional, and it had stopped being so.** Half 2 above
> is written without a condition and `Store::serve_fragment` guarded it with
> `min_apply_index > 0`, which made the field's *default* value the unsafe one. A SQL node is
> exactly the caller that passes zero — it holds a snapshot `ts` and a region id, and a Raft index
> is not a number it can compute — and it does not need one: the round itself covers every commit
> visible at `ts`, because a commit the client was told about was committed on the leader before
> that `ts` was allocated. `min_apply_index` stays as an *additional* floor.
> [ADR 0040](0040-the-engine-a-query-runs-on.md) Decision 6.

## Decision 5: per-table opt-in, through the catalog

Nobody wants a columnar copy of every table, and the tables that want one are known to whoever
designed the schema. So it is a **per-table flag in the catalog**, following exactly the pattern
ADR 0021 established for the retention override and for the same reason: a record of its own under
its own kind byte, readable by a layer that does not link `esker-sql`.

```text
'm' ++ "sql" ++ 'c' ++ tenant:u64 ++ id:u64   one table's columnar replication setting
```

> **Amended (phase 8): the kind byte is `'l'`, not `'c'`.** By the time this was built `'c'` was
> the *checkpoint's* — phase 6d took it, after this ADR was written and without knowing this
> sketch existed. Two kinds sharing a byte is not a cosmetic clash: the layout's whole point is
> that "one scan of the kind byte visits every setting", and a scan of `'c'` would have returned
> checkpoints interleaved with columnar settings, to a placement driver that cannot tell them
> apart because it does not link `esker-sql`.
>
> `'l'` is for the **learner** Decision 1 calls a columnar replica. `crates/esker-sql/src/catalog`
> pins the layout with a golden that also asserts a checkpoint key falls *outside* a scan of the
> columnar range — the collision itself is tested, rather than being avoided and then trusted.

> **Amended (phase 8): PostgreSQL refuses this DDL, and no spelling of it would not be refused.**
> The paragraph below says `ALTER TABLE ... SET (columnar_replicas = 1)` is "the storage-parameter
> spelling PostgreSQL already parses". Measured against 19beta1 rather than assumed, it is not:
> an unqualified custom parameter is `22023 unrecognized parameter "columnar_replicas"`, and an
> arbitrary namespace is `22023 unrecognized parameter namespace "esker"` — only `toast` is known.
> So there is no compatible spelling to choose, and accepting this one is a **deliberate
> divergence** rather than a compatibility win. It is in the register at
> `docs/plans/phase-6a.md` §10a with the others, and the capture is
> `crates/esker-sql/tests/corpus/pg19_storage_parameters.txt`.
>
> The capture also found the asymmetry that reading would have missed: `RESET` of a parameter that
> has never existed anywhere is **accepted**, while `SET` of the same name errors. `RESET` does not
> validate names at all.

Value: a version byte and the number of columnar learners wanted (`0` meaning none, which is also
what an absent record means). Setting it does not bump the catalog version — it changes nothing
about how a row is written or read — for the same reason retention does not. The DDL surface is
`ALTER TABLE ... SET (columnar_replicas = 1)`, which is the storage-parameter spelling PostgreSQL
already parses and this node already answers `0A000` for.

PD is what acts on it: adding a learner is a placement decision, and PD already schedules exactly
that operator (`crates/esker-pd/src/operator.rs`).

## The honest cost, ranked

This is the section to read twice, because the number everybody asks about first is the one that
matters least.

### Third: disk, and it is small

Columnar layouts compress far better than row layouts because a column is a run of one type: run
length and dictionary encoding for low-cardinality strings, frame-of-reference and delta for
integers and timestamps, and only then a general-purpose codec. The published figures are 3–10×,
and they are usually quoted against an *uncompressed* baseline. Our SSTs are already LZ4-compressed
per block (`esker_engine::Compression`), so the honest assumption is the low end of that range
against what we actually store — call it **3×**.

The arithmetic, then, with `R` the size of one row replica:

```text
today                 3R                      (three voting replicas)
one columnar learner  3R + R/3   = 3.33R      +11%
two, for availability 3R + 2R/3  = 3.67R      +22%
at 10x compression    3R + R/10  = 3.10R       +3%
```

**One columnar learner costs single-digit to low-double-digit percent of total disk**, and it is
per table rather than cluster-wide, so the real figure is that fraction of the tables that opted
in. Disk is the cheapest thing in the system and this is the cheapest part of this feature. Anybody
who rejects columnar replication on storage grounds has been given the wrong number.

### Second: write amplification

Every write is applied **twice** — once by the row replicas, once by the columnar learner — and the
second apply is not free. The Raft *log* is shared, so this is not a second consensus round; but it
is a third replication target for the leader to feed (leader egress per region goes from two
followers to three, `+50%`), a whole second apply loop, and a second engine's compaction running
against the same disks and the same page cache.

For a write-heavy table this is the cost that shows up in tail latency, and it shows up on the
*row* side, which is the surprising part: the columnar learner's compaction competes with the row
engine's for I/O on nodes that also serve foreground reads. Placing columnar learners on their own
nodes is the mitigation, and it is a real operational requirement rather than an optimisation.

### First: a second engine is an `esker-engine`-scale build, and then you operate two

The columnar engine is not a module. It is a file format with a footer, a version and golden tests;
a set of per-type encodings each with a round-trip proptest; a stripe/row-group layout with
statistics for predicate pruning; a compaction strategy of its own, because appending an apply
stream column-wise produces small runs exactly as a memtable flush does; a crash test proving that
`kill -9` mid-write loses nothing acknowledged; and a fuzz test proving that no arbitrary byte
sequence panics a decoder (`CLAUDE.md` invariants 2 and 9). That list is `esker-engine`'s own
deliverable list with the words changed.

And then there are two engines to operate, which is the cost that never appears in a design
document. Two compaction schedulers to tune. Two sets of metrics that have to be read together to
diagnose anything. And a new class of bug that does not exist today: **the two engines disagreeing**
— a query answered one way from the row store and another way from the columnar one. That is the
worst failure this feature can have, because it is silent, and the only defence is a differential
test that runs every query both ways and compares, which has to be built *before* the routing rule
and not after.

## The phase 6b synergy, which changes the shape of this

Phase 6b tiers cold SSTs to object storage, and `esker_engine::FileSystem` is the trait that was
built for it — the engine reads and writes through it and does not know whether a file is local.
That opens a second, cheaper form of this feature, and it is worth designing towards rather than
discovering later:

**When a cold SST tiers to S3, rewrite it columnar.** The hot tail stays row-local where point
reads and writes need it; the cold body — which is where a large analytical scan spends all of its
time anyway — lives in object storage in the layout that scan wants, compressed 3–10× against what
it would otherwise have cost to store there. Analytics reads cheap bytes from cheap storage, and
nobody pays for a third replica of anything.

That is a lakehouse, arrived at from the storage side rather than the query side, and it changes
the ranking above completely:

* **write amplification disappears.** There is no second apply — the rewrite happens during a
  tiering pass that was going to rewrite the file anyway.
* **disk gets cheaper rather than more expensive**, because the columnar rewrite *replaces* the
  row-format cold data rather than duplicating it.
* what remains is the file format and the scan path, which is the irreducible part of the work and
  the part that is shared with the learner design either way.

What the tiered form cannot do is answer a query over *recent* data columnar, because recent data
is the row-format hot tail. So the two are complements: the learner is for tables whose whole
history is analysed continuously, and tiered columnar is for tables whose analysis is historical —
which is most of them. **The file format and the fragment protocol are the same in both**, which is
the argument for building those first and choosing the delivery mechanism afterwards.

## Milestones, in order, with MPP last

1. **A single-node columnar file format.** Writer, reader, per-type encodings, statistics,
   goldens, proptests, a crash test and a decoder fuzz. No cluster, no Raft, no SQL. This is the
   long pole and it is worth its own phase.
2. **A scan path and the fragment protocol**, evaluated locally against that format, with the
   differential test against the row engine standing up from the first query.
3. **Either delivery mechanism**: the learner feed (apply stream → columnar) *or* the 6b tiering
   rewrite. The tiered form is cheaper and lands with 6b; the learner form is what a
   continuously-analysed table needs. Whichever comes first, the second is smaller for having the
   first.
4. **Planner routing**, with `EXPLAIN` naming the engine and the session override from day one.
   **Done** — `docs/plans/phase-10-routing.md`, [ADR 0040](0040-the-engine-a-query-runs-on.md).
5. **MPP exchange — last, and only if measured.** Shuffling intermediate results *between*
   columnar nodes so that a join or a high-cardinality `GROUP BY` runs distributed rather than
   finishing on one SQL node. It is the largest piece of work in this ADR, it needs a shuffle
   protocol, spill-to-disk and its own failure handling, and two-level aggregation without it
   already covers the queries that motivated the feature. Building it before the numbers say it is
   the bottleneck would be optimising before profiling, which `CLAUDE.md` forbids for smaller
   reasons than this.

## What each crate has to add

* **A new crate** — the columnar engine. It is not a module of `esker-engine`; its file format,
  compaction and read path share nothing but the `FileSystem` trait and the primitives in
  `esker-base`.
* **`esker-store`** — a second apply target for a region a node holds as a columnar learner, and the
  fragment service in front of it.
* **`esker-proto`** — the fragment request and response, versioned and golden-tested like every
  other message.
* **`esker-sql`** — the cost rule, the engine choice, `EXPLAIN` naming it, the session override, the
  catalog flag and its DDL, and the two-level aggregate that finishes what fragments return.
* **`esker-pd`** — placing columnar learners, which is an operator it already knows how to schedule.
* **`esker-raft`** — nothing. Learners and `ReadIndex` are both already there, which is the reason
  Decision 1 is the shape it is.
