# A SQL table that occupies more than one region

Lane `split`, branch `split-fix`, cut from `main` at `07e2f128`. Closes the first item of
`docs/plans/phase-16-mpp.md` §10 "What would change this verdict": *a SQL table that splits*.

## 1. The finding

A SQL table always occupies exactly **one** region, whatever its size. Range sharding — the
system's whole scale-out story (`CLAUDE.md`: "Linear scale-out by splitting key ranges into
regions") — does not trigger for SQL data at any threshold.

The measurement is in `docs/plans/phase-16-mpp.md` §10: a region holding 200,000 rows reports
`~0 bytes` to PD; a 4 MiB threshold over ~20 MB of table produced one region, identical to the
run that asked for one with a 512 MiB threshold. **The threshold is not the variable — the
measured keyspace is.**

Two functions in `crates/esker-store/src/split.rs` look at one namespace in one column family:

* `approximate_size` (:174) calls `db.approximate_size(cf::DEFAULT, ['r'++start, 's'))`;
* `choose_split_key` (:115) scans the same range and `strip_prefix(&[prefix::RAW])`s every key.

A SQL row is the *user key* of a transaction, so it reaches the engine as
`'x' ++ enc('t' ++ tenant ++ table ++ 'r' ++ row_id) ++ !ts` — the `'x'` namespace, and the
Percolator families. Neither function can see a byte of it.

### The part that is easy to get wrong, and that decides the fix

Widening the range to `['r', 'y')` in `default` — the obvious "fix" — **still reports ~0 for a
SQL table**. `esker_txn::codec::SHORT_VALUE_MAX_LEN` is 255, and a value at or under it is
**inlined into the `write` record**; `default` gets no entry at all
(`crates/esker-txn/src/percolator.rs`, `Op::short_value`/`long_value`). Ordinary SQL rows are
short. So for the workload that produced the finding, essentially **every byte of the table is in
the `write` column family**, and any fix that does not read `write` leaves the finding in place
while looking like it closed it.

## 2. What `approximate_size` should measure

A region owns `[start_key, end_key)` of the **user** key space. `esker-store/src/snapshot.rs`
already knows how that maps down — `physical_ranges_of` (:454), the mapping
[ADR 0032](../adr/0032-a-snapshot-carries-every-column-family.md) established after format
version 1 shipped a snapshot that walked `default` under `'r'` alone and dropped every
transactional record a region held. **That is this bug, one layer over.** The same region shipped
its transactional records correctly and reported none of them.

So: one definition of the mapping, used by both. `physical_ranges`, `physical_ranges_of` and
`PHYSICAL_NAMESPACES` move out of `snapshot.rs` into a new `crates/esker-store/src/keyspace.rs`
and are called from `split.rs` as well. No behaviour change in `snapshot.rs`; it imports what it
used to define.

**Which column families count as a region's size:** `default` and `write`. Not `lock` — a lock is
one in-flight transaction's state, cleared at commit or rollback, so counting it makes a region's
size a function of concurrency and lets a burst of prewrites trigger a split of data that has not
been written. Not `raft` — that is this store's log, keyed by region id and not by user key, and
it is what `SNAPSHOT_CFS` deliberately excludes for the same reason.

**Both physical namespaces in both families**, rather than a table of which family may hold which
namespace. `write` holds only `'x'` today; a scan of the `'r'` range in it costs one seek that
fails its bound immediately, and it cannot go stale. `snapshot.rs:281` states this reasoning for
the snapshot stream and this follows it.

So the size is the sum of four `Db::approximate_size` calls: `{default, write} × {'r' range, 'x'
range}`. Each is the same hint it was, with the same three over-counting directions
`Db::approximate_size` names; all this changes is that the hint now covers the data.

## 3. The boundary-search fix

`choose_split_key` must sample **user keys**, because the split key is a user key: it is checked
by `is_legal_boundary` against `region.start_key`/`end_key`, it goes into the `Split` command, and
every peer applies it against its own copy of the range.

The scan reads **exactly the (family, range) pairs the size counts** — the same four. That is the
property worth stating, because the failure it rules out is the one a partial fix produces: a
region whose size says "split" and whose boundary search says "there is nothing here", which the
split checker records as `refused` and does not look at again until the region has grown by
another whole threshold (`server.rs:1445`).

Each source yields ascending user keys:

* `'r'` range: user key = the key with `'r'` stripped, as today.
* `'x'` range: user key = `esker_txn::key::split(key).0`. A user key has one entry per version, so
  a source **advances to the next distinct user key** — otherwise the sample is weighted by how
  often a row was updated rather than by how many rows there are.

The four sources are merged by taking the smallest current user key and advancing every source
that is on it, so a key held in more than one family or namespace is one key. The merged stream
then feeds the *existing* halving-stride sampler unchanged: [ADR 0012](../adr/0012-split-key-selection.md)'s
three rules (a key that exists, strictly inside `(start, end)`, never the first sample) are about
the sample, not about where it came from, and none of them changes.

Byte order is preserved by the merge: the group encoding is memcomparable, so within a source the
decoded user keys ascend, and across sources a bytewise comparison of decoded user keys is the
same order the region's bounds are in.

## 4. Invariants

* **1/3 (crash safety, immutable files).** Nothing here writes. Both functions are reads over a
  `DbIterator`, and the split path they feed is unchanged: the boundary still goes through the
  Raft log, still applies in the batch that carries `apply_index`.
* **2 (checksums).** No format touched. No new bytes on disk or on the wire.
* **5 (region epoch).** Untouched: the epoch bump is `apply.rs`'s, and both halves still bump.
* **7 (engine and Raft byte-opaque).** The measurement learns *which namespaces and families a
  region spans* and nothing else. The `'r'` mapping is `esker_keys::prefix::raw_key`, the `'x'`
  mapping is `esker_txn::key::prefix`/`split`. No key layout is parsed in `esker-store`, and
  **no new accessor in `esker-keys` is needed** — everything this fix needs already exists there
  and in `esker-txn`, which is what `snapshot.rs` proves by already doing it.
* **9 (no panic on disk data).** A key in the `'x'` range that does not decode is a
  `ProtoError::internal`, matching the existing refusal for a key outside `'r'`.

## 5. Tests

Red first, at the store layer. **Not through SQL** — the defect is in the store's measurement.

1. `split.rs` unit: **a region of committed transactional rows is not zero bytes.** Write ~1 MB of
   `write`-CF records (`esker_txn::key::write` + a `WriteRecord` with an inline short value — what
   a committed SQL row *is* on disk) and assert `approximate_size` reports within a factor of the
   bytes written. Red today: `~0`.
2. `split.rs` unit: **a boundary is found in transactional data.** `choose_split_key` over the same
   region returns a legal boundary that is one of the written user keys. Red today: `None`.
3. `split.rs` unit: **the versions of one key are one key.** A region of few keys and many versions
   each yields no more samples than it has keys — the boundary of 10 keys × 100 versions lands
   near the 5th key, not near the 500th version. (Guards the dedup; without it the sampler is
   weighted by update frequency.)
4. `split.rs` unit: **a long value counts too.** A value over `SHORT_VALUE_MAX_LEN` lands in
   `default` under `'x'`; the size sees it and the boundary search sees its key.
5. `split.rs` unit: **a lock is not size.** A prewritten, uncommitted key adds `lock` bytes and
   those do not move the size. (Pins decision 1 of the ADR, so reversing it is a test change.)
6. `split.rs` unit: **the raw namespace still behaves exactly as it did.** The existing five tests
   stay, unchanged, and are the control.
7. `tests/split.rs` integration: **a region of transactional rows actually splits.** Drive
   `Store::serve_txn` (prewrite + commit, as `tests/txnkv.rs` does) past `TINY_SPLIT_SIZE`, then
   assert region count > 1, the halves tile `["", "")` exactly, **each half is non-empty** by
   `snapshot::key_counts`, and every committed key is still readable through whichever half owns
   it. Red today: it stays at one region until the deadline.
8. `tests/split.rs` integration: the parallel of 7 written in the `'r'` namespace already exists
   (`a_region_that_grows_past_the_threshold_splits`) and is the control.

Trap being avoided (brief §4): the assertions are the split *happening* and the size *changing*,
never that a function returned `Ok`.

## 6. Risks

* **A region now splits where it never did.** That is the point, but it means clusters carrying
  transactional data will start splitting on restart with this build, at 96 MiB. The split path
  itself is phase-4 code with its own tests; what is new is that it now receives work. Test 7 is
  the evidence it survives it.
* **A larger scan.** Up to four range scans instead of one, on a blocking thread, once per split
  attempt. Three of them are an immediate bound failure for a region that holds only one
  namespace. ADR 0012 already prices a split at one full scan.
* **`write` bytes are versions, not rows.** A region's size includes superseded versions until GC
  drops them, so a heavily-updated table splits earlier than its live row count suggests. That is
  the same direction `Db::approximate_size` already over-counts in, and it is the safe one.
* **Moving code out of `snapshot.rs`** could conflict with another lane. `crates/esker-store/**`
  is this lane's, and the move is three items and their imports.

## 7. What this will NOT do

* **Not touch `esker-keys` or `esker-proto`.** Nothing is needed there; §4 says why.
* **Not change the split threshold, the split protocol, the `Split` command, or the epoch rules.**
* **Not add a byte-weighted boundary.** ADR 0012 chose key-count and its consequence (uneven
  halves by bytes, corrected by the next size check) is unchanged.
* **Not measure the `'t'` or `'m'` namespaces as physical ranges.** They are namespaces of the
  *user* key space and reach the engine inside `'x'`; `snapshot.rs`'s `PHYSICAL_NAMESPACES`
  comment already says so and its guard test enforces it.
* **Not touch `esker-sql`.** A SQL table splitting is the *consequence* to be measured later; the
  defect and its test are the store's.
* **Not build the parallel fragment dispatch** phase-16 §10 item 3 asks for. Different lane.

## 8. ADR

[ADR 0073](../adr/0073-a-regions-size-is-the-data-families-it-spans.md) — which column families
and which namespaces count toward a region's size, and why `lock` does not. Claimed at `main`
`07e2f128`, where the highest is 0072.

## 9. Units

| # | What | Commit |
|---|---|---|
| 0 | This plan | `637e6b40` |
| 1 | Red: the size and the boundary of a transactional region | `bc942ab2` |
| 2 | Fix: `keyspace.rs`, and both functions read every family the region spans | `6748f327` |
| 3 | ADR 0073 + DESIGN §6 | `fa238b73` |
| 4 | Red→green: a region of transactional rows actually splits | `4f4564d2` |
| 5 | A version distribution the dedup test can actually fail on, + the gate's findings | `c3b14a3b` |

## 10. What was shown red, and against what

Each of these was run against code that did *not* have the fix, and the output is in the commit
message that carries the test:

* the five unit tests, against `bc942ab2`'s own code — `5000 committed rows of 200 bytes report
  0 bytes`, `choose_split_key -> None`;
* both integration tests, by putting `bc942ab2`'s `src/` back under this branch's `tests/` — `the
  store stopped at 1 regions, wanted 2` after 22 seconds of waiting, and `64 committed rows of
  200 bytes report 0 bytes`;
* `the_versions_of_one_row_count_once`, by taking the deduplication out of `Source::advance` —
  `the boundary landed at row 0 of 10`. Its first shape could not do this, and §5's item 3 is why:
  ten rows of a hundred versions each puts the 500th version inside row five, which is also where
  counting keys lands, so the test agreed with both answers.

`a_lock_is_not_a_regions_size` is the exception and says so in its own doc comment: it passed
before the fix, because everything reported zero.

## 11. What this covers, and what it still does not

The store splits a region of transactional rows, and every assertion in §5 is at that layer. The
consequence — **a SQL query against a table that spans several regions** — was named here as
uncovered and is now covered by `crates/esker-sql/tests/multi_region_rows.rs`, on a real cluster:
real `esker-pd`, real stores splitting on their own measurement, and the client routed through
`PdConn`, the resolver the binary builds. Nothing there places a boundary; the store chooses where.

**Proved.** Nine faces. Seven are compared byte-for-byte against a control cluster that *cannot*
split (`region_split_size: u64::MAX`) carrying the same DDL and the same rows: a full ordered scan,
`count(*)`, a point read at each end of the key space, a range with `ORDER BY` and `LIMIT` across
boundaries, a secondary-index scan whose index range spans regions, and a `GROUP BY` with `count`
and `sum`. The eighth is the client's refresh: a node whose region cache was built before a second
node grew the table past another split still reads every id exactly once. The ninth is a
transaction — a write set spread across every region commits whole, and one that rolls back leaves
every region as it was.

**What it found.** The first cross-boundary transaction could not commit at all:
`TxnClient::grouped` cut the write set by the region cache and the router retried *the group it was
given*, so a group that spanned a boundary could never succeed however often it was retried. The
fix re-cuts a refused group against the cache the refusal repaired.

**Still not covered**, and each is a face rather than a layer:

* a split that happens **while a scan is running**, rather than between statements;
* the one-columnar-fragment-per-region path of
  [ADR 0040](../adr/0040-the-engine-a-query-runs-on.md), which is the columnar arm and belongs to
  `docs/plans/phase-16-mpp.md` §10 item 3;
* replication: `multi_region_rows.rs` runs one store, because what it is about is routing across a
  boundary and a second copy of every region proves that no better.

### A split child elects from scratch, and it costs 62 ms a split — measured 2026-09-09

`Store::adopt_split` brings the child up with `start_peer` and `spawn_ticker` and **nothing else**:
no campaign, no leadership inherited from the parent, no term carried over. Every replica of the
child therefore begins as a follower and waits out an election timeout before anyone stands.

`how_long_a_split_child_has_no_leader` loads a table that splits under itself and samples every two
milliseconds, recording for each child the interval between first sighting and first leader:

```text
4,001 rows, 132 regions, 132 children measured
min 0 ms   median 62 ms   p90 77 ms   max 93 ms
0 refusals during this load
```

**The first instrument saw none of it, and that is part of the finding.** Sampling PD's region
records reported every one of 130 children as led at zero milliseconds — because PD learns of a
child at the next region heartbeat, 20 ms here, by which time the election is over. *An instrument
that reports zero and a system with no window look exactly alike.* The number above comes from
sampling the **stores'** own maps, where a child appears the instant `adopt_split` runs.

**Why it usually costs nothing.** Sixty-two milliseconds is well inside the client's own retry
budget for `NotLeader`, so a writer that arrives during the window waits and proceeds — this load
took zero refusals across 132 splits. It becomes visible only for a proposal that was **already in
the log** when leadership moved, which cannot be retried transparently because the client cannot
know whether it applied: that is the `40003` the batched loads hit at 37 and at 76 regions.

So the cost is not the median, it is the tail shape: **one ambiguous outcome per split that catches
a proposal in flight**, and a bulk load that is wide enough or fast enough to always have one in
flight will meet it on most splits.

### What was done about it, and the two ADRs the doing needed — 2026-09-09

**The child is campaigned by the store that led the parent**
([ADR 0094](../adr/0094-a-split-childs-leader-is-the-parents-leader.md)). `Store::adopt_split` asks
the child's peer to stand as soon as this store has it, and only where this store led the parent,
because two replicas campaigning at once is the split vote the whole thing exists to avoid. It is
still an ordinary election — the other replicas grant or refuse by the usual rules — so nothing
here fabricates leadership; what it removes is the waiting.

**What that ADR claims is the structure and not the clock**, and the measurement above is why:

```text
children led by the store that led their parent   without 35 of 63   with 29 of 29, and 130 of 130
quiet box    median 62-73 ms  ->  median 10 ms, and the 32,765 ms tail is gone
loaded box                        median 87 ms — larger than the before, structure unchanged
```

A duration is a statement about the machine; which store ends up leading is a statement about the
mechanism. So the gate test asserts leadership and the distribution is printed by an `#[ignore]`d
measurement beside it (`how_long_a_split_child_has_no_leader`).

**One campaign is not enough**, and the reason belongs to the split rather than to the election:
every replica creates the child when *it* applies the split entry, the leader applies first, and a
Raft batch for a region a store does not serve yet is dropped rather than refused. Campaigning once
left the median exactly where it was, at 63 ms; `Store::campaign_the_child` asks again for a handful
of ticks and stops at the first leader.

**And the campaign runs only after the region map has taken the child**
([ADR 0099](../adr/0099-one-core-per-region-per-store.md)). `RegionMap::apply_split` refuses three
things — a parent this store does not host, a child it already hosts, a parent whose start key moved
— and before 0099 the child's peer had already been built, registered with the driver pool and
handed a ticker by then, so a refusal left it **alive**: a peer campaigning, and going on
campaigning, for a region nothing on this store can serve. One region was found with
`campaigns_pre: 1108` against `campaigns_real: 91` in exactly that state
(`docs/plans/debts-v1.1.md` #9). `RaftPeer::start` now returns a reservation the caller commits once
the map has taken the peer, so every one of those refusals gives the region straight back, and the
ticker and the campaign both wait for the commit.

**Why a split child cannot lose its group to a learner**
([ADR 0085](../adr/0085-a-vote-is-not-granted-to-a-learner.md)). A child inherits the parent's
membership, learners included, and a learner can never reach a quorum — so a group that grants one
its vote loses a voter's vote and its leader for the term and elects nobody. `Raft::campaign` has
always refused to campaign when this node is not a voter in its own configuration;
`Raft::handle_vote_request` now applies the same test to the peer *asking*, which is the other side
of it. That guard is why the campaign above is safe to fire at a child whose membership is still
settling.
