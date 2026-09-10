# ADR 0101 — A batch of ticks never carries a whole election timeout

Status: **accepted** (2026-09-09) · Numbered 0101 by the coordinator; 0100 is the highest on `main`.

## Context — two correct rules that cancel

**Rule one: the election timeout is randomised per peer.** `Raft::reset_election_timeout` draws from
`Config::election_tick`, `(10, 20)` ticks by default. It is the whole of what stops two peers that
time out together from doing it again: each redraw separates them, so a split vote costs one term
rather than becoming a habit.

**Rule two: a starved tick driver catches up.**
[ADR 0081](0081-the-tick-driver-catches-up.md) — a tick never delivered is elapsed time the cluster
never counts, so a driver that was not scheduled for twenty intervals owes twenty ticks and
delivers them. The store's ticker queues them back to back (`TICK_CATCH_UP_CAP`, 40) and the worker
drains everything queued before it drives anything, which is the batching `docs/DESIGN.md` §6 asks
for.

**Together they cancel.** A batch as wide as the randomisation's own span covers **every draw in
it**: whatever each peer picked, it crosses its threshold inside the same batch, and nothing it
sends leaves the process until the batch is over. So all three campaign in lockstep however
carefully they randomised, the vote splits, the pre-vote round ends with nobody, and the next batch
does it again.

**Measured on a real cluster first**, in the sweep `docs/plans/debts-v1.1.md` #34 records — three
voters of one region, all `PreCandidate`, all `leader None`, terms 209, 210 and then 617, thirty
seconds with no answer:

```text
store 1: handle is peer 12 · published leader None term 209 · core says peer 12 PreCandidate …
store 2: handle is peer 13 · published leader None term 210 · core says peer 13 PreCandidate …
store 3: handle is peer 14 · published leader None term 210 · core says peer 14 PreCandidate …
```

**And then reproduced with no cluster at all**, because `esker-raft` is a pure state machine and a
batch is `tick()` called n times before anything is delivered.
`a_batch_of_ticks_flattens_the_randomised_timeout` (`crates/esker-raft/src/election/tests.rs`) is a
discriminating pair over the *same number of ticks*:

| delivery | after the same ticks |
|---|---|
| one at a time | **exactly one leader** |
| in batches of `high + 1` | **no leader at all**, node 1 at term 6, `campaigns_pre` 6, `campaigns_real` 6 |

Six batches, six terms, no leader. The randomisation is not weak here — it is **flattened**.

## Options

**(a) Widen the randomisation, or lower the catch-up cap.** Tuning: it moves the region of the
parameter space where the two rules cancel, and does not stop them cancelling. A cap below the
election floor also throws elapsed time away, which is the thing ADR 0081 exists to stop.

**(b) A starvation signal in the core.** The core can see the overshoot — `election_elapsed` past
`randomized_election_timeout` — and could defer campaigning when it is large. But the core has no
notion of a batch: `tick()` is the whole of its clock, so a deferral counted in ticks is consumed
by the same batch, and every peer's is consumed alike. It also puts a policy about the driver's
scheduling inside the state machine, which `CLAUDE.md` invariant 4 keeps clean deliberately.

**(c) The batch stops before it can carry a whole election timeout.** The worker counts the ticks
in the batch it is draining and drives when it reaches the election floor, then goes on draining.
Nothing is dropped, every tick is still delivered, and the group *speaks* between chunks — which is
what lets the first peer to time out be heard before the others fire.

## Decision — (c)

`esker_store::driver`'s worker drains a batch until it has carried `TICKS_PER_BATCH` ticks, one
below `esker_raft::ELECTION_TIMEOUT_MIN_TICKS`, and then drives. **Only ticks count against it**: a
batch of appends or reads is exactly what batching is for.

The number is not tuned and is not meant to be. The property is that **no peer can have timed out
on one batch's ticks alone**, which is what restores the randomisation's ability to separate peers;
one below the floor is the largest batch for which that is true.

## Consequences

* A starved worker drives more often while it catches up — one `Ready` round per nine ticks rather
  than one per forty. Each is cheap: a catch-up carries no entries, so the extra rounds are a
  `has_ready` check and a send.
* **ADR 0081 is unchanged in what it claims.** Every owed tick is still delivered and none is
  banked; what changes is how many travel together.
* The core keeps no new state and learns nothing about drivers, so `CLAUDE.md` invariant 4 holds:
  time still enters by `tick()` and nothing else.
* `a_batch_of_ticks_flattens_the_randomised_timeout` stays as a **characterisation** of the core,
  asserting that batched delivery still livelocks it. If that ever stops holding, the core has
  grown a defence of its own and this rule can be revisited — which is why it is an assertion
  rather than a comment.
* **It guards the leader as well as the election, which this file did not set out to claim.** A
  driver's queue carries ticks and messages alike, so a follower can advance `election_elapsed`
  past its own draw on ticks queued *in front of* the heartbeat that would have reset it — and
  §6.2's lease is no defence, since a follower starved alike has spent its own window and grants
  what it would otherwise refuse. Measured on both sides of the cap in
  `a_batch_that_puts_ticks_before_a_heartbeat_deposes_a_live_leader`: at the pre-cap width of 21 a
  live leader is deposed and the term moves; **at the capped 9 it survives**, because every drive
  delivers a heartbeat before nine ticks can reach a draw of ten. The field's *a leader is elected
  and does not survive*, seen at a HEAD that already has this rule, is therefore **not** this
  mechanism — which is what makes the note worth keeping.
* What this does **not** claim: that #34's stall is gone. That stall was separated to an overloaded
  in-process harness — four real store processes reached 663 regions without one — and this removes
  the mechanism the sweep found inside it. The next run of `how_the_leaderless_window_moves_with_the_drivers`
  at 5 ms and 4 threads is where that is checked, and it needs a window.
