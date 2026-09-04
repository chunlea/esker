# ADR 0059 — the placement driver is a Raft group, and its durable records are the log's

Status: accepted (phase 15) · Date: 2026-09-04
Context: `CLAUDE.md` invariants 1, 4, 6 · `docs/DESIGN.md` §7, §9 · `docs/plans/phase-15-pd-ha.md`
· [ADR 0009](0009-the-wire-carries-the-raft-message.md),
[ADR 0010](0010-pd-durable-state.md), [ADR 0011](0011-pd-service-and-the-cluster-id.md),
[ADR 0013](0013-repair-operators-are-requests-not-commands.md)

## Context

Phase 4 shipped one durable PD and recorded the cost of that in `docs/plans/phase-4.md` §15: while
PD is down, `Tso`, `AllocId`, `Bootstrap`, a cold-cache `GetRegion` and all scheduling stop. Phases
5 and 6 then built on top of it, so the exposure is no longer "availability of change" — a cluster
whose PD is gone cannot start a transaction, and therefore cannot serve SQL at all.

`docs/DESIGN.md` §7 has always said how it ends: three PDs replicated with `esker-raft`. What it
does not say is any of the five things a reader could reasonably reverse.

1. **What goes in the log.** PD holds four kinds of state (identity, routing table, allocator,
   oracle) and three kinds of memory (in-flight operators, cooling, settling load). Replicating
   all seven is possible and wrong; replicating the wrong four is subtly wrong.
2. **Whether the log carries requests or decisions**, given that `Bootstrap` mints a cluster id
   out of a wall-clock reading and `Tso` is a wall-clock reading.
3. **When a new leader may answer.** Raft promises a new leader's *log* holds every committed
   entry, and PD reads its oracle out of *applied* state.
4. **What a follower says**, and how the asker finds the leader.
5. **What identifies the group**, given that PD's Raft group has to exist and elect before the
   cluster id of ADR 0011 has been minted.

The third is the one that can lose data silently. `docs/plans/phase-4.md` §15 already named it as
the thing to check first when 4e opened: *"the mark's commit must be observed, not merely
proposed, before a timestamp above the old mark leaves"*.

## Decision

**PD is a Raft group of up to three members over `esker-raft`. Its four durable records become
commands applied by a deterministic state machine into the same `esker-engine` database, in the
same key space, under the same bytes as ADR 0010. Only the leader serves, and only after it has
applied a `TakeOffice` entry of its own term. Every non-deterministic input the leader sampled
travels inside the command.**

```rust
enum Command {
    TakeOffice { term: u64, now_ms: u64 },
    Bootstrap  { store_id: u64, address: String, base_id: u64, cluster_id: u64, now_ms: u64 },
    ReserveIds { end: u64 },                 // 'm' 'a'
    AdvanceTso { mark: u64 },                // 'm' 't'
    StoreBeat  { .., now_ms: u64 },          // 'm' 's'
    RegionBeat { .., now_ms: u64 },          // 'm' 'r' + 'm' 'k', answering Upsert
    Columnar   { wishes: Vec<ColumnarWish> },// 'm' 'l'
    History    { event: OperatorEvent },     // 'm' 'h'
}
```

- One database, two column families: `default` for the records of ADR 0010, `raft` for PD's Raft
  log, hard state and applied index. One WAL, so an apply writes the record and the apply index in
  one atomic batch.
- The scheduler does not move. The in-flight set, the cooling map and the settling deltas stay
  memory, leader-only, re-derived from the next round of heartbeats.
- `Method::PdRaft = 0x030b` carries `PdRaftBatch { group_id, from, messages }`, where a message is
  `esker_raft::Message` under ADR 0009's codec. `group_id` is `mix64` over the sorted member
  addresses.
- `ProtoError::PdNotLeader { leader_id, leader_address } = 19` is what a follower answers, to
  every method.

## Rationale

**Four records in the log, three sets of memory out of it.** The line is not "important versus
unimportant", it is *whether the cluster can tell PD again*. Membership, the routing table and the
two counters cannot be re-derived from anything — the counters least of all, since the whole of
ADR 0010 is that they must not repeat. The in-flight set can: a heartbeat re-states a region's
membership every ten seconds, and ADR 0013 already made repair a pure function of (routing table,
liveness, in-flight). Replicating it would mean reconciling a remembered plan with a cluster that
moved on while a leader changed, which is strictly harder than recomputing — and phase 4 already
tests the recompute, because a PD that loses leadership is, to the scheduler, a PD that restarted.

**The log carries the decision, not the request, wherever a clock is involved.** Three members
that each read `SystemClock::now_ms()` inside `apply` diverge, and a state machine that diverges is
not a state machine. It would also be a second place in Esker that orders on a wall clock, which
invariant 6 forbids in as many words. So the leader samples the clock once and the number travels
in the command; `apply` is a pure function of `(applied state, command)`, testable without a
cluster and identical on all three. The same argument decides `Bootstrap`: the cluster id is minted
by the leader out of `(now_ms, store_id, address)` and carried, rather than re-minted per member.

Where no clock is involved the log carries the request and `apply` decides, because that is where
the ordering *is* the decision. `RegionBeat` is the case: the epoch guard compares the beat against
what PD holds, and letting the log order settle which beat is newer is exactly what the guard
means. The leader reads nothing before proposing, and the `Upsert` verdict comes back out of the
apply.

**`max`, not assignment, on the two counters.** `allocated_end` and `high_water_ms` only ever
advance. Writing `max(carried, held)` makes an apply idempotent in the one direction that matters,
so that a re-proposed entry from a recovering leader cannot walk a mark backwards. It costs a
comparison and removes a class of reasoning.

**A new leader waits for `TakeOffice`.** This is the decision that keeps ADR 0010's guarantee
alive. Raft's Leader Completeness says a new leader's *log* contains every committed entry; it says
nothing about `applied`. PD's oracle is rebuilt from applied state, so a leader that answered a
`Tso` before applying the last committed `AdvanceTso` would resume below a mark that is already
durable, and would hand out a timestamp the previous leader had already given away. That is the
lost update ADR 0010 exists to prevent, reintroduced one layer up.

So the driver proposes `TakeOffice { term, now_ms }` on winning an election and answers
`PdNotLeader` — naming itself — until it applies. Applying it means every entry before it has
applied, which is every entry committed under any earlier leader. Then, and only then, the leader
rebuilds `Allocator::load(allocated_end)` and `Oracle::load(high_water, now_ms)`: the same two
constructors a *restart* already uses, which is the point. A failover is a restart that kept its
socket.

`esker-raft` appends its own empty entry on taking office and this is not a duplicate of it. That
one is what lets an inherited log commit at all (§5.4.2); this one is what tells the state machine
it has caught up. Reading the core's `own_term_index` instead would have worked and would have
meant a new accessor on `esker-raft`, which this lane may read but not change.

**The mark is the lease, so PD needs no other one.** Two things could produce a colliding
timestamp: a deposed leader that has not noticed, and a new leader that starts too low. The
persisted mark answers both, and it answers them with a rule that is already written.

* A deposed leader is confined **below** the mark. Every timestamp it hands out has
  `physical < mark`; crossing the mark requires a commit, and its proposals no longer commit, so
  the call fails rather than answering. *Ack after commit, never after propose* is the whole of it.
* A new leader begins **at or above** the mark, because `Oracle::load` takes `max(clock, mark)`.

Nothing in that argument depends on either clock being right, or on the two agreeing. A leader
lease would add a third mechanism doing the job the mark already does, and would make correctness
depend on clocks, which is what invariant 6 is refusing.

**Reads do not go through the log, and that is not a compromise.** `GetRegion`, `ScanRegions`,
`Status` and the inspector read the engine directly. A deposed leader can therefore answer a
routing question from a state one entry behind — which is precisely the staleness `docs/DESIGN.md`
§7 designs for: a client's cache is a hint the store checks against its epoch, so a stale route
costs a redirect and never a wrong answer (invariant 5). `ReadIndex` exists in `esker-raft` and is
deliberately not used here: it would buy linearizable routing answers that nothing needs and would
put a round trip on the hottest read PD serves. The two answers that *cannot* be stale — an id and
a timestamp — are the two that go through the log.

**A separate error code, because `NotLeader` means something else.** `ProtoError::NotLeader` is
region-scoped: it names a region id, its hint is a *peer* id, and `esker_client::router` answers it
by repairing the region cache and retrying against the hinted peer. A PD redirect sent through that
code would either name region 0 — which is not a region — or poison a cache entry for a region that
does not exist. `PdNotLeader` is a different question with a different answer, so it gets its own
code and its own golden line. Its hint carries an **address** rather than a member id, so that
discovery needs no agreement between operator and client about the order the endpoints were listed
in; a client that was given three endpoints and gets back a fourth address treats it as a hint like
any other and still refuses to leave its configured set.

**A derived group id, because the cluster id does not exist yet.** ADR 0011 put a cluster id on
every PD request for a mistake people actually make: a stale `--pd` flag pointing at another
cluster, whose symptom without a check is not an error but an *answer*. PD's Raft group has the
same exposure and cannot use the same guard, because the group has to elect a leader *before*
`Bootstrap` — itself a log entry — has minted anything. Two PDs from different clusters pointed at
each other would otherwise form one group and replicate one cluster's routing table over the
other's, which is worse than the case ADR 0011 was written for.

`mix64` over the sorted member addresses costs eight bytes on a message that already carries a
Raft batch, needs no state, and is available at process start. Sorted, so listing the endpoints in
a different order on different members is not a different group; changed by adding a member, which
is correct today because a changed member list *is* a different group while membership is static.

**Static membership, and it is a non-goal on purpose.** Three members, listed identically on each,
no `pd member add`. Dynamic membership needs the group id to move into the log — minted once, like
the cluster id — and needs the single-server change rules `esker-raft` already implements to be
driven from an operator path that does not exist. It is a phase of its own, and shipping HA without
it is worth more than shipping neither.

## Consequences

- **`Pd::open`'s signature does not change**, and neither does the behaviour of a one-member PD:
  it campaigns inside `open` and is leader when `open` returns, with no runtime and no ticker. This
  is a hard constraint rather than a nicety — `esker-store`, `esker-sql` and `esker-cli` all call it
  and three of those trees belong to other lanes.
- **A 4a data directory opens unchanged** and gains the `raft` column family, which
  `Db::open_with` creates. There is no migration and no format-version bump: every record keeps its
  key and its bytes, and the goldens in `crates/esker-pd/tests/records.rs` are untouched. The log
  is a new key space, not a changed one.
- **`ProtoError` gains code 19 and `Method` gains `0x030b`.** Both are additions: no existing
  golden line changes, and both gain one, because the sweep in `esker-proto/tests/messages.rs`
  demands a golden per method and per code.
- **Every PD write costs a Raft round trip**, serialized. `AllocId` and `Tso` amortise it over a
  batch (1,000 ids, 3 s of timestamps) and pay it almost never; heartbeats pay it every time, so a
  thousand regions is on the order of a hundred round trips a second. Measured rather than assumed
  (`docs/bench/`), and if it is a problem the fix is batching applies, not weakening the ordering.
- **A crash now recovers more, not less.** A kill between the append and the apply leaves the entry
  in the log, and the restart replays it: an id reservation that used to be lost with the process is
  now recovered. The ordering being tested is unchanged, and `crates/esker-pd/tests/crash_kill.rs`
  keeps its own stated limit — a `SIGKILL` proves the ordering and the restart rule, not the fsync.
- **`esker-raft` gains nothing.** PD drives the same pure `RawNode` under the same five-step
  `Ready` contract. A core gap found here is written down and handed to the raft lane.
- **The failure mode this leaves** is the one Raft leaves: two of three members down and PD stops,
  because a minority cannot commit. It stops *refusing* rather than answering wrongly, which is the
  direction this whole file chooses every time it has a choice.
