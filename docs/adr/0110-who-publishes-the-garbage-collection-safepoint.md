# 0110 — Who publishes the garbage-collection safepoint

Status: **Proposed**, 2026-09-10; **step 1 built 2026-09-11**. Decision material for #58.
The hand-operated publisher below is built and measured — see *Step 1 is built*. **Step 2, who
publishes automatically, is proposed and unbuilt**, and is the part that needs a ruling.

## Context — the collector exists, works, and has never been given a number

The brief for this ADR was "design safepoint GC in compaction". **It is already designed and
built.** [ADR 0021](0021-time-machine.md) decided it, `esker-store/src/gc.rs` implements it as an
`MvccCollector` compaction filter, and it is wired into every store — `Store::open` builds the
collector before the engine, because a compaction filter is a column-family setting that the engine
has to be opened *with*.

What it does is exactly what was wanted:

> Below a key's effective safepoint: the **newest** version, because that is what a read at the
> safepoint returns and dropping it would make an existing key vanish. Everything older goes, and so
> does its `default` entry. A **rollback marker** is the exception — it lives until the safepoint
> passes its `start_ts`, because below that there can still be an in-flight `Prewrite` that the
> marker is the only thing stopping.

The store honours a wire message that raises it, `TxnKvReq::GcSafepoint`, and refuses to lower it.
There are tests for the arithmetic, for the rollback-marker rule, for the per-table retention
override, and for the safepoint surviving a restart.

**And nothing has ever sent one.** Every sender of `GcSafepoint` in this repository is a test:

```text
crates/esker-store/tests/reclaim_range.rs:373,445
crates/esker-store/tests/txnkv.rs:758,764,919,979
```

`esker-pd` does not contain the word `safepoint` outside one comment comparing another
cluster-wide number to it. So in every real cluster the collector's `published` safepoint is the
`0` it was constructed with, `effective_safepoint` is zero for every key, and **no version has ever
been collected.** That is #58's floor: tonight's read-path fixes stop a scan *walking* the history,
and nothing stops the history existing.

ADR 0021 said so at the time, in as many words, under *What the rest of the system has to grow*:

> **`esker-pd`** — the safepoint arithmetic above, a pin for a named checkpoint (the same mechanism
> as an active read), and the "smallest retention in the cluster" input that makes a shorter
> override real.

This ADR is that debt. The question is not what to collect. It is **who computes the number and how
it reaches a store.**

## What tonight's read-path fixes did and did not buy

Both landed before this was written, and they change what is urgent here.

`esker.entries-stepped`, twenty keys, one full scan, V committed versions each:

```text
             before      after
V=1            80          80
V=16          980         220
V=256      15,380         220
```

**The scan's cost no longer tracks V at all**, so the symptom r1 measured — the same work costing
more every pass — is addressed on the read path. What is *not* addressed, and cannot be by any read
path:

* **Space.** r1's arm A grew the data directory ~13 MB per pass, linearly, for ever. Nothing
  reclaims it.
* **Compaction work.** Every compaction rewrites every live byte, and every byte is live.
* **The `log n` the seek is in.** Flat in V is not flat in *total entries*; a seek past a key is
  `O(log n)` in a tree that never stops growing.
* **Cache dilution.** A block cache holding mostly superseded versions is a cache holding less.

So the honest answer to *"how long does the read-path fix hold?"* is: **for the scan, indefinitely;
for the cluster, only as long as the disk lasts.** That is a real reprieve — it converts a
correctness-shaped urgency into a capacity one — and it is why this is Proposed rather than
something to build tonight.

## The hard part, and it is not the arithmetic

A safepoint is a promise that **no reader will ask below it**. Break it and a read returns a
version that was collected, or no version at all, where the transaction's own snapshot says one
exists. So the number has to be at or below the oldest timestamp anything might still read at.

Two inputs, and this system has one of them:

| input | who knows it | present? |
|---|---|---|
| the retention window, as a duration | PD's own configuration | yes |
| **the oldest active read's `start_ts`** | the clients holding transactions | **no** |

Nothing registers a read. `esker-client` takes a `start_ts` from the TSO and uses it; PD is not told
and never hears about it again. A safepoint derived from the clock alone would collect below a long
transaction's snapshot and give it wrong answers with nothing to say so.

### And a trap that is specific to this tree

`esker_pd::tso` composes `ts = physical_ms << 18 | logical` from a real clock. **`CountingOracle` —
which `esker-sql`'s `connect()` still builds, and which every test cluster uses — is a pure
counter**, so the physical half of every timestamp it hands out is zero: every version it has ever
stamped sits inside the first millisecond of 1970. ADR 0021 found this and wrote it down.

A wall-clock safepoint published into such a cluster is `now − 10 min`, which is astronomically
*above* every version in it. The collector would immediately reduce every key to its newest version
— correct by its own rule, and catastrophic for any reader holding one of those tiny timestamps.
**A clock-derived safepoint is only sound where the timestamps came from the same clock**, and that
is a property of the deployment, not of PD.

## Options

### Where the number comes from

* **(a) A fixed window off PD's physical clock.** `safepoint = (now_ms − retention_ms) << 18`.
  Three lines, no protocol change, and wrong in exactly the two ways above: it ignores long readers,
  and it is unsound against any oracle whose timestamps are not wall-clock.
* **(b) The oldest active read, reported by the things holding them.** Every client or SQL node
  reports the minimum `start_ts` of its open transactions; PD publishes the minimum across
  reporters, and a reporter that goes silent is treated as holding its last-reported value until it
  times out. Correct, and it is what the safepoint *means*. Costs a field on an existing PD request
  and a registry in PD.
* **(c) Both, and the minimum of them.** `min(now − window, oldest active read)`. The active-read
  floor is the load-bearing half; the window only stops an abandoned reporter pinning history for
  ever, and it is the half that must be switched off where the oracle has no clock.
* **(d) Stores decide locally.** No. Two stores would collect to different depths and a read that
  crossed regions would see history on one and not the other — a safepoint is cluster-wide by
  definition, which is the sentence ADR 0021 opens with.

### How it reaches a store

* **(e) On the store-heartbeat answer.** `PdClient::store_heartbeat` returns `Result<(), _>` today —
  it answers nothing. Widening it is additive, it already runs every 10 s, and every store sends
  one. This is the channel the design implies: *"there is no command channel and no push — PD
  schedules from what heartbeats tell it and replies on the same heartbeat."*
* **(f) As an `Operator` on the region heartbeat.** Wrong shape: an operator is about one region and
  a safepoint is about a store, and it would arrive once per region per round.
* **(g) A PD-initiated `GcSafepoint` to every store.** The message already exists and is the one the
  store honours — but it needs PD to hold connections *to* stores, which it does not do for anything
  else, and a store PD cannot reach would silently keep everything.
* **(h) An operator command, `esker admin gc --safepoint <ts>`.** Not a design, but it is the
  cheapest possible way to *measure* whether collection fixes r1's climb, using a verb shaped like
  the ones ADR 0109 just added.

## Recommendation

**(c) over (e)**, with **(h) first** as the measurement.

The ordering matters more than the choice. (h) is an afternoon and answers the question this ADR
exists to inform — *does collecting actually flatten r1's four passes?* — without committing the
protocol to anything. If it does not, (c) is a large change bought for nothing and the next suspect
is elsewhere. If it does, (c) is worth its protocol field and its registry.

Within (c), the **active-read floor is the part that must be built**, and the window is the safety
net. Building the window alone is worse than building nothing: it produces a number that looks
right, is published, is honoured, and is wrong precisely when a transaction is long — the case
nobody tests and everybody eventually hits.

## What does not change, and one thing that might

* **Invariant 1 (log before state, fsync before ack)** — untouched. Collection happens in
  compaction, which is not on the write path.
* **Invariant 2 (every byte checksummed, every file versioned)** — untouched. A compaction output
  is an ordinary SST written by the ordinary builder.
* **Invariant 3 (immutable files, atomic pointers)** — **untouched, and this is the one worth
  saying out loud.** Collection drops entries while *writing a new file*; no existing SST is
  modified, and the manifest edit that swaps them is the same atomic pointer move every compaction
  already makes. Nothing here mutates anything in place.
* **On-disk format** — **no change, and it should stay that way.** The safepoint lives in an
  `AtomicU64` on the collector and is re-published every few seconds; after a restart it is zero
  until PD publishes again, which keeps *more* rather than less and is the safe direction. If a
  future step wants the safepoint persisted — to stop a restarted store briefly serving history it
  had already collected — that is a format question and comes back here first.
* **The `lock` and `default` column families** — already handled by the existing collector: a
  `default` entry goes with the `write` record that names it, which is why the collector reads the
  record rather than the key alone.

## Acceptance

* **r1's four passes go flat** on the same file with the same recipe — the measurement this whole
  investigation is for, and the only one that closes #58 rather than deferring it.
* **`esker sst-dump` shows the dead-to-live ratio falling** across a compaction, which is now
  possible at all: until `42a0c40f` it could not open a table a store had written.
* **A long read is not broken by it**: a transaction that starts, waits past the retention window,
  and then reads must either answer from its snapshot or fail loudly — never answer differently.
  This is the test the window-only option cannot pass, and it is how to tell the two apart.
* **A cluster on `CountingOracle` collects nothing**, rather than collecting everything.

## Step 1 is built, and what it measured

`esker admin gc --safepoint <ts> --store <addr>` — one more method on ADR 0109's admin service,
`0x0506`, purely additive, two golden lines added and none changed. It raises the store's published
safepoint (never lowering it), compacts every column family so the filter actually runs, and answers
with the safepoint in force and **per-family SST and entry counts**. It does **not** publish
anything automatically; that is step 2 and remains the user's to rule on.

**The collector works exactly as ADR 0021 describes.** Eight keys, twelve committed versions each,
written through prewrite and commit, flushed, then compacted at two safepoints on two fresh stores:

```text
                    safepoint 0        safepoint u64::MAX
     write               96          →        8            one version per key
      lock              192          →      192            unchanged
   default                0                   0            values were short and inline
      raft                1                   1
```

`write` falls to exactly one entry per key. That is the whole of what this ADR proposed to find out,
and it is now a fact rather than an expectation.

### Two things the measurement found that the proposal did not predict

**The `lock` column family is not collected at all, and it is the bigger number.** 192 entries
against the `write` family's 96, unchanged by any safepoint — because they are not MVCC versions.
Every prewrite puts a lock and every commit deletes it, so V commits leave `2V` superseded *engine*
entries per key, and the safepoint filter has no opinion about a column family it does not
understand. Tonight's engine fix stopped a scan *stepping* through them; nothing removes them.

So **collection alone will not flatten the space curve**, and the prediction handed to r1 should say
so: expect the `write` family to shrink and the directory to fall by less than the version count
suggests. Whether those lock entries survive a *bottom-level* compaction in a longer-lived store, or
only this test's single compaction, is the next thing to measure and is not answered here.

**A second compaction of an already-compacted family is a no-op**, so a store compacted at safepoint
zero and then again at a higher one reports "collected nothing" whatever the collector does. The
acceptance uses a fresh store per safepoint for that reason. Any future measurement that raises a
safepoint on a live store has to force the compaction to have work to do, or it will measure its own
no-op — which is the shape of mistake this whole investigation has been made of.

### And the module doc overstates one thing

`gc.rs` says *"Everything older goes, and so does its `default` entry."* Nothing in `gc.rs` touches
the `default` family: its `cf::DEFAULT` references all read retention configuration. Where a value
is too large to inline it lives there, keyed by `start_ts`, and this measurement could not see the
case because its values were short. **Whether a large value's `default` entry is collected with its
`write` record is unverified**, and it is the difference between reclaiming version records and
reclaiming bytes. It is the first thing to check before anyone quotes a space saving.

## The `lock` family's dead entries — a candidate rule, and what is still unconfirmed

Measured above: 192 entries against `write`'s 96, unchanged by any safepoint. Two facts settle what
they are.

**A lock key carries no version.** `key::lock` is `'x' ++ enc(user_key)` and the doc comment says so
in as many words — it is *"the unversioned key a lock is stored under"*, and the versioned builders
are "this plus eight bytes". So the `lock` family has **no MVCC at all**: a key either holds a live
lock or it does not, and there is no history to preserve.

**The 192 are the engine's own superseded entries.** Every prewrite puts the lock and every commit
deletes it, so V commits leave `2V` entries for one key — a put and a tombstone per transaction —
of which at most the newest matters.

### The candidate

**A superseded `lock` entry may be dropped outright, and no safepoint governs it.** Nothing can read
below the newest entry for a lock key, because there is no timestamp with which to ask: `get_lock`
takes a user key and no `ts`. That is the whole argument, and it is a stronger one than the `write`
family's — collecting a version needs a safepoint because a reader may hold an older snapshot, and
there is no such reader here.

This is **ordinary engine behaviour**, not a filter: an LSM drops a key's superseded entries when it
compacts the bottommost level holding them. So the candidate is not "write a lock collector" but
**"find out why the existing compaction did not"**, and the two obvious suspects are that
`compact_range` did not reach a bottommost level for those keys, or that something pinned a
snapshot across it.

### What is not confirmed, and it is the part that decides the size

**I did not establish why the 192 survived.** The measurement compacted every family and they did
not move; whether that is a property of a single manual compaction, of a store with one level, or of
a rule that would also keep them in a long-lived cluster is exactly what nobody has asked yet. Until
that is known this is a candidate and not a plan — and it may turn out to be no work at all, which
is the outcome worth checking before any is done.

### The shape of the test that settles it

Not an assertion about a number, because the number depends on the level layout, but about a
**difference**:

* commit the same key `V` times so the family holds `2V` entries for it, and verify that it does;
* force a compaction that reaches the bottommost level for that key — more than one level's worth of
  data, so the compaction has somewhere to compact *to*;
* assert the family's entry count falls to about one per key, **and** that `get_lock` still answers
  correctly for a key that is currently locked, which is the only thing these entries are for;
* the counterfactual is a key with a **live** lock: its entry must survive, or the rule has been
  written as "drop locks" rather than "drop superseded locks".

The last one is the one that matters. A rule that collected a live lock would let two transactions
prewrite the same key, which is the failure the lock family exists to prevent.

## Consequences

* **#58 stops being a floor.** Tonight's fixes stopped the scan walking the history; this stops the
  history accumulating, which is the half no read path can reach.
* **A new way to get a wrong answer**, and it is the reason for the ordering above: a safepoint that
  is too high is silent data loss from a reader's point of view. Everything in the recommendation is
  arranged to make the floor come from something that *knows*, rather than from a clock that does
  not.
* **PD gains state it has not had**: a registry of reporters and their oldest reads, which is a
  liveness question of its own — a reporter that dies holding an old timestamp must stop pinning
  history, and the timeout that decides that is a number this ADR does not pick.
* **ADR 0021's `esker-pd` line is finally answerable**, and the "smallest retention in the cluster"
  input it also names falls out of the same registry.
