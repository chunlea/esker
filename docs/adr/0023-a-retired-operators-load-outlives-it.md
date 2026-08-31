# ADR 0023 — a retired operator's load outlives the operator

Status: accepted (phase 4, retest)
Date: 2026-08-31
Context: `docs/DESIGN.md` §7; `docs/adr/0018-balance-moves-the-spread-by-two.md`;
`docs/bench/phase-4.md` Run 2

## Context

ADR 0018 named the thundering herd and defended against it: an operator's `LoadDelta` is applied
to the store counts when the operator is **issued** and withdrawn when it **retires**, so a round
of decisions is a sequence rather than a hundred independent readings of the same stale numbers.

The phase-4 retest found that the defence has a hole exactly the width of a store heartbeat, and
drove a truck through it. Sixteen regions, five stores, `target_replicas` three. PD grew every
region to three replicas and then took **all sixteen** of the bootstrap store's replicas away:

```text
stores-with-a-replica=5 min=3  max=16 gap=13
stores-with-a-replica=4 min=12 max=12 gap=0     <- store 1 holds nothing at all
```

The operator log says it in one shape, sixteen times over — every `AddLearner` paired with a
`Remove` of that region's store-1 peer, thirty-two operators in four and a half seconds:

```text
07:26:17.093  AddLearner region=1  node=65      07:26:17.848  Remove region=1  node=2
07:26:17.275  AddLearner region=13 node=68      07:26:17.967  Remove region=13 node=14
07:26:17.922  AddLearner region=7  node=69      07:26:18.492  Remove region=7  node=8
...                                             ... twelve more pairs ...
```

The 5-store scale-out of `docs/bench/phase-4.md` Run 2 shows the same *shape* from the other side:
*"peers landed on 4 of the 5 stores; store 4 received nothing in this run's window"*. A live store
holding no replica of anything while the others hold many, arrived at either by being emptied or
by never being filled. Whether that particular store was starved by this defect is **not**
established — the same run records 26 of 38 peers still unpromoted when measurement began, and a
window too short to finish is a sufficient explanation on its own. What is established is that PD
must not be the reason, and that is now a property with a test.

**The hole.** Stores report every `store_heartbeat` interval; PD issues operators between two of
them. An operator that *finished* a moment ago is therefore in neither place PD looks: not in the
in-flight set, because it retired, and not in the store's report, because the store has not sent
one since. Every region that heartbeats in that window reads the busy store at its full, unmoved
count, and each concludes — correctly, on the evidence in front of it — that it should be the
next to leave.

**Why the spread threshold cannot see it.** ADR 0018's guarantee is that every move strictly
reduces the spread, and it held for every one of those thirty-two operators. Sixteen moves that
each strictly reduce the spread still empty a store. This is a **sweep**, not an oscillation, and
hysteresis is a defence against oscillation only. The end state even looks tidy: four stores at
twelve apiece, `gap=0`, which is what let the retest's own convergence check pass a run that had
just destroyed a fifth of the cluster's placement.

## Decision

**A retired operator's `LoadDelta` is held until the reports have absorbed it.**

```rust
/// The load of operators that have finished, still corrected for because the stores they
/// moved have not said so themselves yet.
pub(crate) settling: Vec<(LoadDelta, u64)>,
```

- When an operator leaves the in-flight set — `Done`, `Cancelled` or `TimedOut` alike — its load
  is pushed here with PD's clock.
- The rules' `pending` slice is the in-flight deltas **and** these.
- An entry is dropped once **the oldest report among the live stores** is stamped at or after
  `retired_ms`, and unconditionally once it is older than `max_store_down_time`.

One instant for the whole list rather than a question per store. Exact would be per store — a
delta naming only store 3 could go as soon as store 3 reported — but this runs on every region
heartbeat that reaches the rules, and exact costs a scan of the store table per entry per beat.
The whole imprecision is that a correction may outlive its usefulness by up to one store
heartbeat, which is the interval it exists to cover.

**Live stores only.** A store that is down has a frozen stamp, so counting it would let one dead
store pin every correction in the cluster until the age rule swept it — this defect inverted — and
its counts mean nothing anyway.

**At or after, not strictly after.** The store applied the change before its leader sent the
region heartbeat, and PD stamped the retirement when that heartbeat arrived; a report stamped no
earlier was computed no earlier than the change, so it contains it. Holding it one interval longer
looks safer and is not — it double-counts a move the numbers already show, and `tests/balance.rs`
then settles at `[33, 34, 35]` with two moves outstanding instead of stopping.

Held for `Cancelled` and `TimedOut` too, deliberately: PD cannot tell which end of a cancelled
operator actually happened, and holding a correction that turns out to be unnecessary costs one
deferred move, while dropping one that was necessary costs a sweep.

## Rationale

The delta already describes precisely what the store's report is missing. The only thing wrong
with it was *when* it stopped being applied — at the operator's retirement, which is an event in
PD, rather than at the store's next report, which is the event that actually makes the correction
redundant. Tying the withdrawal to the store's own heartbeat stamp closes the window by
construction: there is no interval in which a completed move is invisible.

It also needs no new number. No threshold, no interval, no knob — `last_heartbeat_ms` is already
stamped by PD on arrival (the same field `is_down` reads, for the same reason: `CLAUDE.md`
invariant 6 forbids trusting a store's own clock).

## What was considered and rejected

**A fair-share floor on the source store** — refuse a move that would take the busiest store below
`floor(total / live_stores)`. Written, tested, and removed. It is computed from the store reports,
which are the very numbers that are stale, so in the state that causes the sweep the floor is
stale in exactly the same way and permits exactly the same move. No mutation of it could be made
to fail a test, which is the honest signal that it was doing nothing: with accurate counts ADR
0018's threshold already guarantees no store goes below the balanced level, and with inaccurate
ones the floor is as wrong as everything else. `docs/plans/` doctrine — do not ship a change on
the strength of a plausible story — applies to guards as much as to fixes.

**Predicting the departure at the start of the move** — have the balance `AddPeer` record
`{region_from: busiest, region_to: quietest}` rather than the arrival alone. This double-counts:
the `RemovePeer` that finishes the move records the departure again, so the source reads two
replicas lighter per move. Worse, it makes the source stop looking like the busiest store to the
very rule that has to pick the replica to shed, so the second half of a move starts choosing the
*wrong* peer — the bug ADR 0018 §"What this got wrong first" already records once.

**Counting replicas from PD's own region records** rather than from the store reports. This is the
correct answer and PD has the data: it holds every region's peer list and updates it on every
region heartbeat, so it never needs to ask a store how many regions it has. It is rejected here
only on cost — a scheduler that runs on every region heartbeat cannot afford an O(regions) scan,
so it needs an incrementally maintained per-store replica index, which is a larger change than the
defect warrants. Recorded as the eventual answer.

**Slowing balance down** — a longer cooldown, a smaller in-flight cap. Both make the sweep take
longer and neither stops it, because a sweep is sixteen individually-correct decisions and time
does not make any of them wrong. What the cap *does* bound is the burst: it is how many decisions
can be taken before the first of them appears anywhere, which is why a store may still dip below
its share by at most `max_balance_operators` and no further.

## Consequences

- **The property is now testable without a soak, and is tested that way.**
  `a_sweep_against_one_frozen_store_report_stops_at_the_fair_share` builds the retest's state
  directly and then reports **once**, never again, for fifty rounds of decisions. The cluster
  settles at `[9, 10, 10, 10, 9]`. Without `settling` it is at `[4, 11, 14, 11, 8]` by round nine
  and still falling. The ordinary soaks could not find this and never will: they refresh every
  store's report before every decision, which is the one condition under which the fault cannot
  happen.
- **A store may still dip below its share by the in-flight allowance**, which is
  `max_balance_operators` — the decisions taken before the first of them can show up. That is a
  bound rather than a hole, and it is the assertion the test makes.
- **Memory is bounded twice**: by the store heartbeat in the ordinary case, and by
  `max_store_down_time` in the case of a store that never reports again.
- **A PD restart forgets it**, exactly as it forgets the in-flight set (ADR 0013). A restarted PD
  also re-reads the store reports, so it is not left with half a picture; it is left with the
  reported one, which is where every PD starts.
