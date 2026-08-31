# Debt wave, lane c1 — the two placement flakes, and the prefix that overwrites

Scope: root-cause `snapshot::a_region_reaches_a_store_that_never_had_it` and
`balance::regions_reach_a_store_that_joins_and_none_is_left_without_a_leader` to green under
saturation, and make an `--sst-store` prefix prove whose it is. The rule for this wave is that
bugs get **fixed**; "known flake" is not an end state, and a timeout constant raised without a
named broken assumption is not a fix.

## 0. The repro, before anything else

Pass-rate statistics do not find these; traces do. The harness is
`scratchpad/saturate.sh <bin> <test> <copies> <rounds> <outdir>`: it runs N copies of one test
binary at once, each a multi-thread tokio runtime that takes a worker per core, so ten copies on
a sixteen-core box oversubscribe tenfold. That is scheduling starvation produced on purpose
instead of waited for.

It works immediately. Ten copies, three rounds:

| test | failures |
|---|---|
| `a_region_reaches_a_store_that_never_had_it` | 19 of 30 |
| `regions_reach_a_store_that_joins_and_none_is_left_without_a_leader` | 12 of 30 |

Both are 5/5 green alone. Three distinct failure messages came out, and they turned out to have
**one** cause between them.

## 1. The mechanism bug: `check_quorum` deposes a leader for growing

Every failing run traces to the same eleven lines. From `scratchpad/trace-snap/c1.log`:

```
39.681  INFO  a learner has caught up; promoting it to voter region_id=1 learner=2 store_id=2
              matched=43 leader_matched=47
39.736  DEBUG applied a configuration change at append time id=1 index=49 node=2 kind=AddVoter
39.749  INFO  stepping down: no quorum contact within an election timeout id=1 term=1
39.761  DEBUG became follower id=1 term=1 leader=None
        panicked: NotLeader { region_id: 1, leader_hint: None }
```

Thirteen milliseconds from the configuration change to the leader deposing itself.

**The broken assumption: `check_quorum` treats a peer that has just become a voter as if it had
been silent for the whole preceding election-timeout window.** It has not been silent — it has
had no chance to answer *as a voter* in the part of the window that already elapsed. Worse, for a
promoted learner this is not even a race:
[`check_quorum_active`](../../crates/esker-raft/src/transfer.rs) clears `recent_active` on every
**non-voter** each window, so a learner arrives at its promotion with the flag false *by
construction*, however busily it was replicating a millisecond earlier. The leader then counts
one of two voters active, concludes it has lost quorum, and stands down at the exact moment its
region grew a replica.

Contrast `become_leader`, which starts every peer at `recent_active = false` too but resets
`election_elapsed` with it, so a new leader always gets a whole window to hear from everyone. The
conf-change path inherits whatever is left of the current window — sometimes one tick.

On an idle box the learner's next append response usually lands before the boundary and nobody
notices. Under starvation it usually does not. In `balance`, which promotes a dozen learners at
once, the correlation is not subtle:

| trace | step-downs | promotions | result |
|---|---|---|---|
| c1 | 0 | 21 | ok |
| c2 | 2 | 22 | ok |
| c5 | 1 | 22 | ok |
| c3 | 24 | 28 | FAILED |
| c4 | 26 | 26 | FAILED |
| c6 | 51 | 27 | FAILED |

This is a real bug and not a test artifact: any user promoting a caught-up learner on a loaded
box has a window in which the region's leader may depose itself, stalling writes for an election.
It is the same defect etcd fixed with `RecentActive: true` in `initProgress` ("Otherwise,
CheckQuorum may cause us to step down if it is invoked before the added node has had a chance to
communicate with us"); ours missed it for promotions as well as for new peers.

**Fix** (`crates/esker-raft/src/core.rs`, `rebuild_progress`): a peer that becomes a voter starts
out recently active — both when the change creates it and when it promotes an existing learner.
The old `is_learner` flag is still in the `Progress` when the loop reaches it, so "was a learner,
is a voter" identifies a promotion exactly.

It costs one window of detection on a voter that really is unreachable: it is false at the next
boundary and the leader steps down then. `check_quorum` is a liveness safeguard and never a
safety one, so a window of patience cannot cost correctness.

**Tests** (`crates/esker-raft/src/transfer.rs`):

* `a_promoted_voter_does_not_depose_the_leader_that_promoted_it` — pins `election_tick` to a
  single value so "this many ticks before the boundary" is a fact rather than a draw, and tries
  **every** offset into the window. Fails on the old code at every offset.
* `a_voter_silent_for_a_full_window_still_deposes_the_leader` — the complement, so the fix cannot
  quietly become "check_quorum off". One window of patience, not an exemption from §6.2.

## 2. What the fix does not cover, and why

Re-running the harness afterwards: `balance`'s "region N has no leader anywhere" is gone, and
`snapshot`'s `leader_hint: None` is gone. What remains at ten-fold oversubscription is a
different thing, and the traces say so plainly — region 1's terms racing 22 → 56 → 57 → 59 → 62 →
69 → 73 → 79 → 84 → 89 → 96 → 97 in fifteen seconds, both voters alternately campaigning.

That is a two-voter group whose members cannot exchange a heartbeat inside a **50 ms** election
timeout because the box will not schedule their threads for longer than that. It is correct Raft
on a machine that has been taken away from it, not a defect: production's election timeout is
1–2 s (`ELECTION_TIMEOUT_MIN_TICKS * TICK_MS == 1_000`, asserted in `esker-raft/src/lib.rs`). The
tests compress it twentyfold, to `tick = 5 ms`, to run fast — and scheduling jitter does not
compress with it.

So the second broken assumption is the harness's: **a 5 ms raft tick is deliverable**. See §3.

## 3. The harness assumption: a 5 ms raft tick is deliverable

`esker-raft` counts ticks and never reads a clock, so an election timeout is 10-20 ticks of
whatever the caller says a tick is worth. Production says 100 ms (`TICK_MS`), giving 1-2 s. Both
tests said **5 ms**, giving 50-100 ms — a twentyfold compression, made to keep the tests fast.

Scheduling jitter does not compress with it. A 50 ms election timeout is a bet that the machine
will schedule this thread within 50 ms, and under a saturated `--workspace` run it will not. The
trace is a two-voter region racing its term 22 → 97 in fifteen seconds with no leader long enough
to apply anything. Nothing there is a defect to find: it is correct Raft on a machine that has
been taken away from it.

Both tests now use `tick = 25 ms` — 250-500 ms, a fivefold compression instead of twentyfold —
with the reasoning in a comment at each site so the next person to reach for 5 ms sees why.

`snapshot.rs`'s `put` also stopped unwrapping. A one-shot write made "leadership does not move"
a silent precondition of every test in the file, which is not that file's subject and is not
true: once `AddPeer`'s learner is promoted the region has two voters, and a starved box
legitimately elects the other one. That is what fifteen of twenty failures were after §1's fix
— `NotLeader { leader_hint: Some(2) }`, three lines after the region had arrived exactly as the
test wanted. It now retries retryable answers against a fresh epoch, and still fails on the spot
for anything else.

### Result

| | before | after §1 | after §3 |
|---|---|---|---|
| `a_region_reaches_a_store_that_never_had_it` | 19/30 failed | 18/20 failed | **0/20 failed** |
| `regions_reach_a_store_that_joins_...` | 12/30 failed | 10/20 failed | **0/20 failed** |

Ten copies at once on sixteen cores, twice over: the DoD's twenty consecutive runs each, at a
load meaningfully harsher than the saturated nextest run that produced the original flake.

## 4. The claim marker (task 2)

Design and consequences are [ADR 0029](../adr/0029-the-sst-store-claim.md). What is worth
repeating here is the one thing that changed shape during the work: **identity is not
`(cluster_id, store_id)`.** Two `esker bench` runs have neither id and would have been judged the
same database; two stores misconfigured with one store id are exactly the collision worth
catching. So the authority is a random id drawn once and kept in the database's *own* directory,
with the cluster and store ids carried along so a refusal names something an operator recognises.

Built:

* `esker_engine::fs::claim` — the format (37 fixed bytes, magic, version, CRC32C), the local
  id file, and the typed errors. Golden test; every single-bit flip refused.
* `TieredFileSystem::new` claims or verifies before it lists the bucket, so a database that is
  going to be refused never learns another database's file numbers.
* `esker server --adopt-sst-store` — the explicit hatch for a prefix that holds objects and no
  marker, which is every pre-6c prefix.

Tested: `esker-engine/tests/tier_claim.rs` (11, `MemoryStore`), the format's own unit tests (8),
`tier_minio.rs::a_prefix_claimed_over_http_refuses_the_second_database` (real `MinIO`), and
`esker-cli/tests/tier_acceptance.rs` unchanged and green — three store processes on derived
`node-N` prefixes, losing a disk and rebuilding from the bucket, which is requirement (d).

## 5. A third bug, found by looking at the harness rather than at the code

Killing the leftovers from the early repro runs turned up two `balance` processes still alive an
hour after their harness had given up on them. A test that **hangs** is worse than one that
flakes — CI loses a slot until the job timeout, and the run says nothing about why — so they were
worth a stack before they were worth a `kill -9`. `sample(1)` gave one:

```text
Thread raft-driver-0:   driver::run -> mpsc::Receiver::blocking_recv -> park
Thread tokio-rt-worker: task::cancel_task -> drop Arc<RaftPeer>
                        -> Arc<DriverPool>::drop_slow -> DriverPool::shutdown
                        -> std::thread::JoinHandle::join
```

`DriverPool::shutdown` offers each worker a `Job::Stop` with `try_send`, which fails on a full
queue. The failure was swallowed — *"a full queue on shutdown must not deadlock the caller: the
worker is going away either way, and a closed channel ends its loop just as well"* — but **the
channel does not close.** The senders live in the `DriverPool` that is being dropped, and a
struct's fields are not dropped until its `Drop::drop` returns. So the worker parked in
`blocking_recv` for ever and `shutdown` blocked in `join` behind it.

`raft-driver-1` had exited on its `Stop`; `raft-driver-0` never got one — asymmetric because the
full queue was the busier worker's. Two workers, twelve regions, a 5 ms tick and a tenfold
oversubscribed box is how a 4096-deep queue gets full.

This is a **production** hang and not only a test one: dropping a `Store` takes exactly this
path, so a store on a loaded machine could fail to shut down at all.

Fixed with a flag the workers read on every wake, set before the `Stop`s go out. That makes the
cases exhaustive without a lock on the send path: either the queue had room and the `Stop`
landed, or it was full — and a full queue means a job is pending, means the worker wakes, and the
first thing it now does on waking is read the flag.

The test drives `run` directly, because "a job was in the queue ahead of the `Stop`" is one job
in a channel of one — deterministic, where filling 4096 slots against a draining worker is a
race. It asserts on *termination* from a thread with a deadline, since without the fix it hangs
rather than fails, and a hanging test tells CI nothing.

**What this says about the earlier numbers.** The hung processes were burning cores throughout
§1 and §2, so every measurement in this file was taken under *more* load than it claims, not
less. The 0/20 runs were re-run on a clean machine afterwards and are the numbers reported.

## 6. A fourth: a guard whose truth was assumed

The full `--workspace` run that was meant to be this lane's last gate came back with one failure,
in this lane's own crate:

```text
FAIL esker-store::server shutdown_answers_the_requests_already_running
     panicked at crates/esker-store/tests/server.rs:576:5:
     the shutdown answered nothing at all
```

`written > 0` is **not the property that test is about.** The property is the comment beside it —
an answer must never say a write succeeded when the store was already closed — together with
`pairs.len() >= written`. Both of those hold when nothing was ever in flight, *vacuously*. So
`written > 0` is a **vacuity guard**: the thing that stops the test passing while testing
nothing. And the `tokio::time::sleep(1 ms)` above it was the bet that the guard would be true.

Under a 1799-test saturated run the bet loses. Sixty-four freshly spawned tasks need not have
been scheduled at all inside a millisecond; the shutdown then refuses all sixty-four, `written`
is zero, and the guard fires. **The guard worked.** The failure's real content is "this test did
not run" — a false failure, but not noise.

Not `70405ae`, and checked rather than assumed: that store opens with `StoreOptions::new()`,
whose `raft` is `None`, so it has no `RaftPeer`, so no job ever reaches the `DriverPool`. The
writes take the `blocking(handle)` path and the shutdown flag cannot be on it. Confirmed
independently by lane wy-c2 against `esker-store/src`.

Fixed by making non-vacuity a **fact** instead of a hope: wait until the store has actually
applied one of the writes, then shut down. The one that landed is answered, so the guard is true
by construction; the other sixty-three are still in flight, which is what the test is for. A
longer sleep would only have moved the bet.

**Honest limit on the evidence.** The local repro was not achieved: 0 of 24 under the
same-test saturation harness, and 0 of 20 against twenty looping `balance` processes. That exact
starvation belongs to a 1799-test run and did not reproduce synthetically. The claim here does
not rest on a repro — the fix removes the dependence on scheduling rather than making it less
likely, and the production evidence is the failure message itself.

## 7. The invariant behind all of it

wy-c2's autopsy of the twenty-one-hour orphan named the shape the rest of this file had been
circling. Their located mechanism was wrong — `wait_for` has a 20 s deadline and did at
`8ba2b49^` too — but the smell was right, and reading `peer.rs` found the real one:

```
put() -> Store::serve() -> RaftPeer::propose() -> answer.await     <- oneshot, no timeout
```

That oneshot is resolved by `complete_proposal`, which runs when an entry **applies at the
proposal's index**, or by `fail_outstanding`, which runs on stop, retire and driver shutdown.
Nothing resolved it when a leader merely **stepped down** — so a proposal whose index was never
applied left its caller waiting for ever. `esker-proto` already writes the rule this breaks:
*"a blocking call with no deadline is a hang"* (`transport/client.rs`).

**The invariant is exhaustive, and that is what makes it checkable.** `pending` is only ever
populated on a leader — both propose paths refuse otherwise — so every pending proposal belongs
to a term in which this peer led, and the ways its index becomes unreachable can be enumerated:

| Path | Handler |
|---|---|
| the entry applies, including a *different* entry taking the index | `complete_proposal` — the one case that may honestly answer `NotLeader` |
| the peer is stopped, retired, or its driver shuts down | `fail_outstanding` (`e06acbf`) |
| a snapshot install replaces a region this store holds | the same, via `fetch_snapshot` step 1's `retire_region_now` |
| **the peer stops leading** | `resolve_unreachable_proposals` — the gap |

Its checkable form is a `debug_assert` at the end of `drive`: a non-leader holding an unanswered
proposal is precisely the leak.

The outcome is **`Unknown`, never `NotLeader`.** `NotLeader` is `NotApplied` — "provably did not
take effect" — and that is the one thing that cannot be promised: the entry is in this peer's log
and a quorum may yet commit it. It is `e06acbf` one path over. Reads are failed too, and with
`NotLeader`, because they are the honest opposite: a `ReadIndex` that never established its index
changed nothing.

**Blast radius, measured rather than asserted.** `Transport::call_with_deadline` wraps every call
in `timeout_at` and `Router::call` adds its own, so nothing reached through `esker-client` can
wedge. The exposure is exactly in-process holders of a `Store`.

**What it cost elsewhere, and the fourth assumption.** Turning the hang into an answer broke
`promotion::a_learner_on_a_fresh_store_becomes_a_voter_under_load`, whose writer asserted every
error it saw was retryable. `Unknown` is not retryable, deliberately. That assertion had only ever
been true because the alternative was a hang — the same shape as the other three. The writers now
treat an ambiguous answer as what it is, and repeat only because every write in these tests is an
idempotent `Put` of one fixed value from a single writer, so a second apply cannot be observed.
That is a licence these tests have and a client does not.

## What this lane did not do

* **The simultaneous-claim race** is narrowed to two round trips by a read-back, not closed.
  Closing it wants `PutObject` with `If-None-Match: *`; ADR 0029 records why that waits for the
  next change to `esker-s3`.
* **`esker bench` has no `--adopt-sst-store`.** A benchmark pointed at a stale prefix should get
  a fresh one, not adopt somebody's objects; `esker server` is where an operator has a database
  worth keeping.
* **Nothing was done about the store-level heartbeat intervals** (`heartbeat_tick` and friends
  at 5-20 ms in these tests). They are a schedule resolution, not a correctness threshold, and
  the traces never implicated them. Left alone deliberately.
