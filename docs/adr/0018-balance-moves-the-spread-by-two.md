# ADR 0018 — balance acts at a gap of two, and decides one region at a time

Status: accepted (phase 4d)
Date: 2026-08-30
Context: `docs/DESIGN.md` §7; `prompts/04-multiraft-pd.md` 4d;
`docs/adr/0013-repair-operators-are-requests-not-commands.md`

## Context

4d asks PD to spread regions and leaders evenly. Unlike repair, nothing is broken: balance is an
optimisation, and an optimiser that runs continuously against a cluster it is also changing has
two failure modes that are much easier to build than to notice.

1. **Oscillation.** Move a leader from the store with 5 to the store with 4, and now they have 4
   and 5. Move it back. A snapshot of the counts looks balanced at every instant, and the cluster
   never stops moving.
2. **The thundering herd.** A hundred regions each decide, from the same reported counts, that
   the emptiest store is the place to be. The counts were a heartbeat old, and the emptiest store
   is now the fullest.

There is also a structural question the heartbeat-driven design forces: PD decides one region at
a time, on that region's heartbeat (ADR 0013). A global optimiser would have the whole cluster in
front of it and could compute a minimal set of moves. A per-region rule cannot.

## Decision

**Act only when the load gap is at least two; count the moves already in flight; and decide per
region, greedily.**

```rust
pub const LEADER_SPREAD_THRESHOLD: i64 = 2;
pub const REGION_SPREAD_THRESHOLD: i64 = 2;
```

- A move is proposed only when `effective(busiest) - effective(quietest) >= 2`.
- `effective` is the reported count **plus every in-flight operator's `LoadDelta`**, applied when
  the operator is issued and withdrawn when it retires.
- Region count is considered before leader count; a replica move is add-then-remove; the replica
  that goes is the one on the busiest store, and if that is the leader's the office is
  transferred first.
- At most `max_balance_operators` moves are *started* at once.
- A per-region cooldown (`balance_cooldown`, 300 s) is a second guard.
- **Neither the cap nor the cooldown may block a move already begun.** Repair is subject to
  neither.

## Rationale

**Two, because a move changes the spread by two.** One leaves the busy store and arrives at the
quiet one. So at a gap of 1 a move produces a gap of 1 the other way — the oscillation above, in
one line of arithmetic. At a gap of `n >= 2` it produces `n - 2 >= 0`: every move strictly
reduces the spread and none can overshoot. Convergence is then an argument rather than a hope,
and "the cluster settles and stops" is a property the tests can assert rather than a behaviour
they can only observe. This is the hysteresis, and making it structural is what lets the timer be
a *second* guard rather than the mechanism.

**Effective counts, because reported ones are a heartbeat old.** Without them the second failure
mode is not merely possible but certain: every region's decision in a round reads the same
numbers. Applying an operator's effect at issue makes each decision see the ones before it, which
is what turns a round of a hundred independent choices into a sequence. The delta lives on the
in-flight entry so that it is withdrawn exactly when the operator retires; a tally kept beside the
set could drift, and a balancer that double-counts a move it has forgotten sends the next one to
the wrong place.

**Region before leader**, because moving a replica takes any leadership of that region with it.
Deciding the leader first would propose a transfer that the replica move then undoes — work that
looks like progress and is not.

**Per-region and greedy, accepting the cost.** A global optimiser would move fewer replicas: the
fairness test converges 60/30/10 to [33, 34, 33] in 85 operators where about 81 is the floor —
27 moves of three steps each, since a move whose replica is the leader's must transfer the office
before it can remove the replica. A region on the middle store may still move to the empty store
early and a region from the full store then take its place. Every individual decision was right
when it was made; only the whole sequence is more than the minimum.

That is the price of the heartbeat-driven design, and it buys three things worth more than the
extra moves: PD needs no scheduler loop and no timer thread; a restart re-derives everything from
the next round of heartbeats with no state to reconcile (ADR 0013); and the rule is a pure
function of one region and the store table, so it is unit-testable against a clock a test sets by
hand. A global optimiser would be a second scheduler with its own state, its own cadence and its
own restart story, to save moves in a cluster that is being rebalanced — which is by definition
not the steady state.

**A move in flight must always be allowed to finish**, and this is the rule the other two kept
breaking. A region mid-move sits on **two stores and is counted on both**, so a half-done move
inflates the very numbers every other decision is taken from. Effective counts correct for the
operator itself; they cannot correct for a replica that genuinely exists twice. Every mechanism
that pauses a move therefore has to exempt the steps that complete one — which is also why the
in-flight cap exists at all: bounding the moves under way bounds the inflation, and with it the
number of moves made against a picture that is slightly wrong. With the cap at four, converging
60/30/10 costs 85 operators against a floor of 81; without it, 417.

**Balance can be switched off**, and repair still runs. An operator who wants a cluster left
exactly where it is should not have to give up replica repair to get it.

## What this got wrong first

Three bugs, each found by a test that the previous version of the code would have passed.

1. **A region whose only replica was its leader could never move.** "The leader's replica is not
   the one that moves" was applied to the *first* half of a move, where it is meaningless —
   adding a replica elsewhere does not move the office. In a one-store cluster every region is
   led by its only peer, so no region could ever spread onto a store that joined. The unit test
   covering it had asserted the broken behaviour as if it were intended.
2. **Finishing a move could pick the wrong replica.** With the leader filter on the second half
   too, the replica dropped could be the *newly added* one — undoing the move just made. The
   replica that goes must be the one on the busiest store, full stop; if that is the leader's,
   the office moves first.
3. **A transfer that was finishing a move was treated as a new one**, so the cooldown stranded
   the region on two stores for five rounds. That inflation is what made the balancer chase its
   own tail: 158 replicas for 100 regions, and a "converged" cluster with 58 moves outstanding.

All three were invisible while the fairness model gave its regions no leader and the convergence
test asked only whether *the counts looked even*. They appeared the moment the model gave every
region a real leader — which a real cluster always has — and the test asked instead whether **PD
had stopped asking for anything** and whether the replica count still equalled the region count.
Those are the criteria it uses now, and the lesson is the ordinary one: a model that is easier
than reality tests something easier than reality.

## Consequences

- **Convergence is bounded and testable.** 100 regions at 60/30/10 settle to [33, 34, 33] and
  leaders to [34, 33, 33], and a thousand further rounds produce no operator at all — the
  assertion that a balancer has actually stopped, which a snapshot of the counts cannot make.
- **A cluster with fewer stores than replicas never balances regions**, because every store
  already holds every region and there is nowhere to move one. Leader balance still works. This
  is not a special case in the code; it falls out of "a store already hosting a peer of this
  region is not a candidate".
- **The thresholds are constants, not knobs.** Two is not a tuning parameter; it is the smallest
  value for which the arithmetic works. A cluster wanting looser balance changes the cooldown.
- **4d's `TransferLeader` needed no wire change**, having been encoded and reserved in 4c.
