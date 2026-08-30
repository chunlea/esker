# ADR 0013 — a repair operator is a request on a heartbeat, and PD forgets it on restart

Status: accepted (phase 4c)
Date: 2026-08-30
Context: `docs/DESIGN.md` §7; `prompts/04-multiraft-pd.md` 4c; `docs/adr/0010-pd-durable-state.md`;
`docs/plans/phase-4.md` §6 (race 5)

## Context

4c gives PD something it has never had: an opinion that other processes must act on. Replica
repair means PD deciding a region needs a new peer, and a store's Raft group actually proposing
the membership change. Three questions had to be answered before any of it could be written, and
each has a plausible wrong answer that would have looked fine for a long time.

1. **How does the operator reach the store?** PD has no connection to a store — stores call PD,
   not the other way round (`docs/DESIGN.md` §7, §9). So either PD grows a client and a store
   list to dial, or the operator rides on something the store already asks for.
2. **What does PD do while it waits?** An operator can be lost, refused, applied but unreported,
   or applied and reported late. PD cannot tell these apart.
3. **What survives a PD restart?** `prompts/04-multiraft-pd.md` names "PD restart mid-operator" as
   an explicit test, and §7 forbids two operators for one region — a rule that is trivially broken
   by a PD that forgets what it had issued.

## Decision

**An operator rides on the answer to that region's heartbeat; PD re-sends the same one until a
heartbeat shows what happened; and the in-flight set is memory that a restart discards.**

```rust
PdResp::RegionHeartbeat { operator: Option<Operator> }   // at most one, per region, ever
```

- Progress is read from later heartbeats only — the peer list and the epoch — never assumed.
- An operator carries the epoch PD believed the region was at, and the store refuses one that no
  longer matches.
- A restart re-derives need from `schedule::repair_for`, a pure function of (routing table, store
  liveness, in-flight set), and a re-derived `AddPeer` mints a **fresh** peer id.

## Rationale

**The heartbeat is already there.** Every region's leader beats every 60 s or on a change, so the
answer to that beat reaches exactly the peer that can act, at a cadence PD already pays for. The
alternative — PD dialling stores — needs a connection pool, a store-address book that is live
rather than advisory, retries, and a failure model for "PD cannot reach the leader", all to
deliver a message that will arrive on its own within one interval. It also inverts §7's
direction of traffic, which is what keeps a store's dependency on PD a *cache* rather than a
liveness requirement.

The cost is latency: a repair is noticed no sooner than `max_store_down_time` and delivered no
sooner than the next heartbeat. For repairing a replica after a 30-second outage, a further
minute is not the expensive part.

**Re-sending is safe because the operator is a request.** PD cannot distinguish a lost operator
from an applied-but-unreported one, so it does not try: it repeats itself until the data changes.
That is only sound because the receiver checks the epoch and its own membership before proposing
anything, so a duplicate is refused rather than applied twice. Making the operator a *command* —
something a store applies on receipt — would need exactly-once delivery between two processes
that share no log, which is the problem Raft exists to solve and is not one to re-solve here.

**Observation, not acknowledgement.** A store could acknowledge an operator directly, and the
acknowledgement would be faster than waiting for the peer list to change. It would also be a
second source of truth: an operator acknowledged and then lost to a crash would leave PD believing
something the cluster does not show. The peer list *is* the outcome, so it is the only thing read.
The one nuance is that a heartbeat showing an `AddPeer`'s new replica as a **learner** counts as
progress but not as completion — that is the store catching it up before promoting it, and PD
stops re-sending because the store demonstrably has the work.

**Forgetting on restart is simpler than remembering.** Persisting the in-flight set would mean
reconciling a remembered plan against a cluster that moved on while PD was down: the operator may
have applied, been refused, or been made irrelevant by a split. Every one of those has to be
detected from heartbeats anyway — so the remembered copy adds a second state to keep consistent
and removes nothing. Discarding it makes the restart path *identical* to the steady-state path,
which is why the restart test is not a special case of the code so much as a special case of the
data.

The cost is one wasted peer id per forgotten `AddPeer`, and that cost is exactly why a re-issue
mints a fresh one rather than reusing it: PD has no way to know whether the id it forgot is
already halfway into a Raft configuration, and two peers under one id is unrecoverable in a way
that a burned 64-bit integer is not.

**The timeout is on being stuck, not on taking long.** The clock runs from the last observed
progress. Catching a replica up from a snapshot is slow, and cancelling a transfer that is working
throws the work away and starts again somewhere else. But a timeout there must be: one operator in
flight means no second one, so an operator nobody acts on would block its region's repair for
ever. Writing that rule down is what found the bug — the first version returned "started" from the
effect check before consulting the clock, so a learner that stopped catching up was immortal.

## Consequences

- **`RegionHeartbeat`'s response is a struct now**, and `PdChannel::region_heartbeat` returns
  `Option<Operator>`. Four new goldens, and `TransferLeader` is encoded and reserved so that 4d's
  leader balance does not change the format when it arrives.
- **Repair latency is bounded by a heartbeat interval, and PD has no timer thread.** If that
  latency ever matters, the fix is a shorter region-heartbeat interval for regions PD is watching,
  not a scheduler loop — the cadence is the knob, and it is already in §14.
- **A store that ignores operators is not broken, only unrepaired.** That is what let the wire
  change land before the store side consumed it.
- **PD's in-flight set is invisible to `esker pd inspect`**, which reads a stopped PD's database.
  Anything that wants to see live operators needs a status endpoint on a *running* PD; that is an
  observability item for 4d, not a gap in the durable state.
- **4e inherits a scheduler with no persistent state**, which is the easy half of making PD three
  nodes: the routing table and the allocator go through Raft, and the in-flight set stays exactly
  what it is — the leader's own memory, re-derived by whoever holds the office.
