# ADR 0026 — the quorum-loss boundary, and the door out of it

Status: accepted (phase 4, post-acceptance)
Date: 2026-08-31
Context: `docs/DESIGN.md` §7; `docs/bench/phase-4.md` Run 4;
`docs/adr/0013-repair-operators-are-requests-not-commands.md`; `crates/esker-pd/src/schedule.rs`

## Context

Phase 4's acceptance run killed a store and watched PD repair eight regions. Seven recovered.
Region 27 did not move at all, and 1,410 keys stopped answering:

```text
before the kill:  region 27  epoch=(3,4)  peers=*28@store1V  29@store3V   <- 2 peers, not 3
after 270s:       region 27  epoch=(3,4)  peers=*28@store1V  29@store3V   <- byte-identical
```

Two different failures are tangled in that one line, and separating them is the whole point of
this ADR.

1. **Region 27 was already a two-voter region before anything was killed** — an add that had not
   finished promoting when the pre-kill wait gave up. PD watched it sit there with every store
   alive and did nothing, because the repair rule fired on a store being *down* rather than on a
   region being *short*. That is a bug, and it is fixed in the same change as this ADR: repair
   now triggers on the state (`crates/esker-pd/src/schedule.rs`), quorum-risk regions first.
2. **Once one of the two voters died, nothing PD could have done would have helped.** That is not
   a bug. It is Raft working exactly as designed, and it is a boundary the system needs to state
   out loud rather than discover again in the next acceptance run.

## Decision

**Repair works up to the quorum boundary and stops there. Past it, recovery is an operator's
decision, never PD's.**

- Before the boundary, PD repairs any region with fewer live replicas than `target_replicas`,
  whatever put it there, and orders the work by `schedule::Urgency` — a region at exactly a
  quorum of live voters first, because it is the one a single further failure ends.
- At the boundary, PD keeps issuing the `AddPeer` and promises nothing by it. A region whose
  votes are genuinely gone cannot apply it.
- **PD never forces a configuration.** No automatic recovery, at any timeout, under any
  liveness evidence.
- The door out is a future operator-invoked command, `esker region unsafe-recover`: force a new
  single-member configuration from a named surviving replica's log, one region at a time,
  explicitly lossy-risk. This ADR records that the door is needed and what it must respect. It
  designs nothing else — no flags, no wire message, no authorization model.

## Rationale

**A region cannot repair itself out of quorum loss, because the repair is a proposal.** Adding a
peer is a membership change, a membership change is a log entry, and a log entry commits only
with a majority of the *current* configuration (`crates/esker-raft/src/conf.rs`). Region 27's
configuration was two voters, its majority was two, and one of the two was gone. The `AddPeer`
that would have rescued the region had to be committed by the group it was rescuing. The epoch
never moving is the proof: nothing was ever agreed, because nothing could be.

**PD does not merely fail to fix such a region — it stops hearing about it.** Only a leader
reports a region (`crates/esker-store/src/heartbeat.rs`), check-quorum is on by default, so the
survivor steps down within an election timeout and the region goes silent. Operators ride on the
answer to a heartbeat (ADR 0013), so a region past the boundary has no channel through which PD
could be told to fix it even in principle. Two consequences follow, and both are load-bearing for
whoever builds the door: **detection is by absence** — a region record whose `last_heartbeat_ms`
has gone stale while the stores its peers name are alive — and **recovery cannot be an
operator**; it has to be a direct call to a surviving store, outside the heartbeat path.

**Automatic recovery would trade a bounded loss for an unbounded one.** PD's "down" means
*silent to PD*, which is not the same as gone: a store partitioned from PD may be perfectly
reachable by its peers, and Run 4's own numbers cannot distinguish the two. A PD that forced a
configuration on that evidence would create two groups both believing they are region 27, both
accepting writes, both acknowledging them. Unreadable keys are recoverable when the machine comes
back; two divergent histories of the same key range are not. The judgement that a store is never
returning is a fact about the world — someone has to look at the machine — which is exactly why
the door is a command a person types and not a rule PD applies.

**And the command is lossy by construction, so it must say so.** Forcing a configuration from one
survivor silently drops anything the lost majority committed that the survivor had not yet
received. That is a real, quiet data loss, chosen deliberately over an unavailable range. It
belongs behind a name with `unsafe` in it, a printed summary of what is about to be discarded,
and an explicit confirmation.

**The real defence is not being at two voters**, which is unit 1 above and where the effort
belongs. The boundary moves with the target: three replicas survive one loss, five survive two.
A region sitting under its target is not an untidy region, it is a region with its margin
already spent.

## Consequences

- `schedule::Urgency::BelowQuorum` exists and is *issued but not promised*. A stretch of it in a
  debug log is the signal that repair has run out of road; it is not something to wait out.
- `esker pd inspect` has no staleness view today, and the future `unsafe-recover` needs one:
  regions whose last beat is old while their stores are live. Both are phase-4f/5 work, not this
  change (`TODO(phase-4f)` is deliberately *not* written into the scheduler, which has no place
  to detect an absence from).
- The recovery path cannot reuse the operator channel, so ADR 0013's "at most one operator per
  region, riding the heartbeat" is untouched by it.
- The acceptance run's `verify` failure stands as recorded. This change would have prevented it
  by never letting region 27 reach a bare two voters with a spare store available; it would not
  have rescued it afterwards, and no amount of PD-side cleverness could have.
