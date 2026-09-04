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

[ADR 0055](../adr/0055-a-retirement-is-announced-before-the-record-that-names-it-goes.md). A fifth
prefix in the `raft` column family, `'R' ++ region_id` → the region being reclaimed, whole, written
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
