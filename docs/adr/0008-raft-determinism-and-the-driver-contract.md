# ADR 0008 — Determinism in `esker-raft`, and persistence as a contract

Status: accepted (phase 3, `esker-raft`)
Date: 2026-08-30
Context: `CLAUDE.md` invariant 4, `docs/DESIGN.md` §5, `docs/plans/phase-3.md` §4 and §7

## Context

`CLAUDE.md` invariant 4 requires `esker-raft` to be a pure state machine: no threads, no timers, no
sockets, no file I/O. That settles *what* the crate may not do. It does not settle two questions
that follow from it, and both have several defensible answers:

1. Raft needs randomness — election timeouts must not collide — but the crate may not have ambient
   entropy. Where does it come from, and what guarantees does it give?
2. Raft needs durability — a vote must be on disk before it is acted on — but the crate may not
   write. Who enforces the ordering, and how do we know they did?

## Decision

**1. The RNG is injected, and the node id selects its stream.** `Config::new(id, voters, seed)`
builds an `esker_base::rng::Pcg32` as `Pcg32::new(seed, id)`, using PCG's sequence parameter. A
whole cluster may therefore be constructed from one seed and still have every node drawing a
different sequence.

**2. The election timeout is redrawn at the start of every election**, including every pre-vote
round — not once at construction.

**3. No `HashMap` appears anywhere in the crate.** Per-peer progress and per-election votes are
sorted `Vec`s with binary search.

**4. Persistence is the driver's, and the ordering is a documented contract on `Ready`, not an
enforced one.** The core produces `Ready`; the driver persists `hard_state` and `entries` with
fsync, applies the snapshot first if there is one, applies committed entries in order, answers
reads only past their index, and then calls `advance`.

## Rationale

**Why the id selects the stream.** The obvious injection — hand every node its own seed — makes a
failing simulator run reproducible only if the harness records every seed it used. Deriving the
stream from the node id means one number reproduces a whole cluster, which is what a seed is for.
It also removes a failure mode we would otherwise have to test around: two nodes constructed from
the same seed would otherwise draw identical timeouts and tie in every term forever. That is now
impossible by construction, and `nodes_sharing_a_seed_still_draw_different_election_timeouts` pins
it.

**Why redraw every election.** A timeout fixed at boot makes a tie permanent: two nodes that
happened to draw the same number tie in every subsequent term too. Redrawing makes a tie a delay
rather than a livelock. We redraw for pre-vote rounds as well, which etcd does not — `becomePreCandidate`
deliberately changes nothing but the role there. The argument for etcd's choice is that a pre-vote
costs the cluster nothing, so a repeated tie is harmless; the argument for ours is that a repeated
tie is still a repeated round trip, and the redraw is free. `a_group_sharing_one_seed_still_elects_someone`
exercises five nodes on one seed across thirty-two seeds.

**Why no `HashMap`.** Iterating one is a decision input — who to send to, which peer to step down
for — and its order depends on hash seeding rather than on state. That would make a simulator trace
irreproducible under some seeds and not others, which is the worst way for a test to fail. A sorted
`Vec` of a handful of peers is also faster than a map at this size.

**Why the driver persists.** This is the direct consequence of invariant 4, and the reason the
invariant is worth its cost. If the core could write, "persist before send" would be an internal
detail nobody outside could observe, and the simulator could not test a driver that gets it wrong.
Because the core cannot write, the ordering is visible at the boundary: `esker-sim` drives a
deliberately incorrect driver and checks that safety breaks, which is a test we could not otherwise
write.

The rule is load-bearing beyond message ordering. A leader counts *itself* as holding an entry the
moment it appends one, before any fsync. That is sound only because no follower can acknowledge the
entry until the leader has sent it, and it may not send until it has persisted — so a quorum of
acknowledgements is a quorum of durable copies. Reorder those two steps and the count starts lying.

## Consequences

- Every failing simulator or property-test run is reproducible from `(seed, schedule)` alone. No
  wall clock, no OS entropy, no hash seed enters any decision.
- A driver that violates the ordering produces a cluster that loses acknowledged writes and looks
  healthy doing it. The contract is therefore documented as five numbered rules on `Ready` itself,
  in enough detail to test against, and `docs/raft-spec.md` §"The driver contract" gives each rule a
  row like any rule from Figure 3.1.
- `esker-store` (phase 3e) and `esker-sim` each implement the contract independently. Two
  implementations of a rule is two chances to get it wrong; the mitigation is that the simulator's
  is written to be checked and the store's is checked by it.
- Nothing in the crate can be made concurrent later without revisiting all of this. That is
  intentional: the concurrency belongs above, in the store.

## Alternatives considered

- **An `Rng` trait rather than a concrete `Pcg32`.** Rejected for now: it adds a type parameter to
  `RawNode` and every type that holds one, for a flexibility nothing has asked for. `esker-base`
  already forbids a second generator, so the trait would have exactly one implementation.
- **Enforcing the persistence ordering in the core** — for example by refusing to emit messages
  until the driver acknowledged the previous `Ready`. This is roughly etcd's `AsyncStorageWrites`
  mode. It is safer against a careless driver, and it is more machinery than a two-driver project
  needs; the contract plus a simulator that violates it deliberately covers the same ground. Worth
  revisiting if a third driver appears.
- **Seeding each node independently and recording the seeds.** Rejected: it makes reproduction a
  property of the harness rather than of the seed, and every harness would have to get it right.
