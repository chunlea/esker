# ADR 0094 — A split child's leader is the parent's leader (draft)

*Status: **draft**, for the coordinator to rule on. Numbered 0094 at `78d7a676`, where 0093 is the
highest; a later committer renumbers. The brief that asked for it reserved "0095" — 0094 was free.*

## Context — measured, not assumed

`Store::adopt_split` brings a child region up with `start_peer` and `spawn_ticker` and nothing
else. There is no campaign, no leadership inherited from the parent and no term carried across, so
**every replica of the child begins as a follower** and the group waits out an election timeout
before anyone stands.

`how_long_a_split_child_has_no_leader` (`crates/esker-sql/tests/routing_differential.rs`) samples
the stores' own region maps every two milliseconds while a table splits under a loader:

```text
4,001 rows, 132 regions, 132 children measured from first sighting to first leader
min 0 ms   median 62 ms   p90 77 ms   max 93 ms
```

**A median of 62 ms with no leader, once per split.**

Two things about that number are worth keeping. The first instrument, which sampled **PD**, reported
all 130 children as led at **zero** milliseconds — PD hears of a child at the next region heartbeat
and the election is over by then. An instrument that reports zero looks exactly like a system with
no window. The second is that the window is usually **free**: 62 ms is inside the client's
`NotLeader` retry budget, and the load above took **zero** refusals across 132 splits.

It is not free for a proposal that was **already in the log** when leadership moved. That one cannot
be retried transparently, because the client cannot know whether it applied — it is answered
`40003 the transaction's outcome is unknown … region N stopped leading with this proposal in its
log; it may still commit`, which is what the batched loads met at 37 and at 76 regions. A bulk load
wide enough to always have a proposal in flight meets it on most splits.

## Decision (proposed)

**The child starts led by the store that led the parent.** The parent's leader knows three things at
the moment it applies the split — that it is the leader, that the child's membership is the parent's,
and that the child's log is empty — and that is everything an election would have established.

Two mechanisms would do it, and the choice is the substance of this ADR:

### (a) The parent's leader campaigns its child immediately

`adopt_split`, on the store that leads the parent, calls the child's `campaign` as soon as the peer
is started. The other replicas stay followers and answer the vote.

* **`esker-raft` is untouched**, which matters: it is a pure state machine (`CLAUDE.md` invariant 4)
  and `campaign` is already part of its surface. The change is one call in `esker-store`'s split
  driver.
* It is still an election — one round trip to a quorum — so it removes the *timeout* (the ~60 ms of
  waiting) and not the round trip. The window becomes a few milliseconds rather than sixty.
* **It cannot elect the wrong node**: the other replicas grant or refuse by the ordinary rules, and
  [ADR 0085](0085-a-vote-is-not-granted-to-a-learner.md)'s guard still applies — a vote goes to a
  peer this configuration holds as a voter, and the child's configuration is the parent's, so the
  parent's leader is a voter in it.
* Two replicas could campaign at once only if two stores each believed they led the parent, which is
  the pre-existing split-brain question and not one this creates.

### (b) The child is started **as** the leader, with the parent's term

`RawNode` starts in a state that says leader at the parent's term, with no election at all.

* Removes the round trip as well as the timeout: the window becomes zero.
* But it puts a *state machine* decision in the driver, and the honesty cost is high: a peer that
  declares itself leader without a vote is exactly what pre-vote and the lease exist to prevent, and
  a store that was **no longer** the parent's leader when it applied the split — it can be, since
  applying is asynchronous with leadership — would mint a second leader for the child's term.
* Making that safe needs the parent's leadership to be re-checked at apply time and the child's term
  to be chosen so that a competing election supersedes it, which is a Raft change in a crate that is
  deliberately a pure state machine.

**Recommendation: (a).** It removes the part of the cost that is large (a timeout) and leaves the
part that is small (a quorum round trip), and it does it without teaching the driver to fabricate
leadership. (b) buys the last few milliseconds for a rule that this system has twice paid to get
right.

## What it does not change

* **PD** learns of the child at the next region heartbeat either way; nothing here makes it earlier,
  and nothing here needs it to be. The client reaches the child through the store's own refusal
  carrying its bounds (ADR 0073's repair), not through PD's schedule.
* **The epoch** is the split's, unchanged: a child that elects at term 1 or is campaigned into term 1
  carries the same `(conf_ver, version)` its record was written with.
* **The `40003` is not removed, it is made rare.** A proposal in flight when leadership moves is
  ambiguous whatever replaces the election, and a split still moves leadership from the parent's
  perspective for the keys that leave it. What (a) removes is the sixty-millisecond window in which
  *every* arriving write is in that position.

## The red test

`bulk-load into a table that splits at least fifty times does not produce a `40003``, in the batched
shape that produces it today — 250 rows a statement, which is what met it at 37 and at 76 regions.
The unbatched loader must stay green too, since it already is: a fix that made the narrow case work
by making the wide case worse would pass the first and fail the second.
