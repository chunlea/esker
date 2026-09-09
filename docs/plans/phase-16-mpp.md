# Phase 16 — MPP exchange: the design, and the measurement that decides whether to build it

[ADR 0022](../adr/0022-columnar-learner-replica.md) **milestone 5**, the last one, and the only
one the ADR gates on a number:

> **MPP exchange — last, and only if measured.** Shuffling intermediate results *between* columnar
> nodes so that a join or a high-cardinality `GROUP BY` runs distributed rather than finishing on
> one SQL node. It is the largest piece of work in this ADR, it needs a shuffle protocol,
> spill-to-disk and its own failure handling, and two-level aggregation without it already covers
> the queries that motivated the feature. Building it before the numbers say it is the bottleneck
> would be optimising before profiling, which `CLAUDE.md` forbids for smaller reasons than this.

So this file is in two halves and they are not equal. The **design** below is what milestone 5
would be if it is built, worked out far enough that the verdict is a decision about a known thing
rather than about a guess. The **verdict** is what decides whether any of it is built, and it is
written from `docs/bench/mpp-baseline.md` and from nothing else.

> **Status: measured, and the verdict is `not yet` — see §10.** The exchange is not built, and the
> reason is not a close call: a SQL table never occupies more than one region today, so there is
> never more than one fragment to shuffle between.

## 1. What is already true, and is easy to misread

Four of ADR 0022's five milestones are built, and three facts about *what they built* decide most
of what follows.

**A fragment is one region's whole answer, in one framed message.** The request carries a plan
subtree and the response carries groups (`docs/DESIGN.md` §16.2). There is no streamed fragment
response, and [ADR 0040](../adr/0040-the-engine-a-query-runs-on.md) Decision 4 turns that into a
routing rule: a bare projection scan is **not** routed, because a rows-output fragment would
materialise a whole region on the SQL node where the row path streams a page at a time.

**The SQL node asks its fragments one at a time.** `crates/esker-sql/src/exec/fragment.rs`:

```rust
for shard in &shards {
    let Ok(answer) = source.evaluate(shard, &bytes, ts, 0) else { … };
```

and `FragmentSource::evaluate` takes a single shard, so the serialism is in the trait's shape and
not only in the loop. **R regions cost R round trips end to end however many stores answer them.**
This is not MPP's problem and MPP does not fix it; it is named here because it is the first thing
the measurement will see and because it changes what §10 can conclude — see §10's second question.

**Exactly one plan shape is substituted**, `Aggregate { [Filter] { SeqScan } }` (ADR 0040 Decision
4). A join never reaches the columnar path at all today. **A shuffle is therefore not what a join
needs first**; a join fragment is, and that is a larger and separate piece of work than the
exchange — see §8.

> A small thing found while asserting the engine on every benchmark run, for whoever owns ADR
> 0040. `crates/esker-sql/src/exec/mod.rs` calls `fragment::route` only when the select has no
> joins and no derived table, so a joined plan never reaches the router and `EXPLAIN` prints **no
> `Engine:` line at all** — while a `SELECT *` over the same table prints `Engine: rows` with
> `Reason::Shape`. That is a **third** deliberate silence, and Decision 3 lists two. It is
> defensible on Decision 3's own rule — there was no choice to describe — but it is not what the
> ADR says, and a reader debugging "why is my join not on the columns" gets nothing. Recorded, not
> acted on: `esker-sql` is not this lane's.

## 2. What an exchange is, in this system's own terms

The two-level aggregate finishes like this today:

```
region 1 learner ──partials──┐
region 2 learner ──partials──┼──▶ SQL node: merge every partial into one BTreeMap, finish
region 3 learner ──partials──┘
```

The SQL node receives `regions × groups` partials and merges them into `groups`. With an exchange
it finishes like this:

```
region 1 learner ──┐          ┌──▶ node A: merges the partials for its share of the keys ──┐
region 2 learner ──┼─shuffle──┼──▶ node B: merges the partials for its share of the keys ──┼──▶ SQL node
region 3 learner ──┘          └──▶ node C: merges the partials for its share of the keys ──┘
```

Two things change and only two. The merge is **parallel** — each consumer sees `regions ×
groups/consumers` partials instead of all of them. And the SQL node stops being on the path for
intermediate results: it receives `groups` rows, once, instead of `regions × groups` partials.

What does **not** change: the bytes leaving the stores, and the scan. An exchange moves the same
partials the stores already produce; it moves them somewhere else. That is why §10's question is
about the *share* the finish takes and not about the total.

## 3. The shuffle protocol — a wire change, and its own ADR

New messages on the hand-rolled wire (`docs/DESIGN.md` §9), so this needs an ADR of its own before
a byte is written. **The number is claimed when unit 2 starts, not now**: main's highest is 0072
and lands several ADRs a day, and holding a number across a design that may never be built is how
two lanes end up with one number.

### 3.1 The thing being addressed is not a region

This is the crux, and everything awkward in the design comes from it. Every message on this wire
today is *about a region*: `{ region_id, epoch, peer }` is the header, and `esker-client`'s router
resolves a region to a peer. A shuffle message is about **an instance of a plan fragment running
on a node** — `(query, fragment, partition)` — which no existing header can name.

So a new service namespace, alongside the five in DESIGN §9:

```
0x06 Exchange   0x0601 Open   0x0602 Push   0x0603 Finish   0x0604 Cancel
```

and a new identity that is not a region:

```
exchange_id := coordinator_id:u64 ++ query_seq:u64      unique in the cluster, allocated by the
                                                        SQL node from PD's id allocator
stream_id   := exchange_id ++ producer:u32 ++ partition:u32
```

`coordinator_id` from `Pd::AllocId`, which already exists and already guarantees cluster-unique
ids — the same source a split uses.

### 3.2 A partition is a stream, not a message

`max_frame_size` is 16 MiB and a partition can be larger, so a partition **must** be streamed. The
wire already has the frame kinds: `Stream` and `StreamEnd`, used today by exactly one method,
`RaftTransport::Snapshot` (DESIGN §9). An exchange partition is the second, and it is the same
shape — a run of `Stream` frames ended by a `StreamEnd`.

**This is a prerequisite, and it is shared work.** ADR 0040 Decision 4 keeps a bare projection scan
off the columnar path *because* a fragment answers in one message, and says lifting it "needs a
paged or streamed fragment response, which is a wire change". That wire change is this one. So
milestone 5's first unit pays for a restriction milestone 4 recorded, whether or not the exchange
itself is built — which is worth knowing before §10 decides.

### 3.3 The body

```
open   := version:u8 ++ exchange_id ++ fragment ++ producers ++ consumers ++ partitioning ++ crc32c
push   := version:u8 ++ stream_id ++ sequence:varint ++ payload ++ crc32c
finish := version:u8 ++ stream_id ++ rows:varint ++ messages:varint ++ crc32c
```

Versioned and golden-tested like every other message (ADR 0002), and every byte checksummed
(invariant 2). `payload` is `esker_proto::fragment::result`'s existing body — the format the
partials already cross in — so the exchange carries what a fragment already produces rather than
inventing a second encoding of it. `esker-proto` still does not link `esker-columnar`.

**`finish` carries the counts, and that is the whole of the completeness argument.** A consumer
that has not received a `Finish` from every declared producer, whose `rows` and `messages` match
what it actually received, **must not produce an answer**. A short partition is otherwise
indistinguishable from a complete one, and a partial answer that looks complete is the one failure
this feature must not have — the same sentence `esker-sql`'s shard list already turns on.

### 3.4 The epoch rule, which invariant 5 forces and which is not obvious

> **Every request carries a region epoch. A stale epoch is rejected with a redirect hint; a store
> never serves a request for a range it no longer owns.** — `CLAUDE.md` invariant 5

A `Push` is not about a range, so "the range it no longer owns" needs a reading. The one this plan
takes:

* **`Open` declares the producer set as `(region_id, epoch)` pairs** — the epochs the *planner*
  saw, exactly as `Columnar::shards` carries them today.
* **Each producer checks its own pair before it produces anything**, and refuses a stale one
  exactly as `Store::serve_fragment` refuses a stale shard today. No new refusal semantics.
* **A consumer accepts a `Push` only for a `stream_id` whose producer is in the `Open` it was
  given.** An unknown stream is an error, never a buffered "maybe it is coming".
* **A stale epoch anywhere aborts the whole exchange. It is never retried into.** This is ADR
  0040's existing consequence, restated for many producers: *"re-routing after a split asks about
  a different set of rows and returns a partial answer that looks complete"*. With N producers the
  same hazard is N times more likely and exactly as silent.

An abort is cheap because the fallback is free: the routed plan already carries the row plan it
falls back to, at the same `ts`, and the client sees the answer the snapshot always had.

### 3.5 The partitioning function, and the trap in it

Consumers are chosen by hashing the grouping key. The constraint is not "spread evenly" — it is:

> **Two keys that are `pg_cmp`-equal must land on the same consumer.**

`GROUP BY` here groups by `pg_cmp` equality everywhere (`exec/fragment.rs`'s `GroupKey`, and
DESIGN §16.2), so `-0.0` and `0.0` are one group and two `NaN`s are one group. **Hashing the
encoded bytes would split a group the row engine keeps together** — a wrong answer, silently, and
the exact defect class ADR 0040 Decision 5 exists to prevent on the comparison side.

So: hash a **canonical form** of the datum, not its wire bytes, with one canonicaliser shared by
every node, tested against `pg_cmp` as a property — *for all pairs, `pg_cmp(a, b) == Equal`
implies `hash(a) == hash(b)`*. `esker-base`'s hash is the function; the canonicaliser is new and
belongs beside `pg_cmp`.

## 4. Where the exchange operator lives

Three placements, and the choice decides whether this is MPP or a rearrangement.

| | Where the shuffle runs | What the SQL node does | Verdict |
|---|---|---|---|
| **A** | The SQL-node executor | receives every partial, redistributes | **No.** It is today's bottleneck with two extra copies through the node that was already the bottleneck. |
| **B** | A store-side fragment executor on each columnar learner | plans, allocates the exchange id, declares the sets, receives the final gather | **Yes.** |
| **C** | Store-side first level, SQL-node last level | as B, plus a final merge | This is B. C is B's gather described twice. |

**B**, then: the SQL node is the **coordinator and not a data path**. It sends one `Open` to each
participant, and receives `groups` rows once. Intermediate results never touch it.

The consumers are the columnar learners themselves — the nodes that already hold the data and
already have a fragment executor. A separate pool of exchange nodes would be a second thing to
place, size and fail over, and PD has no operator for it.

**How a fragment names its consumers**: it does not — `Open` does. The consumer set is
`(store_id, address, partition_range)`, resolved by the SQL node from the same region cache that
already tells it which peer of a region is the columnar learner (`esker-client`'s
`PeerRole::ColumnarLearner`). A producer is told where to push; it never resolves anything itself,
because a producer that resolved its own consumers could disagree with the coordinator about the
consumer set, and two producers with different consumer sets is a lost partition.

## 5. Spill to disk

A consumer merging `regions × groups/consumers` partials can exceed memory, and the answer must
not be "the node dies". Spill is the escape, and three invariants shape it before any performance
question does.

* **Above `esker-engine`** (invariant 7). The engine is byte-opaque and knows nothing of plans;
  a spill file holds partials, which are key semantics. It goes in the columnar/exchange crate,
  through `esker_engine::FileSystem` — the same seam a columnar file and a tiered SST use — and
  the engine's own code is untouched.
* **Every byte checksummed, magic and version header** (invariant 2). The format is the one this
  project already writes four times over:

  ```
  spill  := header ++ block* ++ trailer
  header := magic "ESKERSPL" ++ version:u32
  block  := length:u32 ++ payload ++ crc32c(length ++ payload)
  trailer:= blocks:u32 ++ rows:u64 ++ crc32c(itself) ++ magic
  ```

  **The trailer's magic is the commit point**, as in a columnar file (DESIGN §16.1): a file a
  crash left half-written lacks it and is *unsealed* — deleted — rather than corrupt, which is a
  different operational answer.
* **Immutable, and renamed into place** (invariant 3). Written to a temporary, fsynced, renamed.
  Never appended to after sealing.

Spill is **bounded and accounted**: a per-exchange byte budget, and an exchange that would exceed
it aborts to the row plan rather than filling a disk. A shuffle that turns a memory problem into a
disk problem has moved the failure, not removed it.

Lifetime: one directory per `exchange_id`, deleted when the exchange ends however it ends, and
swept at store startup — an orphan directory from a `kill -9` is a leak that grows.

## 6. Failure handling — what is retried, what is aborted, what is never double-counted

The governing property, inherited from milestone 4 and not weakened here:

> **An exchange is all-or-nothing at one `ts`, and its failure is the row plan that was already
> there.**

| What happens | What the exchange does | Why |
|---|---|---|
| a producer dies mid-shuffle | **abort**, fall back to rows at the same `ts` | its partitions are incomplete and a short partition looks like a complete one |
| a consumer dies mid-shuffle | **abort** | its share of the keys has no other home; re-partitioning mid-flight would need every producer to replay |
| a region splits under a producer | **abort**, never re-route | ADR 0040's rule: the halves answer about a different set of rows |
| a leader moves | **retry the `ReadIndex` round only** | the round is the leader's business; the fragment is a learner's, and the learner did not move |
| a `Push` is duplicated | **discard by `(stream_id, sequence)`** | the wire redelivers; a partial that was added twice is a wrong number with no symptom |
| a `Push` is lost | caught at `Finish` by the count, then **abort** | this is what the counts are for |
| the whole exchange times out | **abort** | a deadline that falls back beats one that hangs, which is Decision 4's own trade |
| the coordinator re-asks after any abort | **safe** | a read at a fixed `ts` is repeatable; `esker_client::retry::may_ask_again` already draws this line by method |

**Never double-counted** has exactly two mechanisms and they are both above: dedupe by
`(stream_id, sequence)` on the way in, and a per-stream row count checked at `Finish`. Anything
that gets past both is a bug the simulator's checker (§7) is written to find.

## 7. The simulator, and the checker that would catch a lost partition

`esker-sim` drives this deterministically or it is not testable: an exchange has N producers, M
consumers and a network, and the interesting states are the ones a real cluster reaches once a
week. What makes it drivable is the property `CLAUDE.md` invariant 4 already buys for Raft — the
exchange operator gets the same shape: **no threads, no timers, no sockets**, time in through
`tick()`, messages in through `step()`, effects out through a `Ready`-like batch.

Driven by `esker-sim`'s seeded network with reordering, duplication, delay and drop, and with
producers and consumers killed at chosen steps.

**The checker is conservation, and it has three clauses:**

1. **Every input row is in exactly one partition.** Sum the per-partition input counts; it equals
   the rows the producers scanned. Not "at least" and not "about".
2. **The answer equals the single-node answer.** The same fragments, merged by today's two-level
   finish on one node, at the same `ts`, compared as a `Result` so an error is an answer too —
   which is exactly the shape milestone 2's differential already has (DESIGN §16.3), and it is
   reused rather than re-invented.
3. **A lost partition is an error, never a short answer.** The checker injects the loss and
   asserts the exchange *fails*. A checker that only compares answers passes a system that loses
   a partition and reports fewer groups, because it has nothing to compare the missing groups to.

Clause 3 is the one that pays for the counts in §3.3, and it must be written **before** the
protocol, red, so that the protocol is what makes it green.

## 8. What this phase will NOT do

* **No join push-down.** A shuffle for a join needs a join fragment first, and there is none: ADR
  0040 Decision 4 substitutes one plan shape and a join is not it. A distributed join is
  *build-side broadcast or shuffle-both-sides* on top of a fragment that can express a join at
  all, and that fragment is a bigger piece of work than this whole phase. If the numbers say joins
  are what hurts, the next phase is the join fragment and **not** this one.
* **No cost model.** Nothing here estimates whether an exchange is worth it. The trigger is a
  session variable and a threshold on the planner's existing group estimate, or it is nothing.
* **No repartitioning mid-flight**, no adaptive consumer counts, no skew handling. A skewed key
  puts one consumer under all the load, and this phase records that rather than fixing it.
* **No exchange for anything but the two-level aggregate's second level.** `ORDER BY`, `DISTINCT`
  and window functions all want a shuffle eventually and none of them get one here.
* **No new dependency.** Pure Rust, allowlist only, and nothing on this page needs anything that
  is not already in the workspace.
* **No change to `esker-raft` or `esker-engine`.** Same as ADR 0022's own list: the exchange is a
  plan-layer thing on a wire, and neither of those crates learns what a partition is.

## 9. The measurement

`docs/bench/mpp-baseline.md` §7 has the numbers and the exact commands. In one screen: 200,000
rows, 20,000 groups, one region, three repeats interleaved, on a quiet machine (load 5.94 falling
to 1.59) inside a window the coordinator held other lanes out of.

```text
columnar, 1 group        19 ms      6.4 KiB shipped
columnar, 32 groups      30 ms      8.0 KiB
columnar, 20,000 groups  55 ms   1013.2 KiB
rows, any of the three  6.6 s      20.6 MiB
```

Holding rows and columns constant and varying only the group count: **the whole
cardinality-dependent cost is 36 ms**, of which the SQL node's own CPU is at most 10 ms — the
clock tick, so the instrument cannot resolve it further. Shipping is about **50 bytes a group**.
The control does not move (0.019 s, 0.018 s in the repeat), which is what says the machine was not
what was measured.

### 9a. What the measurement cannot see, and what milestone 5 owes it

The instrument this ADR's own gate needs **does not exist**, in any crate, for anybody:

| what is missing | where it would go | what it would expose |
|---|---|---|
| any clock at all on a routed plan | `esker_proto::fragment::ScanStats` (`crates/esker-proto/src/fragment/mod.rs:133`) | the learner's own evaluate time, so a reader can tell a slow scan from a slow network |
| bytes on a fragment response | the same, or `esker_client::fragment::FragmentAnswer` | `result.len()` is known at `exec/fragment.rs` and thrown away |
| an execution time on `EXPLAIN ANALYZE` | `esker_sql::exec::explain` | PostgreSQL prints `Execution Time:`; this node prints none, for any plan, routed or not |
| a store address distinct from its bind address | `esker-cli`'s `--listen` (`server.rs:279` registers the listen string verbatim) | a proxy between a SQL node and a store, which is how bytes and per-answer timing would be observed without touching either binary |

`esker bench-mpp` works around all four by reading each process's `utime + stime`, `rchar` and
`VmHWM` from `/proc` — which is honest and is *not* a substitute: it says what a process spent,
not what a fragment cost. **If milestone 5 is built, the first three rows are part of it**, because
an exchange nobody can measure is an exchange nobody can tune, and the argument for building it
would be an argument for instrumenting it too.

## 10. The verdict

> **Amended 2026-09-05: the first of this verdict's two gates has been lifted.** The `split` lane
> fixed what §8 found — `esker_store::split::approximate_size` measured the RawKV namespace while
> SQL rows live under `'x'`, so a SQL table never split at any threshold. It measures the SQL
> keyspace now ([ADR 0073](../adr/0073-a-regions-size-is-the-data-families-it-spans.md),
> `crates/esker-store/src/keyspace.rs`), and a SQL table occupies more than one region. **ADR 0022
> milestone 5's structural blocker is therefore gone**: a query can now have more than one
> fragment, so an exchange would have something to shuffle between.
>
> The verdict below is **not** thereby reversed, and the reason is its second half, which never
> depended on splitting: at 200,000 rows and 20,000 groups the whole cardinality-dependent cost on
> the SQL node was 36 ms of a 55 ms query. What changes is that the question is now *measurable* —
> the fragment-count axis §9 could not produce is reachable — and §10's "what would change this
> verdict" is now a list of two rather than three. **Re-measuring on the multi-region cluster is
> the next thing this file owes**, and until that is done the numbers below are single-region and
> say so.
>
> **Amended 2026-09-08.** The *row* path across a multi-region table is now proved end to end on a
> real cluster (`crates/esker-sql/tests/multi_region_rows.rs`, and
> `docs/plans/split-region.md` §11 for what it does and does not cover): scans, point reads, ranges,
> a secondary index, aggregates, a cross-region transaction, and the client's cache refresh on
> `EpochNotMatch`. **This changes nothing about the verdict below**, which is about the columnar
> arm and its economics — item 3's `regions × groups` is still four orders of magnitude away, and
> item 4's parallel dispatch is still unwritten. What it does change is that the row half of the
> cluster this file would re-measure on is now known to be correct rather than assumed: a
> re-measure that produced a wrong answer can no longer be blamed on the row path. It also found
> and fixed a client defect that would have made any such re-measure impossible — a transaction
> across a boundary could not commit at all.

**Not yet — and not because the finish is cheap, though it is. Because there is nothing to shuffle.**

MPP exchange moves intermediate results *between* columnar nodes, and a fragment is one per region
(ADR 0040 Decision 4). A SQL table today always occupies exactly **one** region, whatever its size,
because `esker_store::split::approximate_size` measures the RawKV namespace — `['r'…, 's')` in the
default column family — while SQL rows live under `'x'` in the Percolator column families. A region
holding 200,000 rows reports `~0 bytes` to PD and never crosses a split threshold at any setting:
this measurement asked for four regions with a 4 MiB threshold over ~20 MB of table and got one,
identical to the run that asked for one with a 512 MiB threshold. So a SQL query has one fragment,
an exchange has one producer and one consumer, and there is no shuffle to build. **Milestone 5 is
unreachable from SQL until a SQL table can occupy more than one region**, and that is `esker-store`'s
work, not this phase's.

Second, and it would be the answer even if regions split tomorrow: **the finish is not where the
time goes.** At 200,000 rows and 20,000 groups the entire cardinality-dependent cost on the SQL
node is 36 ms out of a 55 ms query, and the node's own CPU inside it is at most 10 ms. An exchange
removes some fraction of that 36 ms and none of the other 19. The same query on the row engine
takes 6.6 seconds — so the thing that made this workload 350× faster was the columnar path
milestones 1 to 4 already built, and the finish is a rounding error beside it.

Third, **the join is a different question with a different answer, and it is the one worth asking
next.** It does not reach the columnar path at all, so it has no fragment to be pushed into and no
shuffle to attach one to; at 40,000 rows it costs 8 seconds against the aggregates' 55
milliseconds, and `esker.engine` cannot move it. If analytical workloads on this system hurt, the
join is where — and the next phase is a **join fragment**, not an exchange. §8's first bullet
already says this; the numbers now say it too.

### What would change this verdict

Named so the next person can check them rather than re-derive them:

1. ~~**A SQL table that splits.**~~ **Done, 2026-09-05, by the `split` lane** (ADR 0073). The size
   estimate and the boundary scan see the SQL keyspace now. This was called "the cheapest of the
   three" and it was; what it unblocks is the *measurement*, not the exchange.
2. **Fan-out is bounded by distinct stores, not by regions.** Measured on the first multi-region
   cluster anyone ran: five regions, five columnar learners, on **two** distinct stores. PD places
   a learner on the healthiest store *without a peer of that region*
   (`esker_pd::schedule`), and with three voters and four stores few stores are free for any given
   region, so learners cluster. An exchange's parallelism is the number of **nodes** holding the
   fragments; a verdict reasoning from region count would have overestimated it by more than twice
   on that cluster. `esker bench-mpp --diagnose` reports both numbers, and any re-measure must read
   the second (`docs/bench/mpp-baseline.md` §9d).
3. **A finishing cost that grows past the scan.** The shape to watch is `regions × groups`: at 50
   bytes and, say, 40 µs a group (36 ms over ~900 groups' worth of measurable difference — an upper
   bound, since the tick hides the rest), a hundred regions each holding 100,000 groups would ship
   500 MB into one node and merge ten million partials. That is where an exchange earns its keep,
   and it is four orders of magnitude from anything this system can currently produce.
4. **Parallel dispatch first, now that regions do split.** Fragments are asked one at a time
   (§1), so R regions cost R round trips before any merging happens. That is `esker-sql` and
   `esker-client`, no wire change, no shuffle, no spill — and on today's numbers a 25 ms round trip
   per extra region would dominate the 36 ms the exchange is aimed at. **Measure again after
   parallel dispatch, not before.**

### Agreement is not correctness

The sentence this whole thread reduces to, kept here because it was learned three times in one
night and each time it cost a wrong conclusion.

Two engines returning the same rows is not evidence that either is right. The defect that started
this returned **four times** the correct count with both arms internally consistent — and twice
before that, a matrix headed `columnar` reported "0 disagreements" while both arms were in fact the
row engine: once because no learner had been placed so every fragment refused, and once because the
interim guard had deliberately put them there. A comparison is worth exactly what its denominator
is worth.

So the acceptance test computes its expected values **from the fixture** — two hundred rows,
`bucket = id % 4`, `amount = id`, so every count and sum is known without asking anything — and
asserts the engine that answered **before** it compares. Nothing is asked of an engine to validate
an engine. `routing_differential` had the same rule first, in the other direction, for the same
reason: *"a query that fell back agrees with the row engine for free"*.

### The guard, retired by the flip — and what re-arms it

A multi-region columnar query returned **N times** the right answer (measured 2026-09-05,
`docs/bench/mpp-baseline.md` §10). The rule that held it back is keyed on what the source declares:
`FragmentSource::runs_are_region_scoped`, **required with no default**, because a defaulted method
is a silent opt-out and a source that quietly answered "yes" returns a wrong number rather than a
slow one.

**It is now `true`.** The store-side fix took three attempts and the last one is the interesting
one: building the copy from the region's range alone gave 357 rows for 200, applying the scan range
alone gave 52, and the reason both halves were needed *and* still wrong was that **a region bound
was raw where a run's key is memcomparable-encoded** — so the range compared two different
alphabets. Encoding the bound makes it exact.

**The guard code, the trait method and the `EXPLAIN` reason all stay.** What retired the guard is
one `bool`, and what re-arms it is the same `bool`: any future store whose columnar runs are not
scoped to the region a fragment asks about answers `false` and is kept on the rows, saying so in
`EXPLAIN`. That is the point of putting the rule in a declaration rather than in a version check.

Two tests hold the seam and they sit at different levels on purpose:

* `esker-sql/tests/routing.rs::a_source_that_is_not_region_scoped_keeps_a_split_table_on_the_rows`
  — a scripted source declaring `false` makes the guard fire and `EXPLAIN` name it. **In process,
  and the right home**: it tests the planner's rule, which is what the rule is.
* `esker-cli/tests/multi_region_differential.rs` — the acceptance. Real binaries, a split table,
  a learner on every region, `Engine: columnar` asserted before every comparison, and the answers
  checked against values the fixture makes true rather than against the other engine.

The real-cluster guard test that stood between them is **retired**: it asserted the guard fires on
a real cluster, and it cannot any more — it was red because the fix works, which is the one reason
a test should be removed rather than repaired.

### What this measurement cannot say### What this measurement cannot say

It has **one fragment**, so it says nothing about how the finish scales with fragment count — the
axis the exchange is actually about. That axis was the run's purpose and §8 is why it does not
exist. Everything above about multi-region behaviour is arithmetic on a single-region measurement,
and is labelled as such wherever it appears.


---

# The join fragment — getting a join onto the columnar path

§10 named this the next question worth asking, and the measurement is why: a join does not reach
the columnar path *at all*, so at 40,000 rows it costs 8 s where the aggregates cost 55 ms, and
`SET esker.engine = 'columnar'` cannot move it. This section is that unit's plan. It is deliberately
**not** the exchange: one region, one fragment, no shuffle, no spill — multi-region join
parallelism is gated on the `split` lane's work and is named as such in §J7.

## J1. Why a join is not routed today, in one line of code

`crates/esker-sql/src/exec/mod.rs` guards the call:

```rust
if let Some(table) = table.as_deref()
    && inners.is_empty()          // <- no joins
    && !derived_from
{ fragment::route(…) }
```

so a joined plan never reaches the router, `planned.engine` stays `None`, and `EXPLAIN` prints no
`Engine:` line at all — the **third** deliberate silence where
[ADR 0040](../adr/0040-the-engine-a-query-runs-on.md) Decision 3 lists two. Closing that silence is
half of this unit and is not optional: a reader debugging *"why is my join not on the columns"*
gets nothing back today.

## J2. Where the join runs — three options, and the one that needs no wire change

| | Shape | Verdict |
|---|---|---|
| **A** | A **columnar join fragment**: both tables in one fragment, joined on the learner | **No, not yet.** A fragment is evaluated against *one region's* columnar files (DESIGN §16.2), and two tables occupy two key ranges and therefore two regions, on stores nothing co-locates. It needs either co-location PD does not schedule or a shuffle, which is the exchange this file has already deferred. |
| **B** | **Two scan fragments, joined on the SQL node** | **No, not yet.** It needs a *rows*-output fragment, which ADR 0040 Decision 4 refuses precisely because a fragment answers in one framed message and would materialise a whole region on the SQL node. Lifting that is the streamed response of §3.2 — a wire change, an ADR, and `esker-proto`, which this lane does not own. |
| **C** | **A semi-join pushed down as a filter**: the SQL node evaluates the small side, and the large side's fragment carries the join keys as a predicate | **Yes.** No wire change, no new output kind, and it attacks exactly the measured query. |

**C**, then. `SELECT count(*) FROM ledger JOIN dim ON ledger.ghigh = dim.k WHERE dim.bucket = 3`
becomes: read `dim` (small, filtered) on the SQL node at the statement's snapshot; collect the join
keys; send **one existing-shape fragment** over `ledger` whose filter is the outer filter `AND
ghigh ∈ {keys}`, with the aggregate it already carries; finish as the two-level aggregate already
finishes. The join disappears into a predicate.

## J3. When the rewrite is exact, which is the whole of its correctness

A semi-join is not a join. Replacing one with the other is exact only under conditions that must be
*checked*, not hoped for, and each maps onto something the planner already knows:

1. **`left_join == false`.** A `LEFT JOIN` keeps unmatched outer rows NULL-extended; a filter
   removes them.
2. **`probe` is `Probe::PrimaryKey` or `Probe::UniqueIndex`** (`plan/query.rs:316`). This is the
   condition that matters and the reason this slice is small: those two variants mean *at most one
   inner row per outer row*, so replacing the join with a membership test cannot change any
   aggregate's count. `Probe::Materialize` gives no such guarantee — one outer row may match many
   inner rows and `count(*)` must count it many times — so it is **refused**.
3. **No inner column is referenced above the join** — not in the aggregates, the grouping keys, the
   `HAVING` or the projection. A filter yields no `dim` values to project.
4. **`residual` is `None`, or reads only outer columns.** A probe answers its equality exactly, so
   a residual here is the part the probe did not express.
5. **The key set is bounded** (§J5). Above the bound the estimate refuses and says so.

Any of these failing is a refusal with a reason, never a partial honouring — ADR 0022 Decision 3's
rule, and the same one `esker_sql::plan`'s "reject, do not ignore" states.

NULLs need no special case: `a.x = b.k` is never true for a NULL on either side, and a key set
collected from `dim` contains no NULL, so `x ∈ keys` is false exactly where the join matched
nothing.

## J4. The fragment shape — one new expression node, and nothing else

The filter tree has `Column`, `Literal`, `Compare`, `And`, `Or`, `Not`, `IsNull`
(`esker-columnar/src/fragment/expr.rs`, tags 1–7). A membership test can already be written as an
`Or` chain of `Compare(Eq)`, and for a handful of keys that is what this will do. It does not scale:
2,500 keys against 200,000 rows is 500 million comparisons, which would lose to the row engine it
is replacing.

So: **`Expr::In { operand, values }`, tag 8**, values held sorted and probed by binary search (or a
small hash set) so a row costs `log n` rather than `n`.

**This is additive and needs no format-version bump.** An older evaluator meeting tag 8 refuses the
whole fragment — the behaviour DESIGN §16.2 already specifies for an unknown expression node — and
a refusal is a fall back to a row scan, which is the answer that was always there. It gets an
**ADR** regardless, because `CLAUDE.md` says a format change does, and the decision worth recording
is exactly that one: *additive tag, refusal as forward compatibility, no version bump*, with the
golden that pins it.

`MAX_EXPR_DEPTH` already bounds the tree; the value list needs a bound of its own (§J5).

## J5. The cost rule, and the bound

Routing a join needs one more comparison than routing a scan, because the SQL node does work
*before* the fragment goes out — it reads the inner side. So:

* **refuse above `JOIN_KEYS_MAX` keys** (start at 4,096, recorded and tunable, not a magic number
  in a branch);
* the inner side is read at the statement's own snapshot, through the row path, so it is the same
  read the join would have done anyway — the rewrite does not add a read, it *removes* the outer
  scan's row-wise pass;
* the existing ratio rule still applies to the outer table's projection.

## J6. What `EXPLAIN` says — closing the third silence

Every joined plan gets an `Engine:` line. Routed:

```text
Aggregate on ledger
  Engine: columnar  (join pushed down as a semi-join over 2500 keys)
  Columnar Aggregate on ledger  (1 fragment)
    Semi Join Filter: ghigh in dim.k  (2500 keys, from dim)
```

Refused, naming which of §J3 said no:

```text
Aggregate on ledger
  Engine: rows  (join not pushed down: the inner side is materialised, so one outer row may
                 match many)
  Nested Loop
    …
```

The reasons are the conditions in §J3, one string each, so a reader is told *which* rule refused
rather than that some rule did. This needs a new `Reason` variant (or a `Reason::Shape` refinement)
in `plan/routing.rs` — which is in this lane's `exec`-and-routing boundary — and it must print for
**every** join over a table with a columnar copy, including the ones that refuse. A join over a
table nobody asked for a copy of stays silent, which is Decision 3's first deliberate silence and
is correct.

## J7. What this unit will NOT do

* **No exchange, no shuffle, no spill.** Unchanged from §8, and now doubly gated: on the `split`
  lane, and on this unit existing at all.
* **No multi-region join parallelism.** The `split` lane has landed (ADR 0073), so a table *can*
  now span regions — which makes this a deliberate scope line rather than a fact about the system.
  A semi-join fragment goes to *each* region of the outer table and the partials merge exactly as
  they do now, needing no new mechanism; that it needs none is a point in this design's favour and
  is still not built or measured in this unit.
* **No columnar join fragment (option A) and no rows-output fragment (option B).** Both need
  something this lane does not own.
* **No `LEFT JOIN`, no `Materialize` probe, no join whose inner columns are projected.** All
  refuse, and `EXPLAIN` says which.
* **No three-way joins** in this slice: one inner side, checked, or refuse.
* **No new dependency.**

## J8. Test list, red first

The differential is the spec — every join answer from the columnar path must equal the row
engine's, compared as a `Result` so an error is an answer too.

1. **RED, first, before any of the rest:** a join over columnar-eligible tables asserted equal to
   the row engine and asserted to have *run on the columns*. It fails today at the second assertion,
   because the join never routes. This is the test the unit is written against, and it must assert
   its own denominator — `docs/bench/columnar-m2.md`'s lesson and ADR 0040 Decision 5's, where the
   differential found a bug as *"this query was not answered by the columns"* rather than as a
   wrong number.
2. `esker-columnar`: `Expr::In` round-trips through the codec; a golden pins tag 8; an unknown tag
   still refuses the whole fragment; the evaluator agrees with an `Or` chain of `Eq` over the same
   values, including NULLs and `-0.0`/`NaN` under `pg_cmp`.
3. `exec`: each condition in §J3 refuses, one test per condition, asserting the **reason** and not
   merely the fallback — a refusal for the wrong reason is a bug that passes a fallback test.
4. `exec`: the rewrite preserves the answer when the inner side has duplicate *non-key* columns,
   which is the case a careless uniqueness argument gets wrong.
5. Bench: the join measured before and after on `esker bench-mpp`, both engines, engine asserted
   from `EXPLAIN` on every run, recorded in `docs/bench/` beside the aggregate numbers with the
   absolute seconds and never a ratio alone.

## J9. Risks

* **The uniqueness argument is the whole correctness story**, and it rests on `Probe` meaning what
  it says. If a `PrimaryKey` probe can ever pair one outer row with two inner rows, this rewrite
  returns wrong counts silently. Test 4 exists for that, and the differential is the backstop.
* **The inner side is read twice** in the fallback case — once to collect keys, once by the row
  join — if the fragment then refuses. Collect *after* the routing decision, not before.
* **A large key set makes the fragment large.** `JOIN_KEYS_MAX` bounds it; the encoded fragment
  should be checked against `max_frame_size` (16 MiB) rather than assumed under it.
* **The bound is a magic number** until a measurement moves it. It is recorded in one place with
  its reasoning, and the bench in test 5 is what will move it.

## J10. Where the differential stands, and what is actually deferred

**Granted and taken:** the join differential lives in
`crates/esker-sql/tests/routing_differential.rs`, extending the harness it models rather than
copying four hundred lines of it. Two tests, and the first is red:

```text
test a_join_over_columnar_tables_answers_what_the_row_engine_answers ... FAILED
     `SELECT count(*) FROM f JOIN d ON f.dk = d.k WHERE d.bucket = 1` was not answered by
     the columns, so its agreement is free
test a_join_the_rewrite_cannot_express_stays_on_the_rows ... ok
```

It fails at the **second** assertion, which is the one that matters: the answers already agree,
trivially, because both are the row engine. What is missing is the routing. The second test — the
three joins that must *never* route — passes today and must still pass afterwards; over-routing is
the failure that returns a wrong number rather than a slow one.

### The harness is more real than "in-process", and the record should say so

This was handed over as an *"in-process harness [that] cannot exercise a real cluster"*. That
undersells what is there, and the difference decides what is genuinely deferred.
`routing_differential.rs` binds real `TcpListener`s on ephemeral ports, runs a real `esker-pd` and
`STORES` real stores over real sockets, issues a real `ALTER TABLE … SET (columnar_replicas = 1)`,
waits for PD to place a real `PeerRole::ColumnarLearner`, and waits again until that learner has
**answered a fragment** — it even has `stop_the_learner` and a test that kills it mid-flight. Its
own module docs say the one thing that is not separate is the OS process boundary.

So what this join differential does exercise: a real columnar learner, placed by PD, answering real
fragments over real sockets, compared against the row engine at one snapshot.

What is **actually** deferred, named so nobody assumes it is covered:

* **The multi-process form.** `esker bench-mpp` starts PD, stores and the SQL node as separate
  processes; the join is measured there (§J8.5) rather than asserted there.
* **Multi-region.** One region per table until the `split` lane lands, so one fragment. A semi-join
  fragment to *each* region of the outer table, with the partials merged as they already are, needs
  no new mechanism — which is a point in this design's favour and is still not built or tested here.
* **A join whose outer table spans regions.** No longer gated on `split`; gated on this unit's
  own scope line above.

### The `In` node's ADR is 0074, not 0073

Claimed as 0073 against main's then-highest 0072, and the `split` lane landed its own 0073 first.
Later committer renumbers, so this one moved to
[0074](../adr/0074-a-fragment-expression-node-is-added-by-tag-not-by-version.md). The commit that
introduced it (`68bfdf02`) names 0073 in its message and that is now wrong; it is recorded here
rather than rewritten, because the history is what a reader greps and a message that silently
disagreed with the file would be worse than one that is corrected in the open.

### J11. What was left — **built 2026-09-05, and this section did not say so**

> **Corrected 2026-09-09.** What follows described the state before `2e368a98`, and stayed after it.
> The cost of that is on the record: the unit was ruled on again four days later, and it was one
> `git log -S` away from being built twice. A plan section that names something as *left* is a
> request for work, so it has to be closed by the commit that answers it.

Built on 2026-09-05 by `2e368a98`, all of it:

* **the field** — `routing::Columnar::semi_join`, carrying the inner key plan, the outer fragment
  slot and the inner table's name. The commit records the grant in its own words: *"the one field
  this lane was granted in `src/plan`"*, so the permission this section asks for had already been
  given when it was written;
* **the rewrite** — `exec::fragment::push_the_semi_join_down`, called from `resolve` at exactly the
  seam this section names, reading the key set through `Cursor::open` in the same transaction at
  the same snapshot as both the fragment and the fallback, folding it into the filter as an
  `Expr::In` and `And`-ing it with whatever filter was already there;
* **the edges** — a NULL key dropped (it matches nothing, and `Expr::In` refuses a list holding
  one), an inner side with no rows expressed as a literal `false` rather than an empty `IN`, the
  values sorted and deduplicated into the strictly ascending order the decoder requires, and a key
  of a type no fragment carries refused rather than silently turned into a NULL;
* **the cap and the fallback** — more than `MAX_IN_VALUES` keys refuses with a reason and the row
  plan answers, at the same snapshot;
* **`EXPLAIN`** — `Semi Join Filter: dk in d  (2 keys)`, the inner table and the key count;
* **the acceptance** — `a_join_over_columnar_tables_answers_what_the_row_engine_answers`, which
  asserts its own denominator, plus `explain_shows_the_join_it_absorbed`,
  `a_join_whose_inner_side_is_empty_answers_zero_on_both_engines` and
  `a_join_the_rewrite_cannot_express_stays_on_the_rows`. Green in the tree.

**What is actually left is the number.** `MAX_IN_VALUES` is 4,096 and it is a *format* limit — the
most keys a fragment can carry — which is not the same question as the most keys it is *worth*
carrying. Nothing has measured where an `In` of N keys pushed to every region stops beating a
nested loop on the row path, and a planner-side threshold below the format's ceiling is what that
measurement would buy. Until it is measured the cap is the format's, which is safe and possibly
generous.

### The number: what is being asked, and what the answer will be worth

`MAX_IN_VALUES` is a **format** ceiling — the most keys a fragment can carry, checked by the codec
and enforced again in `push_the_semi_join_down`. The planner has no threshold of its own, so today
every join the rewrite can express is pushed down, up to 4,096 keys.

**The two costs move in opposite directions**, which is why a crossover should exist at all:

* the **pushdown** grows with N — N values encoded into the fragment, shipped to *every* region of
  the outer table, and a binary search per scanned row over an N-value list. And the larger N is,
  the *less* the filter removes: at the extreme it matches nearly every row, so the membership test
  is paid on all of them and buys nothing;
* the **nested loop** is roughly flat in N — it scans the outer table and probes the inner one per
  row, and the inner side being narrower changes how many rows *survive*, not how many are probed.

So the question is where the growing line crosses the flat one, and the answer is a planner-side
threshold: above it, refuse the rewrite with a reason and let the row plan answer — the fallback
that already exists, at the same snapshot, with `EXPLAIN` naming the refusal.

**What the measurement will be worth, stated before it is taken.** The fixture is one region's
worth of outer rows, so the shipping cost is paid **once**. A table spread over R regions pays it R
times, and the per-row binary search is paid on each region's own rows — so a threshold measured
here is an **upper bound**: the real crossover on a split table is at a *smaller* N, never a larger
one. If the curve says "no crossover below the ceiling", that is an answer too, and it means the
ceiling is the right place to stop for a single-region table and an open question for a wide one.

The other thing recorded per row is whether the columns **actually answered**. A pushdown that
refused and fell back is the row path timed twice, and a curve made of that would show the two paths
identical everywhere — §10's free agreement, wearing a stopwatch.

## J12. `08006 … key is not in region 0`, and what a fragment may do about it

The join differential went red on the ci-tree with a message that looks like a routing bug and is
one — but not the one it was first read as. This section records what the error is, because the
repair that fixes it is not the repair the symptom suggests, and two lanes were briefly about to
build the wrong one.

### What the message is

`region 0` is not a region. It is synthesised by the **client's own router**, in
`crates/esker-client/src/router.rs`:

```rust
let route = self.resolver.locate(key)?.ok_or_else(|| {
    ProtoError::KeyNotInRegion {
        key: Bytes::copy_from_slice(key),
        region_id: 0,
        start_key: Bytes::new(),
        end_key: Bytes::new(),
    }
})?;
```

So the error means **the placement driver answered "no region covers this key"** —
`PdReq::GetRegion` returning `region: None` (`crates/esker-pd/src/service.rs`), which is
`pd.get_region` finding nothing in the range index (`crates/esker-pd/src/pd/mod.rs`). No store was
asked and no store refused. Three consequences follow, and each one contradicts a reasonable first
reading:

1. **It is not invariant 5 firing.** A store refusing a stale epoch answers `EpochNotMatch`, and a
   store refusing a key outside its range answers `KeyNotInRegion` carrying *its own* region id and
   *its own* bounds (`crates/esker-store/src/region.rs`, `not_in_region`). A real region id and
   real bounds are the signature of a store refusal; `0` with two empty bounds is the signature of
   the driver having no answer.
2. **"Believe the refusal's bounds" cannot be done here.** The bounds are empty by construction.
   This matters because the repair already exists twice — `txn.rs`'s scan loop and `raw.rs`'s, both
   spending `SCAN_ROUTE_REFRESHES` on it — and **both guard on `owns(&start_key, &end_key, cursor)`**,
   which is false for an empty range. That is precisely why the existing repair does not catch this
   case and the error reaches the client: not a missing repair, a repair whose precondition this
   error cannot meet.
3. **It is transient, and classified terminal.** `KeyNotInRegion` is `Verdict::Surface` — correct
   for a key that belongs to no region, wrong for a key whose region was created a moment ago and
   whose driver has not caught up. A split is applied by the store immediately and reaches PD at
   the next region heartbeat, so there is a window in which the driver's routing table is behind
   the cluster. `cross_region_scan.rs` hits it on **"the last row, in the last region"**, which is
   exactly where a freshly split upper half lives.

So the half of the shared helper that fixes this is the half written as *"bounded retries against a
stale driver"*, not the half written as *"believe the refusal's bounds"*. The second half is
already built, twice, and is being unified; the first is the new work, and `region 0` is its test
case.

### What the fragment path needed, and what it did not

The fragment dispatch never surfaced this error and could not have: `exec::fragment::evaluate`
treats every `Err` from a source the same way the module docs promise — fall back to the row plan
at the same snapshot — so an `08006` seen by a client on a routed query came from the **row
fallback**, not from the dispatch. There was therefore no second copy of the repair to write here,
and none was written.

What was missing is different, and is this unit: **a fragment refused for a stale route gave up on
the columnar path entirely.** On a cluster that is actively splitting, every routed query paid a
full fallback. `evaluate` now asks the source for the regions that cover the refused shard's range
and walks them in its place — one call to the trait's existing `shards`, so h1's helper lands
underneath it and needs no signature here.

### The rule, and why it is stricter than the row path's

**A fragment is not a cursor.** The scan paths repair a route by believing the store's bounds and
*continuing* from where they stopped, which is safe because a resumed walk covers each key once
however the boundaries moved. A fragment is an aggregate over the whole of one region's columnar
copy with no key range applied — `esker_columnar` refuses a `KeyRange`, because a columnar file
records none — so a re-dispatch changes not where a walk resumes but **which rows are counted**. A
replacement set covering one byte more than the shard it replaces counts that byte twice: once
here, once against whichever original shard also holds it.

So the replacement is followed only when it **tiles the refused range exactly** — equal at both
ends, no gap between:

| what happened | replacement | followed? |
|---|---|---|
| split | re-tiles the range exactly | yes |
| merge | reaches past the refused end | no — rows |
| gap | covers less than the range | no — rows |
| store down | the same shard back, unchanged | no — rows, and the budget is not spent |
| region keeps moving | legal every time | up to `MAX_ROUTE_REPAIRS` (4), then rows |

Each row is a test in `crates/esker-sql/tests/fragment_route_repair.rs`. Three of the five assert
the **fallback and the requests sent**, not the answer, and that is the point: the row engine is
correct, so agreeing with it proves nothing about which rule produced the agreement — the sentence
this whole thread turns on. The two that do assert an answer assert **12** against a table holding
**2**, so a dispatch that quietly stopped routing cannot pass them.

The budget is four, matching the scan paths' `SCAN_ROUTE_REFRESHES`, and for the same reason: each
repair leaves the query aimed better, but a region that moves on every attempt is one this
statement cannot read at this snapshot, and the row plan answers it correctly while an unbounded
walk would not answer it at all.

One operator-facing consequence, deliberate: `EXPLAIN ANALYZE` counts the fragments **sent**, so a
query that followed a split reads `Fragments: 4 asked, 3 answered` and still answers completely.
That is not a lost fragment — it is the one visible sign that this query met a region that was
moving, and flattening it to `3 asked` would hide the only evidence the repair ever runs.

### The driver-side half, and the one line that still connects it

`esker-client`'s `router::repair_route` (h1, `50fbad88`) is the shared repair, and it reaches the
same reading of `region 0` independently: *"a fact about the driver's knowledge, not about the
cluster"*. It believes the refusal's bounds when they contain the key and otherwise re-resolves on
the router's own jittered backoff before giving up.

**The fragment path reaches it through the enumeration it already calls.** `FragmentClient::shards`
was the one walk of the three that mapped `Router::route`'s error straight to its caller, which is
where `region 0` actually surfaced; h1's v47 made it a third caller of `repair_route`. So:

```rust
let mut route = match self.router.route(&key) {
    Ok(route) => route,
    Err(refusal) => crate::router::repair_route(&self.router, &key, &refusal)?,
};
```

**Nothing further is owed in `esker-sql`, and `repair_route` should stay `pub(crate)`.** It was
offered to be widened for this dispatch; it does not need to be. `re_routed` asks the trait's
`shards`, which *is* that function, so the repair arrives underneath with no new seam and no second
copy — and a planner reaching into the client's concrete types is precisely the coupling
`crate::fragment`'s trait exists to prevent (`crates/esker-sql/src/fragment.rs`, module docs).

It also keeps this dispatch clear of the trap the helper turns on. `repair_route` believes a
refusal's bounds when they contain the key — but `owns(b"", b"", key)` is **true for every key**, so
the driver's own empty-bounded refusal would be believed as if it were a store's, and an empty range
would come to mean the opposite of what it says. `re_routed` never reads a refusal's bounds at all:
it re-enumerates and checks the tiling of what comes back, so the distinction is one it cannot get
wrong because it never makes it.

### Whether `region 0` deserves its own error

Flattening "no region covers this key" into `KeyNotInRegion` is what makes it indistinguishable
from a store's refusal at every call site that matches on the variant — and what made a repair
guarded on `owns(&start_key, &end_key, ..)` silently skip it, since the synthesised bounds are
empty. Naming it separately is an `esker-proto` change and an ADR, and is the coordinator's to
sequence — recorded here rather than improvised.

## J13. A fragment answered from a copy that did not have the row — 2026-09-05

`esker-sql::joint_gate the_learner_answers_fragments_that_agree_with_a_row_scan` failed in a gate at
load ~13: the fragment answered 4 rows and the row scan 5. The fragment was missing `id 4`
("barbara"). 3/3 green alone at 0.47–2.65 s, so a timing window rather than starvation — and a
**wrong answer**, which is the failure this whole feature is built to not have.

**Closed 2026-09-05.** What follows is the finding; the hypotheses it replaced are kept at the end,
because two of them were wrong in ways worth not repeating.

### One entry, not a history

Every store, **including the columnar learner**, held `id 4` in its row store, and all four reported
`applied = 40`:

```text
min_apply_index  38
store 4  leader=Some(false)  applied=Some(40)  columnar=true
  id 4: 1 write records, reads as 19 bytes
```

So this was never a replica that was behind. Its **columnar copy** did not have the row, and the
fragment was allowed to answer anyway.

The rows the test inserts arrive in **one statement**, so they are **one transaction and one Raft
entry**:

```sql
INSERT INTO t VALUES (1, 'ada'), (2, 'grace'), (3, 'edsger'), (4, 'barbara')
```

and every later statement touches those rows again, one at a time. A copy that missed **that single
entry and nothing else** answers exactly what the dump shows:

| row | later write | what the fragment answered |
|---|---|---|
| 1 | `UPDATE ... 'ada lovelace'` | yes, at its new value |
| 2 | `UPDATE ... region 'east'` | yes, at its new value |
| 3 | `DELETE` — a tombstone | correctly absent |
| 4 | **none** | **absent** |
| 5, 6 | inserted later | yes |

`'ada lovelace'` and `'east'` included. **One entry was lost**, and `id 4` is the only row whose
visible state depended on it.

### What loses an entry: a snapshot install

A snapshot writes committed versions **straight into the column families**. No entry applies, so
`RaftPeer::tee_columnar` never runs, and the log it lands on begins after the snapshot's index. A
replica that falls behind its leader's compaction boundary is repaired by exactly this — which is
what load does, and why the test was green alone.

`Store::fetch_snapshot` replaces such a region through `Store::retire_region_now`, which stops the
peer and, until this was fixed, left the columnar slot in the store's map; `host_region` then
reattached it. The copy that came back was the one that was there before, missing every version
between where it had got to and the snapshot's index — and with no path back to them, because the
log cannot replay entries it has truncated and `ColumnarSlot::ensure` short-circuits on a table that
is already open, so an open copy is never re-walked. Entries after the snapshot are teed normally,
which is why the copy then looks current and answers confidently.

The refusal rule that covers this **already existed**: `resume` will not replay from a manifest
older than the log's truncation point, and `columnar_resume.rs` has tested that since
[ADR 0038](../adr/0038-a-run-manifest-names-the-index-its-runs-are-complete-to.md). It was only ever
consulted on the way in.

### The fix

`ColumnarSlot::saw(index)`, called for **every entry a columnar learner applies**, committing or
not, drops every open copy when that entry did not follow the last one the slot was told about. The
next `ensure` re-opens and the resume rule sends it to a re-walk. `Store::retire_region_now` also
drops the slot, closing the hole where it opens rather than at the next entry.

Red-first: `a_copy_not_told_of_every_entry_re_walks_instead_of_answering` fails `5` against `6` with
the check neutered. Green: 335/335 on `-p esker-store --all-features`.

### The three readings that were wrong, and what misled each

**"The gate proves the wrong thing."** `catch_up` establishes freshness with
`read_index_as_learner` and `peer.applied_index()`, both statements about the **row** state machine,
while the fragment is answered from the columnar copy. That much is true and is still worth knowing.
The conclusion drawn from it — that gating on the copy's own index would have caught this — was
wrong twice over. `ColumnarApply::applied_index()` is `runs.applied()`, the **durable manifest**
index, which moves only on a seal; `catch_up` runs *before* `slot.table()` seals, so between seals
it names less than the copy holds and such a gate would refuse fragments that are perfectly fresh.
And here the copy was not behind by its own reckoning at all — it had been teed entries well past
the missing one — so the gate would not have refused this answer. Withdrawn by the coordinator once
the first half was shown.

**"The row was skipped because `is_columnar_learner()` was still false."** An entry applied before
a peer's region record calls it a columnar learner is genuinely not teed. But the copy's first open
**rebuilds from the region's committed state**, which covers every such entry, so the gap is real
and self-healing and was not this. It is closed anyway: the first entry after the role lands is a
gap in `saw`'s reckoning.

**"The copy holds the stream and not the history."** The shape of the dump invites this, and the
`INSERT`-as-one-entry reading is what dissolves it. A fix built on it would have aimed at the
conversion walk, which was correct all along.

### The lesson that cost the most

The first version of the fix compared indices **on the read path**, when a fragment asks. It passed
its test and it does not work: the peer carries on applying, and one entry teed after the gap
advances the index past the truncation point again, so a fragment arriving later sees nothing wrong.
The test passed only because it teed nothing afterwards — a test passing on the mechanism it was not
testing.

**A gap is a fact about the moment it happens**, and the next entry destroys the evidence. That is
why the check is on the apply path. And it is fed by every entry rather than by commits, because a
prewrite commits nothing and still applies, so a detector fed by commits alone would see a gap at
every transaction.

### The property the tests pin

Two, at the two levels where the claim can be made.

`esker-store/tests/columnar_resume.rs` —
`a_copy_not_told_of_every_entry_re_walks_instead_of_answering`. Fully deterministic, no threads and
no timing, next to the resume rule it is the missing half of. A copy is opened and **kept**, rows
then arrive **without a tee** on a log compacted past everything the copy holds — both halves of a
snapshot install — and one further entry is applied and teed, which is what makes the copy's indices
look continuous again. The re-walk has to be decided at that entry or not at all.

`esker-store/tests/snapshot.rs` —
`a_placed_columnar_learner_answers_for_the_rows_that_predate_it`. The twin of
`a_placed_columnar_learner_holds_what_the_leader_holds`, which proves the learner's **row** column
families hold what the leader's do and stops there — deliberately, because when it was written the
fragment service was the thing it stood in for. This asks the fragment service itself, through the
same `Service` the server dispatches through, so `Store::serve_fragment` runs whole: the epoch
check, the role check and the catch-up. Nothing anywhere asserted that a *placed* learner answers a
fragment for rows committed before it existed.

### Where this was left

`Store::retire_region_now` drops the region's columnar slot as well, so the hole is closed where it
opens and not only at the next entry. The gate on `catch_up` that was originally specified —
`ColumnarApply::applied_index()` against `min_apply_index` — was withdrawn once the durable-seal
reading above was shown; if it is ever wanted as a safety net against *other* ways a copy can lag,
it needs `complete_index` and it needs to run after a seal.

## J14. The first write on a fresh connection was refused 25006 — 2026-09-05

Six sightings on real clusters: `SELECT 1` succeeds, the first **write** comes back

```text
ERROR 25006: cannot execute INSERT in a read-only transaction: this node's schema lease has
expired and the placement driver is unreachable
```

and nothing on PD or on the node logs anything. Reads keep working throughout, which is what makes
it look like a client problem.

### What it is not

The startup path was the first suspicion and it is sound: `attach_pd` fetches the lease
**synchronously**, fails startup if it cannot, and the client socket opens only afterwards — so a
fresh connection always finds a lease that has been granted. PD's `SchemaLease` handler is pure
config with no leader gate, so PD's "no cluster yet" does not refuse it. And `StoreBackend`
overrides both lease methods rather than inheriting their trait defaults, which was the one reading
that would have made the evidence below meaningless.

### The mechanism

`LeaseRefresher::run` was one loop doing two jobs:

```rust
loop { sleep(lease / 3); renew(); report(); }
```

The renewal is recorded before the report, so *that* renewal is never late — which is what the
code's own comment claimed, and it was true and not sufficient. **The next sleep did not begin until
the report returned.** The report is `columnar_wishes`: a transaction opened against the cluster and
a catalog range scanned across it, whose cost is a region mid-split, a leader that has moved, or a
store saturated by someone else's load. A report costing more than the remaining two thirds of the
lease lets it expire — and nothing logs it, because the renewal succeeded and a slow read is not an
error.

### Confirmed by instrumenting, not by arguing

A temporary log of each round's two halves, then the same six attempts on the real-cluster harness:

```text
attempt 3:  SLOWROUND  renew_ms=0  report_ms=3879  period_ms=1666   (lease_ms=5000)
```

`renew_ms=0` — the renewal is instant. `report_ms=3879` — the report is the whole of it. The next
renewal was due 3879 + 1666 = **5,545 ms** after the last, 545 ms past expiry, and the same attempt
is the one whose `INSERT` returned 25006. **`ERROR 25006` and `SLOWROUND` appeared in attempt 3 and
in no other, one for one, neither ever without the other.**

(The script's own summary line first said 6 of 6. `$(grep -c … || echo 0)` yields `"0\n0"` when grep
matches nothing, and `"0\n0" != "0"`, so every attempt scored as a hit. The per-attempt lines were
right; the total under them was not. Read the per-item outcomes.)

### The fix

Three parts, none of them a grace period and none of them a wait on the write path:

1. **The renewal loop schedules by deadline**, sleeping until `round_start + period`, so the cost of
   a renewal comes out of its wait rather than being added to it.
2. **The report moved to a thread of its own** with its own cadence and one round at a time. Its PD
   half already carries a transport deadline; its backend half is a cluster read and its duration is
   **not** bounded — which is exactly why it is no longer allowed near the renewal.
3. **A renewal landing more than half a lease after the previous one logs a warn** with both
   durations, in `PdLease::record` so that every path recording a lease is measured. Half rather
   than the whole, because at the whole the node has already refused a write and the log arrives
   after the client's error.

### The residual the test found after the fix

With the loop fixed the deterministic test still failed, at **1 lapsed sample in 263** rather than
166 in 258 — the same 1 every run, so structural. It is the startup round: `refresh()` recorded the
lease and *then* ran a report that outlasted it, handing the client socket a lease already aged by
the whole report. That is the field report's "the first renewal has not landed", exactly.

So the startup round now reports **first** and renews **last** — the opposite order to the loop, for
the opposite reason: nothing is being kept alive across it, and what matters is that the lease be
fresh when it returns, because the caller opens the socket next.

Worth recording that the test found this and the reasoning did not: the mechanism above explains the
loop, and the loop was only most of the bug.

### The proof, in three states of the same tree

| tree | lapsed samples, of ~260 | |
|---|---|---|
| the loop as it was | 166 / 258, 146 / 246, 165 / 258 | red ×3 |
| the loop fixed, startup untouched | 1 / 263, 1 / 266, 1 / 257 | still red ×3 |
| both | **0** | **green ×5**, ~4.5 s |

Red and green were run by **one script that owned the tree for its whole duration** — red three
times, `git apply` the fix, green three times — because the first attempt at this had me editing
`pd.rs` while the red proof was still building against it, which would have compiled the fix into
the runs that were supposed to prove it red.

### The test

`esker-sql/tests/pd_wiring.rs::a_report_slower_than_the_lease_does_not_expire_it`. Deterministic and
clusterless: the existing `StandInPd` on a socket, the real `PdConn`, `PdLease` and
`LeaseRefresher`, and a backend given to **the refresher only** whose `begin()` sleeps three
lease-lengths. Sessions keep the ordinary backend, because what is under test is the renewal's
cadence and not a slow statement.

It watches `Backend::schema_lease_remaining()` — literally the value the write path turns into
`25006` — rather than any proxy for it. The slow-report wrapper forwards `schema_lease_remaining`
and `schema_step_interval` explicitly: both have trait defaults, and a wrapper that inherited them
would answer "this node may always write" from the very object the test uses to watch a lease lapse.

## J14. The two engines disagreed once — 2026-09-09, not reproduced, and what will say so next time

`esker-sql::routing_differential the_two_engines_agree_while_a_writer_keeps_committing` failed once
in the gate for `76001434` at 03:18, at load 8–10:

```text
the two engines disagree (under a writer) on
`SELECT region, count(*) FROM t GROUP BY region ORDER BY region` at 468962246262784000
routed: [[north 711], [south 8]]
```

The row engine's answer is not in the record — the gate log keeps the first lines of a failure and
the diagnosis was cut one line short of it, which is the first thing this section is here to stop
happening again.

### What it cannot be

**Two different moments.** `compare` pins both runs to one instant with
`SET TRANSACTION SNAPSHOT 'esker-<16 hex>'` — by token rather than by `read_as_of`, because a TSO
timestamp's logical half is what separates two commits inside one millisecond and no time a user can
write carries it. And the contract is explicit that the instant is enough: `FragmentReq::ts` is
**one number with two jobs**, the snapshot the request is made under *and* the MVCC visibility the
evaluator applies while it scans, deliberately separate from `min_apply_index` because *"they fail
differently — one refuses with `TooFarBehind`, the other silently returns older data"*.

So at one pinned `ts`, two answers is a wrong answer. What it does **not** say is which side, and
that is exactly what the record could not settle.

### Ten rounds, and they do not reproduce it

`the_two_engines_agree_while_a_writer_keeps_committing`, alone, ten times, 2026-09-09:

| rounds | one-minute load | result |
|---|---|---|
| 1–2 | 4.6 – 4.9 (quiet) | green, 11–25 s |
| 3–5 | 10.0 – 15.2 | green, 25–58 s |
| 6–8 | 16.3 – 16.7 | green, 22–86 s |
| 9–10 | 13.6 – 14.4 | green, 22–24 s |

The load was ambient — other lanes building and a gate running — rather than an arm, and it is
recorded because it happens to span the band the sighting fell in and four rounds above it. Ten
green says the window is narrow, and nothing else. *(The controlled six-thread arm is a separate
row; see the handover for its numbers.)*

### The other red this test has, and why it must not be read as this one

Under a **six-thread arm on a box already at 31**, round 3 of five failed — and not as a
disagreement:

```text
the snapshot imports: StoreUnavailable("gave up after 9 attempts: peer is not the leader of region 1")
```

That is the three-store cluster losing its leader while the box is starved, caught at
`SET TRANSACTION SNAPSHOT` before either engine answered anything. It is an **availability**
failure of the harness, it happens at a load band far above the one the sighting fell in (8–10),
and in a gate log it appears under the same test name as the thing being hunted.

Worth stating because the two want opposite readings: a disagreement is a wrong answer and a
`not the leader` is a machine with nothing left. A red on this test is not evidence of the first
until its message has been looked at.

### The instrument, so the next sighting is self-diagnosing

Three things are printed on a disagreement now, and one of the three the investigation asked for
turns out not to exist.

1. **Ask again, at the same instant, in the reverse order.** A snapshot is a function: one `ts`
   must answer the same rows for ever, on either engine. So the second pair separates two
   investigations that want opposite work — *disagreeing again* is a **deterministic wrong answer**
   (go to the runs), *agreeing the second time* means the read was **never pinned** (go to the read
   path). Reversing the order distinguishes "it follows the engine" from "it follows which ran
   first".
2. **Every store's applied index**, taken in process from the harness's own stores.
3. **A run's `[lo, hi]` window does not exist**, and asking for one is the wrong question here:
   runs hold **every version** and visibility is resolved at read time — *"the newest version of
   each key with `commit_ts <= ts`"* — so a run is bounded by an **apply index**, not by a time.
   Point 2 is what stands in for it. Putting an apply index on `FragmentResp` would be a format
   change for a diagnostic, and it is not made for one.

### What a deterministic probe now rules out

`a_pinned_snapshot_does_not_move_when_later_rows_commit` asks the sequential half of the same
question and **passes**: take an instant, answer it on both engines, commit a hundred rows, answer
the same instant again — nothing moves on either side, and the columns did answer (the test fails if
they refused both times, which would have been the row engine compared with itself).

So visibility at a pinned `ts` is sound when the commits are *between* the reads. Whatever the
window is, it needs a commit landing **while** a fragment is being evaluated. That is a narrowing
worth having: it is measured rather than argued, it is permanent, and it costs 1.7 s.

It also removes the asymmetry the concurrent test cannot control. There the routed run goes first
and the row run second, so **whichever engine fails to pin sees more commits by the time it runs** —
one number cannot say which failed. The probe asks each engine to agree with *itself* across a
hundred commits, and the instrument above re-asks in the reverse order for the same reason.

### The nearest prior, and what would tell them apart

[J13](#j13-a-fragment-answered-from-a-copy-that-did-not-have-the-row--2026-09-05) is the same family
of question and was closed on 2026-09-05: a columnar copy that missed one Raft entry after a
snapshot install answered **four rows where the scan had five**. Its fix — `ColumnarSlot::saw(index)`
dropping an open copy the moment an entry does not follow the last one it saw — is a gap detector,
and a gap of that kind should no longer be reachable.

So the direction of the error is what separates them, and it is the number the record lost:

* the columnar side **lower** than the rows is J13's shape — a copy missing versions — and would
  mean the detector has a hole;
* the columnar side **higher** is not J13's shape at all. Nothing in a missing-entry story adds
  rows. It would point instead at version resolution: `scan::visible`'s resolver carries **one**
  settled key, which is sound only while the merged stream is globally ordered by key, and
  `scan::merged::order` skips a key column it cannot read on either side rather than treating the
  rows as incomparable.

That second sentence is a place to look, **not a finding** — no measurement here supports it, and
the ten rounds say nothing about it either way. It is written down so the next sighting is read
against something rather than from scratch.
