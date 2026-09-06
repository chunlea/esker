# 0081. The tick driver catches up on the ticks a starved runtime missed

Date: 2026-09-05

## Status

Accepted.

## Context

`esker-raft` counts the election timeout in ticks and never reads a clock — `CLAUDE.md` invariant 4,
"time enters via `tick()`":

```rust
fn tick_election(&mut self) {
    self.election_elapsed += 1;
    if self.election_elapsed >= self.randomized_election_timeout { /* campaign */ }
}
```

The store drove that clock with one tick per wake:

```rust
let mut ticker = tokio::time::interval(interval);
ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
loop { ticker.tick().await; peer.tick().await; }
```

`Delay` does not catch up. A tick missed because the task was starved is **lost**, and the next is
scheduled a full interval after the late wakeup — so the ticks a peer receives are not
`elapsed / interval` but a function of how often the runtime scheduled it. `peer.tick()` then
travels through `DriverPool::deliver`, where it can also queue behind the region's own apply work.

The consequence is a liveness failure that looks like nothing at all. Under load `election_elapsed`
grows slower than the world, so a 10–20 tick timeout stretches with the starvation, and a region can
sit with **every peer a follower that has simply not counted high enough**. `docs/plans/debt-c7.md`
§24 records three independent sightings across three crates — `promotion`, `snapshot`, and the
`esker-client` router — of a region with no leader, and no peer believing anyone else had it, for
**ninety seconds** at fourteen busy threads. Writes to that range cannot proceed for as long as it
lasts. It is not invariant 1: nothing acknowledged is lost, because nothing is acknowledged.

The tree already had the right instinct one line away, for a different job. PD heartbeats use
`Skip`, deliberately, so that a stalled store does not emit a run of identical reports. **A report
and a clock want opposite policies**, and they had been given the same family.

## Options

**A. `MissedTickBehavior::Burst`.** The default: replay every missed tick. Correct in the sense that
the time really did pass, and one word to change. Unbounded, though — after a long stall a peer is
handed an arbitrary number of ticks at once, implicitly, and every peer bursts together.

**B. Advance by elapsed time, explicitly and with a cap.** The driver reads a monotonic clock on
each wake, computes the ticks owed since the last one it delivered, and delivers that many, bounded.

**C. Leave it.** Treat a starved box as out of scope. Defensible for a laptop under a deliberate
load arm; not for a loaded production node, which is the same shape at a different scale.

## Decision

**Option B.** `crate::peer::drive_ticks` reads `Instant::now()` on each wake and delivers the ticks
owed since the last delivered one, capped at `TICK_CATCH_UP_CAP` (40 — one election timeout's worth
at the widest randomised bound).

Three properties, each chosen rather than fallen into:

- **The core still reads no clock.** Invariant 4 is about `esker-raft`, and it is untouched: the
  driver is the thing that knows how much time passed, and it says so in the only unit the core
  accepts. `Instant` is a duration since a local event and orders nothing.
- **Capped, not unbounded.** A peer starved for longer than an election timeout has already lost
  whatever lease it had; replaying every owed tick would have all of them campaign the instant the
  box recovered. Delivering the cap gets each peer to its timeout and no further, and the
  randomised timeout does the rest — which is what it is for. This is why B and not A: A is the
  same idea, implicit and unbounded.
- **The excess is warned, not banked.** Past the cap the debt is forgiven and a `warn` names what
  was swallowed. Banking it would have the next wake deliver a second cap's worth for time already
  gone; and a peer owed more than a cap is a peer whose box stopped scheduling it, which is worth a
  line in the log whatever the election does next.

Under the cap, the bookkeeping advances by **what was delivered** rather than to `now`, so a wake
that is half an interval late carries that half forward instead of rounding it away a tick at a
time.

## Consequences

**After a stall, elections happen.** That is the point, and it is also the change in behaviour: a
loaded node that previously sat leaderless now campaigns. Terms will advance across a stall where
they used to sit still.

**A thundering herd is bounded but not abolished.** Every peer of a stalled region reaches its
timeout at about the same moment. `randomized_election_timeout` is what separates them, and the cap
is what stops a replay making the pile-up worse. If a future sighting shows repeated split votes
after a stall, that is the place to look, and the cap is the knob.

**The heartbeat schedules are deliberately not changed.** `server.rs`'s two `Skip` intervals are
right for what they do. The comment above the region-heartbeat schedule claimed the peer's ticker
was "the same shape, for the same reason"; it no longer is, and the comment says so — it also named
`Delay` as the variant that replays, where it is `Burst`.

**Testable without a busy machine**, at both levels. The rule and its bookkeeping are pure
functions (`ticks_owed`, `wake`) with unit tests that assert the old one-per-wake behaviour as the
failing arm, so the defect is stated as a fact rather than described. The **loop** is tested under
`tokio`'s paused clock, which required enabling tokio's `test-util` feature in `esker-store`'s
**dev**-dependencies: a feature of a crate already in the runtime graph, so nothing joins it and
`deny.toml`'s crate budget is untouched. A test that waited for a real busy machine to starve the
driver would be asserting the scheduler's mood instead of the driver's rule.

## What is not settled

This ADR fixes a mechanism that **explains** §24's sightings; it does not prove they were it. The
confirming evidence is one occurrence under a fourteen-thread arm showing `raft_role=Follower` with
a term that has not moved — the instrument in `promotion.rs` already prints exactly that. If such an
arm instead shows `Candidate`, this hypothesis is wrong, the fix above is still correct on its own
terms, and §24 stays open.
