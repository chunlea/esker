# ADR 0061 — a placement driver joins a group it is told the name of, as a learner, before it votes

Status: accepted (phase 15) · Date: 2026-09-04
Context: `CLAUDE.md` invariants 1, 4, 5 · `docs/DESIGN.md` §7, §15 ·
`docs/plans/phase-15-pd-ha.md` §11 · [ADR 0011](0011-pd-service-and-the-cluster-id.md),
[ADR 0013](0013-repair-operators-are-requests-not-commands.md),
[ADR 0059](0059-pd-is-a-raft-group.md)

## Context

[ADR 0059](0059-pd-is-a-raft-group.md) made the placement driver a Raft group and said membership
was **configuration**: the same `--peers` list on every member, never changed at run time. It also
said, twice, what dynamic membership would cost — *"the group id has to move into the log, minted
once, like the cluster id"* — and left it as a phase of its own. This is that phase.

Four things have to be decided, and the first is the one that cannot be deferred.

1. **What identifies a group whose membership changes.** ADR 0059 derives the group id from the
   sorted member list, and a member refuses any Raft batch that does not carry its own. Adding a
   member changes the list, so it changes the id, so every member refuses every other's traffic —
   the group partitions itself at the moment it grows. A derived id and a moving membership are
   incompatible, and the derivation is the half that has to give.
2. **How a new member learns where the others are, and how they learn where it is.** A
   configuration is a set of node *ids*; a transport needs addresses.
3. **Single-server changes or joint consensus.**
4. **The order of a replacement**, which is the only part of this an operator will run under
   pressure: a member is gone, the group is one failure from losing quorum, and the sequence has to
   be one that never asks a quorum of members that are not there.

## Decision

**A group's id is derived once, from the membership it was founded with, and then persisted.
A member joining an existing group is told that id and writes it before it starts. A member's
address rides in its own conf change's `context`. Changes are single-server, and a member is added
as a learner and promoted only once it has caught up.**

```rust
// PersistedState, format version 2: what a member needs before it has applied anything.
pub struct PersistedState {
    pub hard_state: HardState,
    pub conf_state: ConfState,
    pub applied_index: Index,
    pub truncated_index: Index,
    pub truncated_term: Term,
    pub group_id: u64,                       // new: minted once, never recomputed
    pub members: Vec<(NodeId, String)>,      // new: the address book
}

// esker-raft's own type, with the address in the bytes it already carries for a caller.
ConfChange { kind: AddLearner, node: 4, context: encode(address) }
```

## Rationale

**The id is derived once rather than minted at random, because a rolling upgrade must be a
no-op.** Every placement driver running today computes its group id from its `--peers` list on
every start. If this release minted a fresh one, the first member upgraded would refuse the others'
traffic and the group would stop — an upgrade that takes a cluster down to add a feature nobody
asked for yet. Deriving from the *founding* list and writing the answer down means the id a
deployment has today is the id it keeps, and the change is invisible until somebody adds a member.
A version-1 record decodes as group id zero, which is the signal to mint from the configured list
and write it: one branch, taken once per member, ever.

**The record wins over the command line, because `esker-raft` already decided that.** `Config`'s
documentation says a restarting node takes its membership from its log, "never from a command line
that may be out of date" — and after a membership change that is exactly what `--peers` is. The
same rule now covers the address book and the group id, so an operator who forgets to update a unit
file gets a warning rather than a member that has quietly rejoined the wrong group.

**The address goes in the conf change's context, not in a second entry.** `ConfChange` carries
opaque caller bytes — the type's own documentation offers "a store id, a peer address" — and
`esker-store` has used them for exactly this since phase 4. One entry means the voting set and the
address book cannot disagree about *whether* a change happened. A separate `Command` carrying the
address would be two entries for one change, and every interleaving of two entries is a state
somebody has to reason about.

They can still disagree about *when*, for one `Ready`: a configuration is in force from the moment
its entry is appended (dissertation §4.1), and a state machine sees it on apply. So the route is
learned at append, before that `Ready`'s messages go out. This is not a new rule; it is
`esker_store::peer::learn_routes`, and it exists there because the alternative was a leader unable
to address the peer it had just added.

**The address book lives in `PersistedState`, beside the conf state.** It answers the same question
those fields answer — what does this member need *before* it has applied anything — and a member
that is catching up has no state machine to read. Putting it in the `default` column family would
make the ability to reach the group depend on having already reached it.

It also has a mechanical consequence worth stating, because it is what let this be built at all
while `crates/esker-pd/src/transport.rs` belongs to another lane: `MemberList` now carries an
explicit id, so `MemberList::group_id()` answers the *recorded* one and derives only for a founding
group. The transport reads that method and needs no change. Without it, every batch it sent would
carry a derived id that moved the moment a member was added.

**Single-server changes.** `esker-raft` implements them and this lane may read that crate but not
change it — but it is also the right answer here rather than merely the available one. Joint
consensus buys a change of several members at once; `pd members add` and `pd members remove` move
one, an operator runs them one at a time, and `RawNode::propose_conf_change` already refuses an
overlapping one. `docs/DESIGN.md` §15 keeps the question open where it is live: the *store's*
groups, where a scheduler issues the changes and might one day want to move two peers together.

**A learner first, and the arithmetic is the argument.** The case that matters is the one an
operator runs under pressure — three members, one gone, quorum 2 of 3 with two live:

* `AddVoter(4)` **deadlocks the group.** The configuration takes effect on append, so the quorum
  becomes 3 the instant the entry is on disk; three members are configured-and-live only if the new
  one counts, and it has an empty log. The entry that made the quorum 3 needs a quorum of 3 to
  commit, and there are 2. The group has stopped, and undoing it needs the quorum it no longer has.
* `AddLearner(4)` commits, because **a learner is not in the quorum**. It catches up — by snapshot,
  which for a placement driver is a couple of hundred kilobytes. Then `AddVoter(4)` takes the quorum
  to 3 with three live, and commits. Then `Remove(dead)` returns it to 2 of 3.
* Removing first would work and is worse: 3 → 2 leaves a quorum of 2 out of 2, so both survivors
  must answer every commit until the replacement lands. Add-before-remove never has fewer live
  members than it needs, which is the same sentence ADR 0013 uses about region replicas.

**So `pd members add` is a reconciliation, not a script.** Three proposals with a catch-up between
them is three places to be killed, and an operator who reruns the command must not be told the
member already exists. It looks at what is there — nothing, a learner, a voter — and does what is
missing. The kill test is the point of the shape, not an afterthought to it.

**The client's rule about a hint outside its list has to change, and the group id is what replaces
it.** ADR 0059 had a store refuse a `PdNotLeader` naming an address it was not configured with,
because following one would let another cluster's placement driver route it. Under dynamic
membership that address may simply be a member added since the store started. So the refusal
becomes a **refresh**: ask the member that gave the hint for its `Members`, and adopt the list —
*if its group id matches the one this client first learned*. The protection is unchanged and the
growth is allowed, because the group id is now the stable thing it was not before.

## Consequences

- **`PersistedState` goes to format version 2**, and a version-1 record still reads: it decodes as
  group id zero and an empty address book, which is exactly the state a pre-upgrade member is in.
  No golden file covers it — it is phase 15's own record, pinned by round-trip tests — and both new
  fields are appended, so the existing ones are byte-identical.
- **`Method::PdMemberChange` (`0x030d`) joins the wire, beside `Pd::Members` (`0x030c`)**, with its goldens. An addition: no existing
  golden line moves.
- **`MemberList::group_id()` means two things now**, and the type says which: the recorded id when
  the group has one, the derived id when it is being founded. A group that has never changed
  membership cannot tell the difference, which is what makes the upgrade invisible.
- **A membership change costs every connection.** `PdWiring` replaces the whole transport rather
  than adding one queue to it, because `crates/esker-pd/src/transport.rs` belongs to the `tls` lane
  until its RPC unit lands. Raft retransmits and a change is an operator action, so the cost is a
  reconnect nobody times; the shape it should collapse into is written in the plan's §11.7.
- **Five members are now reachable and three are still what is tested.** `MAX_MEMBERS` allows the
  transient fourth an add-before-remove needs. A deployment that grows past three is outside what
  this phase measured.
- **Nothing schedules a membership change.** PD repairs *region* replicas because it can watch a
  store go quiet; there is no equivalent observer for its own group, and inventing one would be a
  placement driver deciding to replace itself on evidence it is the only judge of.
