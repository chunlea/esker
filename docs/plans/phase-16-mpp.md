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

1. **A SQL table that splits.** Until then every other line here is moot. It is also the cheapest of
   the three: the size estimate and the boundary scan both need to see the `'x'` namespace and the
   Percolator column families.
2. **A finishing cost that grows past the scan.** The shape to watch is `regions × groups`: at 50
   bytes and, say, 40 µs a group (36 ms over ~900 groups' worth of measurable difference — an upper
   bound, since the tick hides the rest), a hundred regions each holding 100,000 groups would ship
   500 MB into one node and merge ten million partials. That is where an exchange earns its keep,
   and it is four orders of magnitude from anything this system can currently produce.
3. **Parallel dispatch first, if regions ever do split.** Fragments are asked one at a time
   (§1), so R regions cost R round trips before any merging happens. That is `esker-sql` and
   `esker-client`, no wire change, no shuffle, no spill — and on today's numbers a 25 ms round trip
   per extra region would dominate the 36 ms the exchange is aimed at. **Measure again after
   parallel dispatch, not before.**

### What this measurement cannot say

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
* **No multi-region join parallelism.** One region per table today, so one fragment. When the
  `split` lane lands, a semi-join fragment goes to *each* region of the outer table and the
  partials merge exactly as they do now — no new mechanism, which is a point in this design's
  favour and is deliberately not built or measured here.
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

## J10. One path question for the coordinator

The cluster-level differential in §J8.1 belongs beside `crates/esker-sql/tests/routing_differential.rs`,
which is the harness it models — and this lane's paths stop at `crates/esker-sql/src/exec/**`. A
**new** file (`tests/routing_join_differential.rs`) conflicts with nobody, but it is outside what
this lane was given. Asked rather than assumed. Until it is answered the red test lives as far in as
it can reach — the rewrite's decision and the `In` evaluator have unit tests in `src/exec` and
`esker-columnar` — and those cannot exercise a real cluster, which is where ADR 0022 says this
defence has to stand.
