# Debt wave, lane c6: what the inventory still owed, and what it had already been paid

The brief for this lane named eight items from the debt inventory taken at `4334403` — #2, #5, #21,
#6, #10, #18, #12 and the CLI group #14/#15/#16/#17 — and one standing order above all of them:
*verify every entry against HEAD before writing a fix, because three of seven recorded debts in an
earlier wave were not what the record said.*

Seven of the eight were not what the record said. They were **already closed**, by waves
[c3](debt-c3.md) and [c4](debt-c4.md), which took overlapping slices of the same inventory before
this brief was written. The verification pass is §1; it is the wave's first result and the reason
its scope is what it is. §2 onward is the work that was actually still owed.

## 1. The verification pass, item by item

Each paragraph says what is at HEAD (`c9afbd1`), with the evidence that settles it. No fix was
written for any item in this section, because writing one would have meant reverting somebody's.

### #2 — a retired region's data is never reclaimed → **closed in c3, with one crash window left**

`Store::retire_region` (`crates/esker-store/src/server.rs:767`) no longer carries the
`TODO(post-v1)` the inventory quotes. It stops the peer, destroys the Raft state, and then calls
`reclaim_retired_range`, which runs `snapshot::clear_range` over the region's range in every column
family and `reclaim_columnar_copy` after it, behind two gates — the membership must no longer name
a peer on this store, and no region this store still hosts may overlap the range.
[ADR 0034](../adr/0034-a-removed-peer-is-swept-and-its-range-reclaimed.md) is the decision,
`92a5add` the commit, and
`crates/esker-store/tests/retire.rs::a_removed_peer_reclaims_the_range_in_every_column_family` the
test. The brief's framing — "ADR 0034 sweeps the peer; this is the bytes" — reads the ADR's title
as if it stopped at the sweep; c3 did both halves, and §1b of its plan is the columnar tree.

What is *not* closed is narrower than the recorded debt and is §2 of this wave: the two steps are
not crash-safe as a pair. `raft_log::destroy` deletes the region's metadata record — the only
record of which range this was — in a synced batch, and `clear_range` runs after it. A crash
between them orphans that range's keys permanently, because nothing left on disk names them. The
method's own documentation calls this state "recoverable, and never served", which is true of
reads and false of disk: no code path ever comes back for those bytes.

### #5 — `WalSyncMode::Never` does not disable what it names → **closed in c3**

`WriteOptions` carries a `Durability` (`Policy` / `Durable` / `Buffered`), not a `bool`, so a write
can express "no opinion" and leave the database's mode something to decide;
`WalSyncMode::Interval` is read by a real background thread. `crates/esker-engine/src/options.rs`
documents both, [ADR 0036](../adr/0036-a-write-may-have-no-opinion-about-durability.md) is the
decision, and `docs/bench/debt-c3.md` measured it: 90.37 s before, 170.26 ms after, for the same
20,000 rows. The DESIGN §14 row the brief asked to be made honest — "store WAL sync mode | `Never`
— the engine adds no `fsync` of its own, so each request's `sync` flag decides" — already describes
the code.

### #21 — `Db::ingest` refuses any overlap, tombstones included → **open**, and this wave's §3

`DbInner::place` (`crates/esker-engine/src/db/ingest.rs:126`) still refuses three ways: against
another file in the same ingest, against the memtable, and against any level of the current
version. The module header's rationale is unchanged and correct as far as it goes. This is the one
item of the eight that HEAD still owes.

### #6 — the client exhausts its nine attempts against a moving region epoch → **closed in c3**

`Router::send` counts *failures* rather than attempts: a refusal that moved the region's epoch —
that is, one that taught the client something — resets the budget, and only an epoch change does,
because a `NotLeader` hint that moves no epoch is the loop the budget exists to stop.
`crates/esker-client/src/router.rs:383-399` documents it and names the exact symptom the inventory
recorded, `gave up after 9 attempts: region epoch does not match`. c3 §3 is the unit.

### #10 — the columnar copy is rebuilt by a full walk at every open → **closed in c4**

The run manifest carries an `applied` index at format version 2
(`crates/esker-store/src/columnar/runs.rs`), and an open replays the Raft log from there instead of
walking the region.
[ADR 0038](../adr/0038-a-run-manifest-names-the-index-its-runs-are-complete-to.md) is the decision,
with `crates/esker-store/tests/columnar_resume.rs` and the `ColumnarSlot::last_build` observable
that makes a resumed copy distinguishable from a rebuilt one. A version 1 manifest reads as
`applied = 0`, which is the full walk.

### #18 — tiered-read block size fixed at 4 KiB → **closed**

`CfOptions::block_size` is a `BlockSize`, not a `usize`: `Storage` resolves to `BLOCK_SIZE` (4 KiB)
on local disk and `TIERED_BLOCK_SIZE` (16 KiB) when the database's SSTs are tiered, and
`Fixed(bytes)` is never overridden — the same "a scalar cannot say *no opinion*" shape as #5, given
a name in advance this time. `d4570d7` is the commit and
`crates/esker-engine/tests/tiered_block_size.rs` is the test, including the non-default round trip
the brief asked for (`BlockSize::Fixed(BLOCK_SIZE)` on a tiered database, and
`Fixed(64 * 1024)` on a local one). DESIGN §14 already reads "4 KiB local, 16 KiB tiered".

### #12 — leaked objects after a failed `DeleteObject` need an offline reconciler → **closed in c4**

`esker sst-store reconcile <s3://bucket/prefix> --data-dir DIR` exists
(`crates/esker-cli/src/reconcile.rs`), listing objects the manifest does not name, dry-run by
default and deleting only under a flag. c4 §4 is the unit.

### #14/#15/#16/#17 — the CLI group → **all four closed in c4**

`Pd::Status` carries `OperatorStatus` for a *running* PD (`crates/esker-proto/src/pd.rs:362`), so
in-flight operators are visible (#14); `region ls` asks for pages via `Pd::ScanRegions` rather than
one `GetRegion` per region (`crates/esker-cli/src/region.rs:183`) (#15); `esker server` takes
`--region-split-size`, `--store-heartbeat-ms`, `--region-heartbeat-ms` and `--heartbeat-tick-ms`
(#16); and `esker bench` takes `--adopt-sst-store` (#17). c4 §5–§8 are the units.

### What the pass cost, and what it bought

Under an hour of reading, against a wave of eight fixes that would have collided with two merged
branches. The rule that produced it is worth restating in the form this wave found it: **an
inventory is a snapshot, and a brief cut from a snapshot ages at the rate the other lanes commit.**
The cheapest check is not "does the site still exist" but "does the *symptom* still exist" — for
five of these seven the site had moved, and for all seven the recorded sentence was still findable
in the source, in a doc comment explaining why it used to be true.

## 2. The retirement that a crash left half-done — the residual under #2

### What was there

Retiring a region is two durable steps, and the first one destroys the input to the second.
`raft_log::destroy` deletes the region's `'l'`, `'s'`, `'m'` and `'p'` records in one synced batch;
`reclaim_retired_range` clears the range afterwards. The `'m'` record is the only thing on this
store that says which **keys** the region was — its start and end live nowhere else — so a crash
between the steps leaves them in `default`, `lock` and `write` under no region, with nothing that
can name them again. Not the store, which hosts nothing covering them; not PD, which knows the
cluster's regions and not what any disk still holds; not a later retirement, which needs the record
that is gone.

The old comment called that state "recoverable, and never served". The second half is true and is
the safety property. The first half was not true of anything: no code path came back for those
bytes. So the window is not a delay, it is the leak ADR 0034 was written to end, surviving inside
the fix for it — one whole region's data, permanently, on a path **every rebalance takes**.

### The fix

[ADR 0056](../adr/0056-a-retirement-is-announced-before-the-record-that-names-it-goes.md) — the
commit that landed it says 0055, which is the number it was written under and which `main` took
first for the TLS options ADR; renumbered on the merge, per the rule that the later committer
moves. A fifth prefix in the `raft` column family, `'R' ++ region_id` → the region being reclaimed, whole, written
**in the same synced batch that deletes the record it describes**. It is the mirror of `'p'`, the
pending-snapshot record, whose own documentation had already made the argument: data arriving into
an unowned range was given a durable name in phase 4, and data leaving one was not.

Two consequences of "the batch destroys the input" shaped the rest:

* **gate 1 moves in front of the destroy.** Whether the membership still names a peer on this store
  is computed from a region record, and after the batch there is none. Its answer *is* the
  argument: `Some(region)` announces, `None` keeps the keys and announces nothing;
* **the open-time sweep runs *after* the peers are hosted**, because gate 2 asks the region map
  whether anything this store serves covers the range, and the map is empty until they are. Run
  first it would answer "nothing overlaps" for every announcement and empty a range under its
  owner — which is what the second red below actually did.

`finish_retirement` is the whole of what a retirement does to data — gate 2, the range, the
columnar tree, the announcement — as one blocking function with two callers, the live path and the
sweep. A second implementation of a delete is a second chance to get a delete wrong.

The announcement is dropped only when `clear_range` reports the range **provably empty**. That
turns the pre-existing "loud and harmless" failure into a retry: before this, a clear that failed
left the keys with nothing to come back for them, which is the same permanent leak by a different
route.

### The tests, and the two reds

`crates/esker-store/tests/retire.rs`, three new tests beside the wave-c3 one, 1.2 s for the file.

**Red 1 — the sweep does not run at open.** All three fail, and the one that matters says the debt
in its own words:

```
assertion `left == right` failed: a retirement interrupted by a crash left its range on disk for ever
  left: [("default", 1), ("lock", 1), ("write", 4)]
 right: [("default", 0), ("lock", 0), ("write", 0)]
```

**Red 2 — gate 2 does not run at open**, the announcement swept with an empty overlap list. One
test fails, and it is the data-loss direction:

```
assertion `left == right` failed: a stale retirement announcement emptied a range this store still serves
  left: [("default", 0), ("lock", 0), ("write", 0)]
 right: [("default", 1), ("lock", 1), ("write", 4)]
```

Six keys gone from a range the store was serving. That is what makes gate 2 load-bearing rather
than a comment, and it is why `a_stale_announcement_never_empties_a_range_this_store_still_serves`
announces a region whose range is one a live region covers — a split parent's stale record has
exactly that shape.

The third test, `an_announcement_whose_range_is_already_empty_is_simply_dropped`, is the crash that
lands *after* the clear and before the announcement goes, which is the ordinary case at open. It
runs on a store that hosts **nothing**, and that is the setup rather than a detail: on a store that
hosts a region every range is inside one, so gate 2 would refuse and the test would pass without
reaching the code it is about.

### One thing the first version of the test got wrong, and what it found

Driving the first step by hand left the region's peer running, and the restarted store came back
**hosting the region it had just been removed from**: a live driver writes its state record back
underneath the batch that deleted it. `retire_region` stops the peer before it destroys anything,
so the test now does too. The failure was the test's, and the reason it is written down is that it
is also a statement about the production order — the stop is not tidiness, it is a precondition.

### And the same thing with a real `SIGKILL`

`crates/esker-store/tests/retire_crash_kill.rs`, in the shape `crates/esker-engine/tests/crash_kill.rs`
established: the test binary re-executes itself with `--exact`, so the child is a test in the same
file and there is no second binary to keep in step. The child opens a store, commits eight keys,
performs step one of the retirement, **reports that it is durable**, and waits to be killed; the
parent kills it and starts the store again.

The kill is aimed rather than random, and the file says why: the window is two adjacent synced
writes, so a signal thrown at a running retirement lands in it about never, and a test that reaches
its subject about never passes for other reasons. What is arranged is *when* the process dies; that
it dies, and what it leaves on the filesystem, is real.

It is not a duplicate of the simulated crash. That one is the precise instrument — it can stop
exactly between the steps — and it proves the recovery logic. This one proves the half about the
process: that the announcement is on the platter and not in a buffer when the process ceases to
exist, and that `Store::open` of a database a killed process left behind finishes the job. Red
without the sweep, with the count the crash really left:

```
assertion `left == right` failed: a retirement a SIGKILL interrupted left its 8 keys on disk for ever
  left: [("default", 8), ("lock", 0), ("write", 0)]
 right: [("default", 0), ("lock", 0), ("write", 0)]
```

## 3. `Db::ingest` refused any overlap, tombstones included

Inventory #21, the one item of the brief's eight that HEAD still owed.
`crates/esker-engine/src/db/ingest.rs`.

### What was there

`DbInner::place` refused three ways — against another file in the same ingest, against the
memtable, and against **any level** whose files' ranges overlapped the candidate's — and each
refusal named the same reason: a file built elsewhere carries another database's sequence numbers,
so an overlap has no defensible answer.

The reason is right. The rule was a proxy for it, and a badly-fitting one.

### The rule, which is about keys and not about ranges

**An ingest is refused exactly when the file holds a user key the column family already has an
entry for** — a value, a point tombstone, or a range tombstone covering it.

Sequence numbers only decide anything between two entries under the **same** user key. For entries
under different keys the question is never asked: a read resolves one user key at a time, finds
exactly one version of it, and the numbers decide nothing. Range disjointness is a sufficient
condition for key disjointness, not an equivalent one.

And in this system the gap between the two is the ordinary case, not a corner. `esker-txn` encodes
the MVCC version **into the key** — `'x' ++ enc(user_key) ++ !ts` — so two versions of one row are
two distinct engine keys, and two files can interleave completely across a range while sharing not
one key. A bulk load of a time range for rows that already exist has exactly that shape: it
overlaps everything and collides with nothing. Every one of those was refused.

### The structural constraint is not a reason to refuse

Levels 1 and below are sorted runs — their files must be range-disjoint or a seek cannot binary
search the level — and L0's files overlap by construction. So a file whose keys are free but whose
range is not is not a refusal, it is a **placement**: it goes to L0 and a later compaction sorts it
downward. `place` therefore stops returning an error and returns the deepest level whose ranges
leave room, which is 0 when none does. A genuinely disjoint bulk load still lands deep.

### What the check consults, and the duplication it removed

The read path's own list. `DbInner::merge_sources` was extracted out of `Db::iter` — the cursors
over every memtable and every level, plus the range tombstones, from one pinned version — and both
callers now use it. An ingest that consulted a different set than a read would refuse ingests that
are safe or, far worse, allow one whose keys a reader can already see. It returns a `MergeSources`
rather than a tuple because the three parts are one snapshot at one instant: split apart, a caller
can hold cursors over files a dropped version has let a compaction delete.

The walk is over the *candidate's* keys, seeking the merged cursor to each, which costs a seek per
distinct key in the file rather than a scan of the range — and the range can be the whole column
family while the file is small, which under MVCC keys is the ordinary case.

Point tombstones do **not** free a key, and the module says so: a delete is an entry under its key,
and the sequence number would still have to choose between the delete and the ingested value.
Range tombstones are checked separately from the cursors, because a range delete hides keys the
merged run has never seen (ADR 0017) — a cursor would show nothing and the conflict would be
invisible.

### The property test, and the two reds

`crates/esker-engine/tests/ingest_overlap.rs`, two properties. Both sides draw keys from one
24-wide space, so ranges overlap heavily and key sets collide often; a generator that partitioned
the space would have tested the old rule and never produced the case this unit is about.

1. **the decision matches the rule**, both directions — `Ok` iff the ingested key set is disjoint
   from every key the family has an entry for and no range tombstone covers one;
2. **an accepted ingest reads back as the union**, and a refused one changed nothing, asserted per
   key across the whole space.

Property 2 is what makes property 1 worth having: widening a rule is easy to do in a way that
passes every "is it refused" test and answers the wrong value afterwards, and a wrong answer is a
*readable* one.

Red in both directions:

* **too permissive** (nothing is ever a conflict) — `ingesting {2} into a family holding {2}: the
  rule says free=false, ingest said None`;
* **too strict** (the pre-c6 range rule, restored) — both properties fail, and
  `interleaved_ranges_that_share_no_key_are_all_accepted` fails outright, which is the case the
  widening exists for.

`crates/esker-engine/tests/checkpoint.rs`'s two refusal tests both use genuinely shared keys and
stay red, as they should; one assertion moved from the old message to the new one, which names the
colliding key.

### One seek for the disjoint case, so the widening does not cost a bulk load

The rule costs a seek per distinct key in the candidate file, where the old range check cost one
lookup per level. On the path ingest exists for — a phase-6 bulk load of a large file — that is a
regression, and shipping the widening without answering it would trade a refusal for a cliff.

So the old range check is kept, for what it is actually good for: a cheap **sufficient** condition
rather than the rule. One seek positions the merged cursor at the first key of the column family at
or after the candidate's smallest; if there is none, or it sorts above the candidate's largest, and
no range tombstone's bounds reach the candidate's range, then no key of the file can be held and
the walk is skipped entirely. A bulk load into fresh key space therefore pays one seek, as before.

It is a conservative test in both of its halves — a gap *between* two tombstones is not exploited,
and a tombstone ending exactly at the candidate's first key falls through to the walk rather than
being reasoned about — because a short-circuit that is wrong is an ingest that is allowed, and the
walk it falls through to is merely slower. The property test is what guards it: removing the
tombstone half alone turns the decision test red, since `range_del` is one of its generated inputs.

### A debt found while writing the rule, and not fixed here

**An ingest is visible to snapshots taken before it.** `ingest` raises the database's sequence
number *above* the file's, so later writes sort above the ingested data — but it does not give the
file a number of this database's own, so an ingested entry keeps a number from a numbering this
database never issued. A reader holding a snapshot older than the ingest can therefore see the
ingested keys, because their numbers may fall below its own.

It is unaffected by the rule above — it is about *when* an ingest becomes visible, not about which
of two versions wins — and closing it means a per-file global sequence number in the SST footer,
which is a format change with a golden test and an ADR. Recorded here, and marked in the module
header, rather than fixed inside a unit about overlap.

- **Site:** `crates/esker-engine/src/db/ingest.rs`, `Db::ingest`'s `raise_seqno_above`.
- **Size:** medium; a format change, so it needs the human (`CLAUDE.md` §"Ask before doing").

## 4. The `TODO(phase-N)` markers: eight had outlived their phase, four had not

A marker naming a phase that shipped is worse than no marker: it tells a reader the code is
unfinished when the finishing is three phases behind them, and it hides the ones that are real. All
of them in this lane's crates were checked against the code they point at. Two `TODO(post-v1)`
markers are left alone — they name a version rather than a phase that has passed
(`esker-store/src/server.rs` on read-only replicas, `esker-store/src/snapshot.rs` on
sequence-number rewriting, which is §3's debt seen from the other side).

### Outlived their phase — the marker is gone and the doc says what is there instead

| Site | The marker said | What is actually there |
|---|---|---|
| `esker-store/src/regions.rs`, `RegionState` | `TODO(phase-4b)`: a split replaces two entries under one lock | `RegionMap::split`, from `Store::adopt_split`; a membership change goes through `RegionMap::replace` |
| `esker-store/src/regions.rs`, `RegionMap::remove` | "Nothing in 4a calls it; `TODO(phase-4c)` is the `RemovePeer` operator" | `Store::retire_region` calls it, and race 3's hazard is closed at both ends ([ADR 0056](../adr/0056-a-retirement-is-announced-before-the-record-that-names-it-goes.md)) |
| `esker-store/src/meta.rs`, `stage_region` | `TODO(phase-4b)`: a split writes both halves' records in one batch | `crate::peer`'s `stage_split` does exactly that |
| `esker-store/src/raft_log.rs`, `PersistedState::conf_state` | "Nothing writes this after `open` … see the `TODO(phase-4)` on `RaftLogStorage::snapshot`" | Both halves stale: `stage_compact` writes it at every truncation, and the `TODO` it points at is gone — a dangling cross-reference |
| `esker-store/src/peer.rs`, `applied_conf` | `TODO(phase-4c unit 5)`: moved by applying a `ConfChange` | `apply_conf_change` moves it, and nowhere else |
| `esker-cli/src/server.rs` | `TODO(phase-4b)`: a split makes the list grow while the server runs | Splits exist; the line still only prints what was found at open, which is what the comment now says |
| `esker-sim/tests/raft_snapshot.rs` | `TODO(phase-3d)`: the harness has no action that adds or removes a voter | `crates/esker-sim/tests/raft_membership.rs` is that harness, with the membership in the observation and a quorum per configuration |

One of these was not merely stale but **wrong**, and it is worth its own line.
`esker-store/src/meta.rs`'s `stage_removal` said "the `RemovePeer` operator is what calls this".
`RemovePeer` reaches `raft_log::destroy`, which deletes the metadata key alongside the region's
log, state and pending-snapshot records in one synced batch — and it has to, because a record
removed without them leaves a log for a region nothing hosts. A reader following that comment to
find the retirement path would have found the wrong function. Its doc now says what it is for and
who uses it.

### Still open — a debt entry each, and the marker re-tagged `TODO(debt-c6 #n)`

**#1 — one timer per region is one task per region.** At fifty regions that is fifty timers where
one wheel would do; it is the same sharding decision the apply workers already made and belongs
with it.
*Site:* `crates/esker-store/src/server.rs`, `Store::tickers`. *Size:* medium.

**#2 — a reverse scan is routed by its exclusive upper bound.** `routing_key` answers `start` for
every `Scan`, and for a reverse scan `start` is the **exclusive** upper bound to walk down from. A
`start` sitting exactly on a region boundary therefore routes to the region *above* the one holding
every key the scan should return, and the answer is an empty page — which a caller cannot tell from
the end of the range. Reverse scans are reachable: `RawClient::scan_reverse` and the `reverse` flag
on `RawKvReq::Scan` and the txn scan.
*Site:* `crates/esker-client/src/wire.rs`, `routing_key`. *Size:* small; a test needs two regions
and a boundary-aligned bound.

**#3 — the peer a request is aimed at ignores which peer just failed.** `RegionCache::target`
answers the cached leader or the first peer in the list, so a down store is asked twice inside one
retry budget. The budget itself was fixed in wave c3 — it counts failures rather than attempts —
which makes this the remaining half: the retries are now the right *number*, aimed the same way
each time.
*Site:* `crates/esker-client/src/region_cache.rs`, `RegionCache::target`. *Size:* small-medium.

**#4 — the client's store book is a fixed list, not PD's.** `TcpStores` is handed addresses at
construction and learns no store it was not given, so a store added to the cluster is unreachable
until the client is rebuilt. The marker predates PD existing at all; PD exists, and this is still
not wired to it.
*Sites:* `crates/esker-client/src/transport.rs` (module header) and
`crates/esker-client/src/tcp.rs`, `TcpStores::connect_with`. *Size:* medium. **Owner:** this is the
`pdha` lane's territory — the markers are re-tagged and nothing else in `tcp.rs` was touched.

## 5. `docs/DESIGN.md` against the code, for the engine, the store and the client

`CLAUDE.md`: *"If code and DESIGN.md disagree, fix one of them in the same change — they must never
drift."* Four disagreements, and in all four the code was right and the document was describing a
version of the system that two waves had already replaced. Each is one that would mislead a reader
into writing wrong code, not a wording preference.

**§4.1 — `WriteOptions { sync: bool }`, "defaults to `true`".** Replaced in wave c3 by a
`Durability` with three states ([ADR 0036](../adr/0036-a-write-may-have-no-opinion-about-durability.md)),
and the whole point of that change was the state a `bool` cannot express: **no opinion**. A reader
following §4.1 would have written `WriteOptions { sync: true }` against an API that has no such
field, and, worse, would have taken from it the belief the ADR exists to correct — that a write
either demands durability or forbids it, so a database-wide policy has nothing to decide. §4.2's
`sync = false` sentences move to `Buffered` with them, and its `wal_sync_mode` line now says that
`Interval` is a real background thread and what `Never` still syncs.

**§6, the snapshot receive — "`Db::ingest` refuses any overlap including tombstones (§4.1)".** True
until §3 of this wave; the rule is now about a shared *key*. The sentence is load-bearing where it
sits — it is half the argument for why a snapshot ships key-value pairs instead of linking files —
so it is corrected rather than deleted, and the argument survives intact: a receive retried after a
partial one collides with exactly the keys the partial one left. The same paragraph in
`esker-store/src/snapshot.rs`'s module header says the same thing and was corrected with it.

**§6, the snapshot receive — "Only into a range this store holds nothing in… A peer that already
has data is refused and stays behind."** This is the one that was not merely out of date but
inverted. `receive_raft` **replaces** a region this store already hosts: it retires the old peer so
nothing is driving the region, then empties and refills the range. The comment in the code says
why, and names what the document still describes as the design: *"Refusing it was 4c's limitation
and phase-4 acceptance showed it is not an edge case: a peer that falls behind its leader's
compaction boundary can only be repaired this way, and until now it could not be repaired at all."*
A reader of §6 would have concluded that the repair path this system depends on does not exist.

**§10, the client's retries — the budget's unit.** §10 said retries are bounded by a budget of 8
and a deadline, which is still true and is no longer the whole rule. Wave c3 made the budget count
**failures rather than attempts**: a refusal that moved the region's epoch resets it, and only an
epoch change does. Both halves matter to anyone reading the section to predict client behaviour —
without the first, nine redirects through a splitting region look like a client bug; without the
second, "it resets on a redirect" reads as a budget that cannot expire.

No disagreement was found in §4.3–§4.9, §6's other bullets, or §10's remaining rules; the §14
defaults table was checked against the constants it names and matches, including the two rows this
wave brushed against (`store WAL sync mode` and `SST data block … 4 KiB local, 16 KiB tiered`).

## 6. The election flake: `status()` is not a barrier, and one helper never got the memo

Escalated by `h1`: `peer::tests::a_proposal_orphaned_by_a_step_down_is_answered_unknown_rather_than_hanging`
failed 2 of 3 full-workspace container runs and passed 5 of 5 alone, panicking at
`crates/esker-store/src/peer.rs:2628`.

### The line number was the finding

2628 is not in the test. It is `panic!("the peer never took office")` inside
`lead_without_a_quorum`, the **setup helper** both that test and
`a_proposal_a_retire_jumps_past_is_answered_unknown` use to drive an election by hand. So nothing
about orphaned proposals was failing; the peer never became leader, and the test never reached its
subject. `a_proposal_a_retire_jumps_past_is_answered_unknown` had its own sighting recorded on
09-03 — the same helper, seen from the other test.

### What it was

`RaftPeer::settled`'s documentation had already written this bug down, one screen above the helper:

> Every other query is answered inside the driver's handling of a message, which runs **before the
> batch is driven**, so awaiting one of those and then looking for the messages your own input
> produced can find nothing.

`lead_without_a_quorum` did precisely that: `take_sent()` for the core's `RequestVote`, grant it,
`tick()`, `status()`, repeat — where `status()` is answered during *handling* and the drive is what
hands a message to the transport. It is the defect wave `5662300` fixed in the two tests beside it
(`a_lone_voter_applies_its_own_proposals`,
`a_message_is_never_sent_before_its_entries_are_durable`) and did not fix here.

### Red on demand, and the instrumentation is what made it a diagnosis

The panic said only "the peer never took office". Instrumented with ticks, grants, role and term,
and run ten times with twenty-four spinning threads on the box, it failed **3 of 10** in two shapes:

```
the peer never took office: 400 ticks, 0 votes granted, and it is PreCandidate in term 0
the peer never took office: 400 ticks, 25 votes granted, and it is Candidate in term 1
```

**Zero grants in four hundred ticks is the whole mechanism in one number.** It is not a slow drive:
the driver handled four hundred ticks and never drove once, so no message ever reached the auditor
and the loop had nothing to answer. The 25-grant shape is the same starvation one layer up — grants
landing about once per sixteen ticks, each answering a term the candidate's next timeout had
already left behind.

### The fix, and what replaced the 400 tries

`peer.settled().await` after the tick. With it, every iteration is one fully driven tick and the
election becomes **deterministic**: exactly **20 ticks and 2 grants**, measured six times, three of
them under the same twenty-four-thread load. So the loop is bounded at 40 rather than 400, and the
grant count is asserted to be exactly two — one pre-vote, one vote.

That assertion is the bug stated as a number. Two means one uninterrupted campaign; the failures
were 0 and 25, and neither is reachable by a slow machine alone. It is an assertion about
synchronisation rather than about speed, and it stops four hundred tries from hiding the next
occurrence behind sheer number of attempts.

| | quiet | 24 spinning threads |
|---|---|---|
| before, bound 400 | passed | **3 of 10 failed** |
| before, bound 40 | passed 10 of 10 | **9 of 10 failed** |
| after | passed | **20 of 20 passed** |

### What it is not, stated plainly

**Not a defect in `esker-store`'s source, and the report should not be read as one.** In production
the clock is `spawn_ticker` at 100 ms, so ticks cannot outrun the driver the way a spin loop can;
the hazard needs a caller that ticks in a tight loop, which is a test. The fix is therefore in test
code, and the honest limit of it is that the failure stays load-dependent: with the barrier gone it
is green 10 of 10 on a quiet box. What is deterministic is the *success* path — 20 ticks, 2 grants,
invariant under load — and that is what the new assertion pins.
