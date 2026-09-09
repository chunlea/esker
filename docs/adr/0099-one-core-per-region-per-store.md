# ADR 0099 — A region has one core on a store, and registering claims a place rather than taking it

Status: **accepted** (ruled 2026-09-09) · Numbered 0099 by the coordinator, who holds 0100 as the
next free number; 0094 is the highest on `main` and 0095–0098 are claimed in other lanes.

## Context — one line of a panic message, which no single core can produce

`docs/plans/debts-v1.1.md` #9 caught a three-store cluster in this state. A write had been refused
for ninety seconds with `peer is not the leader of region 53`, and the three stores said:

```text
store 1: region 53 term=7   is_leader=false believes_leader=None  raft_role=Leader   voted_for=Some(54)
         Counters { campaigns_pre: 1108, campaigns_real: 91, vote_requests_sent: 2398, … }
store 2: region 53 term=263 is_leader=false believes_leader=Some(54) raft_role=Follower
store 3: region 53 term=263 is_leader=false believes_leader=Some(54) raft_role=Follower
```

Read it as one core and it is nonsense twice over. `Raft::become_leader` sets `role = Leader` and
`leader = Some(self.id)` two lines apart, and `PeerCore::publish_leader` runs after **every**
message the driver handles as well as after every drive — so a core that is being driven cannot
have `raft_role=Leader` beside a published `believes_leader=None`. And `campaigns_real` counts
`become_candidate`, each of which adopts a term, so ninety-one of them cannot end at term 7.

Read it as **two** cores and every field falls into place. Stores 2 and 3 are right: peer 54 is
their leader at term 263, and its core says so. What is wrong is that the handle store 1's request
path holds is **not that core's handle** — it is an older peer's, frozen at the moment its core was
displaced, and `RaftPeer::is_leader` reads exactly the atomics that peer stopped publishing.

The mechanism was four ordinary lines, each reasonable alone:

1. `RaftPeer::start` handed its core to the driver pool **before** any caller had decided whether
   this store may host the region — it must, since the peer has to exist to be offered;
2. `DriverPool::register` was `cores.insert(region_id, core)`: it **replaced** whatever core was
   registered under that id and said nothing, dropping the displaced core without failing what it
   owed its callers;
3. both callers write the region map **after** that, and the map refuses five different things —
   `RegionMap::insert` refuses a duplicate id and an overlapping range, `RegionMap::apply_split`
   refuses a parent this store does not host, a child it already hosts, and a parent whose start
   key moved;
4. on any of those refusals the caller's `?` returned — and the peer it had built stayed alive
   inside the ticker task spawned for it, so **nothing ever stopped it**.

What that leaves is a live core: ticking, campaigning (`campaigns_pre: 1108` — and, for a split
child, campaigning deliberately, because [ADR 0094](0094-a-split-childs-leader-is-the-parents-leader.md)
asks it to), winning elections, answering every Raft message and every `status`. Beside it sits the
handle the store actually uses, publishing nothing. The region is unwritable for the life of the
process, and it is invisible: the store's own report, its region heartbeat and its scheduling all
read the same frozen atomics, so PD sees a region nobody leads and stops repairing, splitting or
promoting it — a **permanent** core-versus-PD disagreement out of the same root.

`RaftPeer::stop` had the same confusion pointing the other way: it retired **by region id**, so a
handle dropped after its region had been taken over stopped the core that had taken it.

## The options

**(a) Registering refuses a region the pool already drives.** One line at the claim, and both
callers' `?` are already in the right place.

**(b) Reserve the map entry before building the peer.** Most correct in principle; it means moving
`apply_split`'s three judgements ahead of the peer and duplicating the map's rules at both callers.

**(c) Each caller stops the peer it built when the map refuses it.** Smallest, and it leaves
correctness to every future caller — and, with retire-by-id, a caller doing it late stops somebody
else.

## Decision

**(a) and a reservation, not (c).** (a) alone closes only the branch where a core is *displaced*;
the other four refusals do not need a core to be there already, so registration succeeds and the
orphan is created by the map's refusal rather than by the register. So:

1. **`DriverPool::register` refuses a region this pool is already driving** and returns a `Token`
   naming the registration. The claim is taken in the pool, under its own lock, and not in a
   worker: `adopt_split` registers a child from the **parent's driver thread**, which can be the
   same worker, so waiting for a worker to answer would be a thread waiting for itself.
2. **`RaftPeer::start` returns a `Reservation`, not the peer.** The caller holds it until the
   region map has taken the peer, then calls `commit`. Dropping it uncommitted gives the region
   back — without waiting, for the same driver-thread reason. **No caller has to remember**, which
   is the property that matters: the branches that leaked were the ones nobody was thinking about.
3. **Retiring names the registration.** `RaftPeer::stop` passes its token, and a worker retires a
   region only if the token still matches, so a superseded handle stops nothing.
4. **The ticker and ADR 0094's campaign move after `commit`.** A peer that is only reserved should
   not be counting time, and above all should not be *campaigning*: a campaign is the one thing a
   peer does on its own initiative, so a child this store turns out not to host would otherwise
   campaign — and go on campaigning — for a region nothing on this store can serve.

## Consequences

- A store cannot drive two cores for one region, and the refusal is loud rather than silent: a
  second `host_region` for a hosted region now fails at the claim with `region N is already being
  driven by registration T`, where it used to fail at the map having already displaced the core.
- `Reservation` is `#[must_use]`, so a caller that ignores it is a compile-time warning rather than
  a leak.
- The worker keeps a token beside each core and logs an error if a registration ever displaces one
  it still holds — the state this ADR makes impossible, said out loud if it happens anyway.
- The permanent core-versus-PD membership disagreement described above is closed with it: a region
  whose handle is live is a region whose leader-side heartbeat reaches PD.
- ADR 0094 keeps its campaign; it just runs one line later. The 62 ms it removes is unaffected —
  the map write is a lock and a `BTreeMap` insert, not I/O.

## The red test

`every_refusal_leaves_nothing_driving_the_region_it_refused` (`crates/esker-store/src/server.rs`)
walks all five refusals and **collects** rather than stopping at the first, because the answer
wanted is which branches leak. Against the code as it was — register replacing, the ticker holding
the peer across the refusal, nothing giving the region back — it reads:

```text
an overlapping range left region 41 driven (region 41 [b"a", b"b") overlaps region 1 [b"", b"") already on this store)
  a split of a region this store does not host left its child 43 driven (region 42 split, but this store does not host it)
  a split that moved its parent's start key left its child 44 driven (a split moved region 1's start key, which a split never does)
  a second host of region 1: its peer is no longer the core that answers for it
  a split into region 1, which this store already hosts: its peer is no longer the core that answers for it
```

Three peer-level tests pin the parts: `a_peers_own_core_is_the_one_that_answers_it` (which read
`peer 1 was answered by peer 2` before the fix, and is the one that turned #9's panic line into a
mechanism), `a_reservation_nobody_committed_gives_its_region_back`, and
`a_superseded_handle_retires_nothing`.

## What this does not claim

**Which of the five refusals fired on the run that produced #9's line is not known.** Tracing was
off and the log is the panic message. The state is proven — two cores under one region id, and the
argument above is exhaustive over a single core's reachable states — and the class is closed here;
which member of it was hit that night is not, and no future reproduction is needed to justify the
fix, because every member produced the same permanent stall.
