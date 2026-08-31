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

