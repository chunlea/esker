# Debt wave, lane c3: the recorded engine, store and client debts

Seven items from the inventory taken at `4334403`, in the order the brief gave them. Each one is a
deterministic repro first, shown red against the old code, then the fix, then the same repro green.
Nothing here is documented instead of fixed.

## 1. A retired region's data is never reclaimed — and nobody tells a removed peer

Inventory #2. `crates/esker-store/src/server.rs`, `retire_region`, carrying `TODO(post-v1)`.

### What the repro found, which is not what was recorded

The debt as written is a disk leak: `retire_region` stopped the peer and destroyed its Raft state
and left the region's keys on disk, in all three column families, under no region. Its comment
blamed the engine for having no range tombstones, which stopped being true in phase 5
([ADR 0017](../adr/0017-range-tombstones.md)); `snapshot::clear_range` had been doing this exact
operation, verification pass and all, since then.

`crates/esker-store/tests/retire.rs` builds the recorded shape — two stores, a region placed on the
second by `AddPeer`, four committed rows, one uncommitted prewrite and one `RawKV` put so `default`,
`write` and `lock` all hold keys, then `RemovePeer` — and it failed **before it reached the
assertion it was written for**:

```
timed out waiting for the second store to stop hosting the region
```

with the leader's own log showing the change applying on the leader and nowhere else:

```
a region's membership changed region_id=1 index=14 peers=1 removed_self=false
an operator applied region_id=1 node=2 kind=Remove
```

Store 2 never logged a membership change at all. **A removed peer is never told it was removed.** A
configuration change takes effect when it is *appended* — §4.1, and `esker-raft` asserts it in
`conf::tests::a_leader_removed_by_a_committed_change_steps_down` ("applied at append") — so the
instant the leader appends `Remove(n)` its `Progress` has no entry for `n`, and the entry that says
`n` is gone is the first one `n` is not sent. The commit needs a quorum of the *new* configuration,
which `n` is not in. It is not a race and it does not depend on the group's size: removing peer 3
of `{1,2,3}` commits on `{1,2}`, and peer 3 hears nothing.

`retire_region` had exactly one caller — a peer applying the conf change that removed it — so on
the operator path **it never ran at all**. That is why the leak was never noticed: the retirement
that was supposed to leak was not happening either. And the leak is the least of what it leaves:
the peer stays, with no leader, campaigning for ever against a group that has replaced it, and a
restart does not heal it, because `docs/plans/phase-4.md` §6 race 3 tombstones a record whose peer
list does not name this store and this record still names it.

### The fix

[ADR 0034](../adr/0034-a-removed-peer-is-swept-and-its-range-reclaimed.md), in two halves.

**The sweep.** `Store::sweep_orphaned_regions`, on the store-heartbeat schedule beside
`promote_caught_up_learners` and for the same reason that one is there. A region whose peer has had
no leader for 50 consecutive rounds is asked about once, and PD's answer retires it only when it is
about the same region id, carries a strictly greater `conf_ver`, and names no peer on this store.
The leaderless count is a throttle; all the safety is in those three conditions, and every other
answer — including no answer — keeps the region.

**The reclamation.** `retire_region` destroys the Raft state and the region record first, synced,
and then empties the range through `clear_range`, which covers `default`, `lock` and `write` across
both physical namespaces. A crash between the two leaves keys under no record, which is where this
store used to live permanently; the other order would leave a record pointing at a half-emptied
range.

Two gates in front of the delete. The membership must name no peer on this store — and the record
that answers that has to come from the caller, because the store the sweep found holds the record
from *before* the change and always will. This is not theoretical: with the sweep working and the
membership still read from the map, the run said

```
a region was retired while its record still names a peer on this store; its range is left alone
```

and the test failed on the reclamation assertion. The second gate is that no region this store
still hosts may overlap the range, which is what catches a stale record — a parent narrowed by a
split would otherwise delete its child's keys.

### The test, and the two reds

`crates/esker-store/tests/retire.rs::a_removed_peer_reclaims_the_range_in_every_column_family`.
1.2 s, three consecutive runs, and it asserts both halves of the rule: the shed store's range is
empty **per column family**, and the store that still hosts the region is unchanged, counted per
family and read back through the front door. Counting the families before the removal is what makes
"empty afterwards" mean *emptied* rather than *never filled*.

Red twice, each red isolating one half of the fix:

1. without the sweep — `timed out waiting for the second store to stop hosting the region`;
2. with the sweep and without the authoritative membership — `timed out waiting for the shed range
   to be reclaimed`, with the gate's own warning above it.

The probe the assertions are built on is new: `esker_store::snapshot::key_counts` answers how many
keys each shipped column family holds in a range. `clear_range`'s own emptiness check is the one-bit
version, and one bit cannot tell a "two of three column families" miss from a clean sweep — which is
the shape [ADR 0032](../adr/0032-a-snapshot-carries-every-column-family.md) found in the snapshot
stream this week.

## 1b. The columnar runs go with the region

Unit 1 left this owed and written down, which under this wave's standing order is a bug rather than
a deferral. A region's data is not only its keys: a columnar learner writes an immutable tree of run
files under `<data_dir>/columnar/<region_id>/`, swept by a manifest of its own, **beside** the
engine rather than inside it — so nothing the engine reclaims can reach it, and a retired region
left one behind on every store that had ever held a columnar copy of it.

It could not be built out of what `FileSystem` had. `delete` takes a file, `list` takes a directory,
and nothing in the trait says which a path is, so a recursive removal written against it cannot
take the first step. `FileSystem::remove_dir_all` is therefore new, with **no default**, which is
what makes each of the five implementations state its own answer rather than inherit a wrong one:
`LocalFileSystem`, `MemFileSystem`, `FaultFileSystem`, `TieredFileSystem`, and the `CrashFs` in
`esker-engine`'s own version test.

Three decisions in it are worth the words:

* **idempotent**, because the caller may be running after a crash interrupted it, so "already gone"
  is the ordinary case and not a failure;
* **the directory goes too**, not just its contents, because "does this exist" is how a caller asks
  whether the reclamation happened;
* the in-memory implementation matches by **ancestry, not string prefix**, so `columnar/12` is not
  swept up beside `columnar/1`. That is one `starts_with` away from a silent cross-region delete.

The fault injector deliberately does not count or fault it, on the same terms as its `open`: this
removes files of a region the cluster has already taken away, every caller logs a failure and
carries on, so an injected fault would exercise a `warn!` rather than a recovery path.

On the store side the slot in `Store::columnar` is dropped **before** the tree — it owns the
`RunSet` that owns the manifest, and a fragment arriving mid-removal would otherwise reopen the
table and write a manifest back into a directory being deleted — and the removal runs
unconditionally after the range clear rather than chained onto its success: the copy is derived from
the range and belongs to a region that is gone either way. The parent directory is `fsync`ed, since
a removal is not durable until the directory that held the entry is.

Asserted in the same test as the range, and in both directions:
`a_removed_peer_reclaims_the_range_in_every_column_family` plants a copy for the region being shed
**and one for a region that is not**, and the second is what says the removal is per region id
rather than a sweep of `<data_dir>/columnar`.

## 2. The `receive_raft` snapshot ask races the conf change — and the recorded fix is worse

Inventory #7. `crates/esker-store/src/server.rs`, `send_snapshot`.

### The repro, and the fix that failed it

`crates/esker-store/tests/snapshot.rs` holds the ordering still by adding a **voter on a store that
does not exist**: the core takes the change when it appends it, the commit then needs a quorum of
the new configuration — which the absent peer is half of — so it never commits, never applies, and
the record never moves. The core has peer 2 for the rest of the process and the applied record does
not. Red immediately, with the recorded sentence:

```
InvalidRequest { detail: "peer 2 is not a member of region 1 and may not have a copy of it" }
```

`docs/plans/phase-8-learner.md` §close bullet 4 names the fix: *"the sender could check the core's
membership rather than the applied record."* Implemented exactly as written, that test goes green
and **`tests/promotion.rs` fails 3 runs of 3**, with a learner stranded for the whole 30 s deadline.
Bisected by reverting that one condition: 2 of 2 green, and eight seconds faster.

The mechanism is in `host_region` (`server.rs:489`). A snapshot's header carries `source.region` —
the *applied* record — so serving on the strength of the core's membership ships a header that does
not list the peer receiving it. The receiver writes that record, writes every byte of the region,
and then `host_region` declines to start it, correctly, because a record that does not name this
store is one it must not serve. It logs a warning and returns `Ok(())`. The transfer "succeeds",
nothing is hosted, the leader announces again, and it repeats. A retry that cost one heartbeat
interval became a stall with no end — the same shape phase-4 kept finding, produced this time by
the fix for it.

### What landed instead

[ADR 0035](../adr/0035-a-snapshot-ask-waits-for-the-record-that-names-its-peer.md). The core's
membership decides whether the caller is a **stranger**; the record decides **when** it is served.
A peer the core has and the record does not is held for up to 500 ms and served the moment the
record lists it — which is when the change has committed and applied, and therefore when it can no
longer be rolled back by a new leader. That is a stronger check than reading the core rather than a
more convenient one: the ask the core alone would have admitted is exactly the one that must not be
admitted yet, because a region shipped to a peer a rollback removes is a copy of a range with no
owner.

The bound is what keeps a design mistake from becoming a deadlock: adding a *voter* to a group that
then needs it for quorum cannot commit until that voter has the region, which is what the wait is
trying to give it. Nothing here does that — `AddPeer` adds a learner and promotes it, and a
learner's addition commits on the existing voters alone — but a wait with no end would turn the day
somebody tries into a hang instead of a retry.

And the receiver refuses a header whose record does not name it, **before a byte is written**. Same
retry, nothing left durable, and the silent version of this failure becomes a loud one — including
against a sender at an older version, which nothing in this build can otherwise cover.

### The test, and what it can and cannot assert

`a_peer_the_core_has_and_the_log_has_not_committed_is_waited_for_and_then_refused` asserts three
things, all deterministic: the ask is **refused**, the refusal says the change *has not applied*
rather than that the peer *is not a member* — which is the whole assertion, since only the second
sentence can come from reading the applied record and stopping — and it took at least 400 ms, which
says the sender waited rather than refusing on sight. A stranger, by contrast, is refused in under
400 ms.

Writing it turned up something about the harness worth keeping. **A refusal arrives in one of two
places**: a store that refuses before the stream is opened fails the call, and one that refuses
*after* — which is what a waited refusal does — sends the error as the stream's first chunk. The
first version of this test read only the failed call and reported a refusal as a snapshot that had
been served. `snapshot_refusal` now reads both, and `snapshot_refused` is written in terms of it,
so the older tests in the file gain the same coverage rather than keeping their own half of it.

**What it does not assert is the served-after-waiting path, and that is a real gap.** The window it
would need — a conf change committed but not yet applied *here* — closes at the speed of one apply,
and every way of holding it open that this lane could construct also stops the change committing,
which is the thing being waited for. The end-to-end evidence for that path is
`tests/promotion.rs` and `a_placed_columnar_learner_holds_what_the_leader_holds`, which are the
tests that failed 3 of 3 under the naive fix and are green under this one.

## 3. The client spends its retry budget on progress

Inventory #6. `crates/esker-client/src/router.rs`, the budget check; the budget itself in
`retry.rs` ("eight is nine calls in total").

### What the repro showed, which is the whole argument

`a_moving_epoch_burns_the_budget_with_most_of_the_deadline_unspent` scripts ten refusals, each
teaching a strictly newer epoch than the last — a region that keeps moving, which is what a
saturated box splitting and rebalancing actually produces. Against `FakeTransport` and `FakeClock`,
so there is no wall clock and no socket in it. It reproduces the recorded failure exactly:

```
Err(RetriesExhausted { attempts: 9, source: EpochNotMatch { ... version: 10 } })
gave up after spending 2.266s of a 10s deadline, with 9 calls made
```

**2.27 seconds of a ten-second deadline.** The client did not run out of time; it ran out of
attempts, with 77% of its own budget for the call unspent, and handed the caller a failure for a
call that had seconds left to succeed in. That is the evidence the brief asked for, and it settles
the question it posed: the budget is not too small and the deadline is not too long — the two are
counting different things and the wrong one is deciding.

Every one of those nine refusals was **progress**. An `EpochNotMatch` carries the regions that
replaced the one the client asked about, so each attempt leaves the cache more correct than it
found it and the next is aimed better. Counting them against the same budget as a store that will
not answer treats "you learned something, try again" as "this is not working".

### The fix

The budget counts **consecutive attempts that taught this client nothing**. After each repair the
router asks `learned_a_newer_epoch`: does the cache now hold a newer epoch for this key than the
attempt that just failed was addressed with? If so the budget resets; if not it is spent as before.

Three things about it are deliberate:

* **`Epoch::is_stale_against` is the comparison**, which is the same one the *store* uses to decide
  the request was stale in the first place. The two counters move independently — a split bumps
  `version`, a membership change bumps `conf_ver` — so "newer" is not one comparison, and asking
  the shared predicate is what stops the client's idea of progress drifting from the store's idea
  of staleness.
* **Only the epoch counts.** A `NotLeader` hint moves no epoch and does not reset the budget:
  chasing a leader around a region that is not changing is exactly the loop the budget exists to
  stop. This is the conservative half, and it is why every existing budget test is untouched.
* **Reset rather than decrement.** A call that keeps being given fresher routing keeps its whole
  budget for the moment it stops being given any.

The backoff schedule is unchanged and still counts total attempts, so a pathological store cannot
be hammered: it is met with the same exponential curve to the same 2 s ceiling.

### The other half, which would make the fix worse than the bug if it were missing

"Progress does not spend the budget" is only safe while something else is counting.
`an_epoch_that_never_settles_ends_at_the_deadline_and_says_so` scripts two hundred ever-newer
epochs and asserts the call ends in `DeadlineExceeded` having spent at least half its timeout. The
caller still gets an answer inside its call timeout, and the answer now says it ran out of *time* —
which is true and actionable — rather than out of attempts, which was not.

All 22 tests in `crates/esker-client/tests/retry.rs` pass, including the three that pin the budget
for the cases that are not progress: `a_redirectable_error_is_retried_exactly_the_documented_number_of_times`,
`a_read_that_never_gets_an_answer_exhausts_the_budget_and_says_why`, and
`the_deadline_stops_a_retry_storm_before_the_budget_does`.

## 4. `WalSyncMode::Never` did not disable what it names — and `Interval` was never read at all

Inventory #5. `crates/esker-engine/src/db/write.rs`; the mode in `options.rs`.

### The repro, which is a counter and not a stopwatch

`crates/esker-engine/tests/wal_sync.rs` puts a filesystem that counts `sync_data` per path under
the engine and writes twenty rows. That is the only way to settle a question about a durability
knob: a benchmark can be slow for a dozen reasons, and the recorded evidence for this debt was a
benchmark plus a stack sample. Four assertions, and three were red:

* `per_write_syncs_every_group` — **green**, the control: twenty puts, twenty syncs. The counter
  works.
* `never_does_not_sync_a_write_that_only_took_the_default` — **red**: *"a mode that names itself
  `Never` synced 20 default writes"*. Twenty, not zero.
* `interval_syncs_in_the_background_without_the_writer_waiting` — **red**, and red at the first
  assertion: the writer waited for every sync itself.
* `a_clean_close_syncs_what_the_mode_deferred` — **red** at its *precondition*, for the same reason.

### The mechanism, and why the recorded diagnosis was wrong

One line decided everything:

```rust
let sync = options.sync || self.options.wal_sync_mode == WalSyncMode::PerWrite;
```

An **OR**, so the mode could only ever *add* syncing, never remove it. And
`WriteOptions::default()` was `sync: true`, which `Db::put` and `Db::delete` take — so no write in
this repository ever left the mode anything to decide. The root cause is a missing state, not a
wrong comparison: a `bool` can say "durable" and "not durable" but not **"no opinion"**, and
without a write the policy is entitled to decide, a policy is decoration.

`docs/bench/columnar-learner.md` read a stack sample of this — 2075 of 2114 frames in
`commit_group`, 31 in the WAL flush — and concluded that "whatever it is waiting on, it is not an
`fsync`". It was an `fsync`: `wal.writer.sync()` is called from inside `commit_group` under the WAL
lock, and the sample was reading the syscall's frames as its caller's. The lead was recorded
honestly and pointed at the right line; only the inference from the sample was wrong.

`Interval(d)` was a second defect hiding behind the first. **Nothing read that variant** — the line
above compares against `PerWrite` and nothing else — so it was `Never` under another name, and a
database configured for *bounded* loss had unbounded loss without saying so.

### The fix

[ADR 0036](../adr/0036-a-write-may-have-no-opinion-about-durability.md). `WriteOptions` carries a
`Durability` — `Policy` (the default), `Durable`, `Buffered` — the precedence lives in one function
so the mode and the demand cannot drift, `Interval` is a real background thread, and a clean close
syncs whatever the mode deferred, because these modes trade durability away for a *crash* and an
orderly shutdown is not one.

Every `WriteOptions { sync: … }` literal in the tree became `synced()` or `unsynced()`, which
preserves each site's intent exactly because each site had already written down which one it meant.

### Invariant 1, checked rather than asserted

`Durable` outranks every mode, so a caller that demanded durability still gets it. `Buffered` is
invariant 1's one sanctioned opt-out and stays explicit. What changed is only what a caller with
*no* opinion gets, and under the default `WalSyncMode::PerWrite` that is what it always was.

**`esker-store` is bit-for-bit what it was**, and that is the audit rather than a hope: the crate
has no `WriteOptions::default()` write site at all — every write says `synced()` or `unsynced()` —
so its durability was never resting on this knob. It has always opened its engine `Never` and has
always done its own syncing.

`esker-engine`: **409 tests, 409 passed**, `crash_kill` included. Store, PD, CLI and client
together: **691 passed**.

### The numbers

`docs/bench/debt-c3.md`, both from the same command on the same machine minutes apart: 20,000
single-row applies through `Db::put` on a `Never` database went from **90.37 s to 170.26 ms**, or
4.5 ms to 8.5 µs per row. **531×**, and the before-number reproduces the recorded 95.94 s almost
exactly. The columnar half of the same benchmark did not move, because it never touches the
engine's write path — which is what makes the pair a measurement rather than an anecdote.

## 5. The macOS power-loss gap was closed before it was recorded

Inventory #1, the widest standing gap against invariant 1 as the inventory ranked it:
`crates/esker-engine/src/fs/mod.rs`, `TODO(full-fsync)`, with the rationale in the module header.

### What the check found

The brief said to read the toolchain's own `std` before adding anything, and to stop rather than
add a dependency. `rust-src` is a rustup component rather than a dependency, so reading it costs
nothing:

```text
$ grep -rn F_FULLFSYNC $(rustc --print sysroot)/lib/rustlib/src/rust/library/
library/std/src/sys/fs/unix.rs:1397:            libc::fcntl(fd, libc::F_FULLFSYNC)
library/std/src/sys/fs/unix.rs:1411:            libc::fcntl(fd, libc::F_FULLFSYNC)
```

Line 1411 is inside `File::datasync`:

```rust
pub fn datasync(&self) -> io::Result<()> {
    cvt_r(|| unsafe { os_datasync(self.as_raw_fd()) })?;
    return Ok(());

    #[cfg(target_vendor = "apple")]
    unsafe fn os_datasync(fd: c_int) -> c_int {
        libc::fcntl(fd, libc::F_FULLFSYNC)
    }
    ...
```

and 1397 is the same thing inside `File::fsync`. **`std` issues `F_FULLFSYNC` for both calls on
Apple targets.** `LocalWritableFile::sync_data` calls `File::sync_data`, which calls `datasync`. So
the engine has had power-loss durability on macOS for as long as it has been built with a `std`
that does this, and the recorded debt — "on macOS an acknowledged write can be lost to a power
cut", ranked first of twenty-two — was **not true of this toolchain**.

The premise it rested on is true and was reasoned one step too far: `fsync(2)` on macOS really does
return without forcing the drive's own write cache, and `fcntl(F_FULLFSYNC)` really is the only
thing that does. What the comment did not check is that the standard library had already made that
substitution on the caller's behalf.

### What was actually built

The knob, because the brief asks for one and because the sentence above is a claim about somebody
else's code. `Options::sync_call` — a `SyncCall` of `Data` (the default) or `All`, an enum rather
than the `bool` it started as because clippy's `struct_excessive_bools` was right that `Options`
had enough of those and because naming the two calls reads better than naming one of them "full" —
makes the log take `sync_all` instead of `sync_data`:

| | `SyncCall::Data` (default) | `SyncCall::All` |
|---|---|---|
| Apple | `fcntl(F_FULLFSYNC)` | `fcntl(F_FULLFSYNC)` — identical |
| Linux | `fdatasync` | `fsync`, which flushes the inode's other metadata too |

So it buys nothing on Apple and buys metadata on Linux. It is worth having anyway as the lever to
pull if a future `std` stops doing what the citation says — which is the whole reason the wiring
gets a test rather than the durability doing.

`WritableFile::sync_all` is new, with no default implementation: one that answered by quietly doing
the weaker thing would make the option a lie in the direction that costs data, and the compiler is
the only reviewer that reads every implementation. The fault injector counts and faults it as a
`SyncData`, deliberately — a crash test is exercising a durability barrier that did not hold, and
which of the two calls raised it is a distinction none of those tests draws; a separate operation
would silently halve the fault rate of every plan naming `SyncData`.

The module header now states what is true per platform, with the `std` line quoted in it, and the
`TODO(full-fsync)` is gone. **No dependency was added**, and none was needed: `libc` is `std`'s own
business here, not ours.

### The test

`a_sync_chooses_by_option` counts `sync_all` against `sync_data` through the same counting
filesystem §4 uses, and asserts the option changes which call is made. It cannot assert durability,
because on this platform there is none to tell apart — the two calls are the same syscall. What it
pins is the thing that actually rots: that the option still reaches the call site. Shown red by
replacing the branch with `if false`: *"the option is on and the log still took `sync_data`"*.
