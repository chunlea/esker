# ADR 0094 — A split child's leader is the parent's leader

Status: **accepted** (ruled 2026-09-09, mechanism (a)) · Numbered 0094 at `78d7a676`, where 0093 was
the highest; the brief reserved "0095" and 0094 was free.

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

**Ruled: (a).** It removes the part of the cost that is large (a timeout) and leaves the part that
is small (a quorum round trip), and it does it without teaching the driver to fabricate leadership.
(b) buys the last few milliseconds for a rule that this system has twice paid to get right.

## What it changes, measured either side — and what it does not

**The structure, which is the property:**

| | children led by the store that led their parent |
|---|---|
| without the campaign | **35 of 63** |
| with it | **29 of 29**, and 130 of 130 on another run |

**The wall clock, which is not:**

```text
quiet box, before   median 62–73 ms   p90 77–94 ms   max 93 ms — and once 32,765 ms
quiet box, after    median 10 ms      p90 35 ms      max 87 ms
loaded box, after   median 87 ms      p90 150 ms     max 1,609 ms
```

**Read those two tables together, because the second one is why the first one is the ADR.** On a
quiet box the window is six times smaller and the half-minute tail is gone. On a loaded box it is
*larger than the before* — 87 ms against 62 — while the structure is unchanged at 29 of 29. A
duration here is a statement about the machine; **which store ends up leading is a statement about
the mechanism**, and it is the same under both.

So this ADR claims the second and not the first. The window shrinking is a consequence worth having
and not a property worth asserting, which is also why the gate test asserts leadership and the
distribution is printed by an `#[ignore]`d measurement beside it.

### Two things the build found that the design did not

**1. `tokio::spawn` on a driver thread is a panic, and a panic there stops the region.** `adopt_split`
runs on the parent's *driver* thread — a plain thread from `DriverPool`, not a reactor worker — so
`tokio::spawn` has no runtime to attach to. The first version did that, and the symptom was not a
crash in the log: the same load split **twice** instead of a hundred and thirty times and the writer
took 79 `08006`s, because the regions it wanted had stopped moving. The store's own runtime handle,
which `spawn_ticker` already uses, is the fix.

**2. One campaign is not enough, and the reason is the shape of a split.** Every replica creates the
child when *it* applies the split entry, and the leader applies first — so a campaign fired the
instant the leader adopts its child reaches stores that do not serve that region yet, and a Raft
batch for a region a store does not serve is **dropped** rather than refused. Measured: campaigning
once left the median exactly where it was, at 63 ms. It asks again for a handful of ticks and stops
as soon as there is a leader. Every attempt is an ordinary pre-vote, so a wasted one costs no term,
and if none of them lands the region elects on its timeout exactly as it did before.

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

## The red test, and why it is not the one this ADR first named

The draft named *"a bulk load into a table splitting fifty times does not produce a `40003`"*. **It
was run three times before anything was built and it failed one of the three**, so as a gate test it
would fail one run in three whatever the code did. Worse, the refusals it caught say
`region N stopped leading with this proposal in its log` — a peer that **had** leadership and lost
it, which is not the child that never had one. This ADR removes the second and says so above: the
`40003` is made rare, not removed.

The assertion that replaced it was **also wrong, and for the same reason one step further in**: a
median time-to-leader under thirty milliseconds. It passed on a quiet box, failed at 84 ms on a gate
running two chains at once, and — measured afterwards — comes out at 87 ms on a loaded box *with the
fix in place*. A duration is a performance property and a gate is the worst place to assert one; this
repository had written that down for the mpp differential the same night.

So the gate asserts the **structure**: `a_split_child_is_led_by_the_store_that_led_its_parent`, four
in five children led by the store that led their parent. Without the campaign it is 35 of 63 — not
the one-in-three a uniform election would give, because the parent's leader is likelier to win one
anyway — and with it, 29 of 29. The bar sits far from both. The distribution and the ambiguous
outcomes are printed by `#[ignore]`d measurements beside it.

**And one trap in measuring it, which cost a wrong conclusion before it was found.** The first
version of that test compared the parent's `leader_peer_id` with the child's and reported **1 of
130**, which reads exactly like a mechanism that never fires. It fires every time: a trace in
`adopt_split` showed one store per split reporting that it led the parent, on all hundred splits. A
**peer id is numbered per region**, so the parent's and the child's are ids in two different spaces
and comparing them answers no question. `is_leader` — each store's statement about itself — is the
thing both halves can be in.
