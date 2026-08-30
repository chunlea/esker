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
- Region count is considered before leader count; a replica move is add-then-remove; the leader's
  own replica is not the one that moves.
- A per-region cooldown (`balance_cooldown`, 300 s) is a second guard; repair ignores it, and so
  does the second half of a move already begun.

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
fairness test converges 60/30/10 to 33/34/33 in 120 operators where the minimum is about 54,
because a region on the middle store may move to the empty store early and a region from the full
store then takes its place. Every individual decision was right when it was made; only the whole
sequence is more than the minimum.

That is the price of the heartbeat-driven design, and it buys three things worth more than the
extra moves: PD needs no scheduler loop and no timer thread; a restart re-derives everything from
the next round of heartbeats with no state to reconcile (ADR 0013); and the rule is a pure
function of one region and the store table, so it is unit-testable against a clock a test sets by
hand. A global optimiser would be a second scheduler with its own state, its own cadence and its
own restart story, to save moves in a cluster that is being rebalanced — which is by definition
not the steady state.

**Balance can be switched off**, and repair still runs. An operator who wants a cluster left
exactly where it is should not have to give up replica repair to get it.

## Consequences

- **Convergence is bounded and testable.** 100 regions at 60/30/10 settle in two rounds, leaders
  in one, and a thousand further rounds produce no operator at all — the assertion that a
  balancer has actually stopped, which a snapshot of the counts cannot make.
- **A cluster with fewer stores than replicas never balances regions**, because every store
  already holds every region and there is nowhere to move one. Leader balance still works. This
  is not a special case in the code; it falls out of "a store already hosting a peer of this
  region is not a candidate".
- **The thresholds are constants, not knobs.** Two is not a tuning parameter; it is the smallest
  value for which the arithmetic works. A cluster wanting looser balance changes the cooldown.
- **4d's `TransferLeader` needed no wire change**, having been encoded and reserved in 4c.
