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
