# ADR 0007 — The Raft message set: what we fold together, and why

Status: accepted (phase 3, `esker-raft`)
Date: 2026-08-30
Context: `docs/DESIGN.md` §5, `docs/raft-spec.md`, `docs/plans/phase-3.md` §3

## Context

Ongaro's dissertation defines four RPCs — AppendEntries, RequestVote, InstallSnapshot, and (in
chapter 3) TimeoutNow — and describes heartbeats as AppendEntries with no entries. Implementations
differ in how literally they take that. etcd/raft splits heartbeats into their own message type and
gives InstallSnapshot its own response; other implementations fold them.

We had to choose before the simulator lane could build against the type, so the choice is recorded
here rather than in a code comment.

## Decision

**1. A heartbeat is `AppendEntries` with no entries.** There is no separate message type.

**2. A snapshot is acknowledged with `AppendEntriesResponse`.** There is no
`InstallSnapshotResponse`.

**3. `AppendEntriesResponse` carries a rejection hint of *two* fields — an index and the term the
follower has there** — not an index alone.

**4. An `AppendEntries` carrying no entries does not consume a follower's in-flight window.**

**5. `AppendEntries` and its response carry a `context` field**, which a `ReadIndex` round uses to
tag the heartbeat it rides on. It is empty on an ordinary append.

**6. A `RequestVote` from an older term is ignored, not refused.** A *pre-vote* from an older term
is refused, with this node's term.

## Rationale

**Folding heartbeats** means one consistency check, one carrier for the commit index, and one
rejection path instead of two of each. The split in the paper is expository: nothing in the
algorithm needs a heartbeat to be a distinct message, and every rule that applies to an append
applies to an empty one. The cost is that a heartbeat could in principle be rejected, so
`send_heartbeat` anchors it at what the follower is already known to hold (`progress.matched`),
which cannot fail the check. That also gives us A5 for free: the advertised commit index is capped
at what the follower has, so it can never be told to commit something it does not hold.

**Folding the snapshot acknowledgement** follows the same reasoning one level up. Installing a
snapshot moves a follower's log to the snapshot's index; the acknowledgement answers exactly the
question an append response answers — *what index do you now match?* — and a separate type would
carry the same field down a second code path to the same place.

**The two-field hint** is what makes catch-up cheap. Backing off one index per round trip turns a
long divergence into a long outage: Figure 7's leader and its follower (f) differ over eleven
entries, which is eleven round trips. With the term in the hint, the leader discards every entry
above that term in one step and repairs the same divergence in two. That is measured, not asserted:
`the_rejection_hint_costs_a_round_trip_per_term_not_per_entry`.

**Not charging empty appends to the window** is a consequence of decision 1 that we got wrong
first. `max_inflight_msgs` exists to bound entry traffic to a slow follower. An empty append is a
heartbeat, and heartbeats are also how a follower learns its commit index has moved — so charging
them meant a leader with a small window could throttle the very messages that make a write visible.
The bug was found by a test asserting three messages and getting two.

**The `context` field** is the price of decision 1. etcd's `ReadIndex` rides on `MsgHeartbeat`,
which has a context field of its own; with heartbeats folded, the field has to live on
`AppendEntries`. It costs one byte on the wire when empty. The alternative — inferring
confirmation from any response whose index is high enough — is unsound: a response already in
flight when the round started would falsely confirm it.

**Ignoring a stale `RequestVote`** departs from Figure 3.1, which says to reply false. A candidate
a term behind cannot win whatever we say, and a node stuck in a campaign loop is better left
unanswered than kept company. A stale *pre-vote* is answered, because that reply is the mechanism
by which a node returning from a partition discovers how far behind it is — refusing to answer it
would leave the node campaigning forever.

## Consequences

- The wire format for phase 3e carries one message kind fewer than the paper describes, and one
  extra field (`context`) on the two most common messages.
- A future joint-consensus change (deferred; `docs/DESIGN.md` §5) does not interact with any of
  this: it changes who counts, not what is sent.
- Decision 6 means a client cannot distinguish "my vote request was refused" from "my vote request
  was dropped". Nothing depends on that distinction: a candidate that receives neither a grant nor
  a refusal simply loses the election, which is the same outcome.
- If we later want a leader to detect an unreachable follower faster, decision 1 makes it slightly
  harder — there is no message whose only purpose is liveness. The `recent_active` flag that
  check-quorum already maintains is the intended mechanism.

## Alternatives considered

- **Keep the paper's message set exactly.** Rejected: two of everything, for no algorithmic gain,
  in a crate whose whole value is being small enough to model-check.
- **A single-field rejection hint (index only).** Rejected on the measurement above.
- **A separate `Heartbeat` message carrying only the commit index and the read context.** This is
  etcd's design and it is defensible; it avoids the `context` field on `AppendEntries` and makes
  "a heartbeat never rejects" true by construction rather than by anchoring. We chose folding
  because the rule count matters more to us than the field count: every message kind is another row
  in `docs/raft-spec.md` and another case in the model.
