# 0035 — a snapshot ask waits for the record that names its peer

Status: accepted. Debt wave c3, unit 2, from `docs/plans/debt-c3.md` §2 — recorded as "the
`receive_raft` snapshot ask races the conf change".

## Context

A conf change takes effect in the Raft core when it is **appended** and in the region record when
it is **applied**. Between those two a leader already knows about a peer its own record does not
list — and that gap is exactly where the new peer asks for the region, because the traffic that
tells it the region exists is the traffic the leader started sending the moment it appended.

`Store::send_snapshot` checked the asking peer against the applied record, so it answered

```text
peer 5 is not a member of region 1 and may not have a copy of it
```

which is a correct sentence about the wrong membership. Recorded in
`docs/plans/phase-8-learner.md` §close bullet 4, with a fix suggested: *"the sender could check the
core's membership rather than the applied record."*

**That fix, implemented exactly as suggested, is worse than the refusal it removes**, and this is
measured rather than argued. The snapshot's header carries `source.region`, which is the *applied*
record. Serving on the strength of the core's membership therefore ships a header whose region
record does not list the peer receiving it. The receiver writes that record, writes every byte of
the region, and then `Store::host_region` declines to start it — correctly, because a record that
does not name this store is one it must not serve — logging a warning and returning `Ok(())`. The
transfer "succeeds", nothing is hosted, the leader announces again, and the whole thing repeats
for ever. `crates/esker-store/tests/promotion.rs` fails three times out of three with a learner
stranded for the length of the run: the phase-4 acceptance stall, reintroduced by the fix for a
retry. Reverting only that check turned it green twice in a row and eight seconds faster.

It is the shape this codebase keeps finding, in the direction nobody looks: a **transient**
condition — a conf change in flight — turned into a **permanent** one, this time by the code
written to stop it being permanent.

## Decision

**1. The core's membership decides whether the caller is a stranger, and the record decides when
it is served.** Three answers instead of two:

* the asking peer is in the **applied record** — served immediately, as before;
* it is in the **core's configuration** and not the record — the ask is legitimate and this store
  has not caught up. It is **held**, up to `RECORD_CATCHUP_WAIT` (500 ms), and served the moment
  the record lists it. A wait that expires refuses and says that it expired, which is where this
  was before: the leader's next announcement asks again;
* it is in **neither** — refused as a stranger, on sight, exactly as before.

**2. What the wait is waiting for is the change committing, and that is the point rather than an
accident.** The record moves when the conf change *applies*, which is after it commits, which is
after it can be rolled back by a new leader. A region shipped to a peer that a rollback removes is
a copy of a range with no owner, sitting on a store that will never serve it. So waiting for the
record is a stronger check than reading the core, not merely a more convenient one — and the ask
that the core alone would have admitted is precisely the one that must not be admitted yet.

**3. The wait is bounded, and the bound is what keeps a design mistake from becoming a deadlock.**
Adding a **voter** to a group that then needs it for a quorum cannot commit until that voter has
the region, which is what the wait is trying to give it. Nothing in this system does that —
`AddPeer` adds a learner and promotes it once it is caught up, and a learner's addition commits on
the existing voters alone — but a wait with no end would turn the day somebody tries into a hang
instead of a retry.

**4. The receiver refuses a snapshot whose record does not name it, before a byte is written.**
`Store::fetch_snapshot` checks the header's region for a peer on this store and refuses the
transfer if there is none. This is defence in depth against exactly the failure above: it costs
the same retry, leaves nothing durable behind, and turns a silent permanent stall into a loud
refusal. It also holds against a sender at an older version, which is the case no amount of care
in this build can cover.

## Consequences

A placed replica is served on its first ask in the ordinary case, instead of on the retry after the
leader's next announcement. That is the latency the debt was about, and it is now bounded by an
apply rather than by a heartbeat interval.

A snapshot request may now occupy a connection and a request task for up to half a second. It is
one request per placement, the wait is only entered for a caller the core has already vouched for,
and the alternative was a refusal the caller had to pay a heartbeat interval to retry.

`ProtoError::Unsupported` is what an expired wait and a mis-addressed header both answer, and both
are refusals a caller retries rather than errors that fail anything.

## Alternatives rejected

**Serve from the core's membership.** The fix as recorded. Measured above: it strands a learner
for ever and fails `tests/promotion.rs` 3 of 3.

**Send a region record with the asking peer added to it.** Makes the receiver start, and leaves it
holding a record whose `conf_ver` does not include the peer it lists. The conf change then arrives
down the log and applies as a no-op — `stage_conf_change` returns early when `moved.peers` equals
what it already has — so the `conf_ver` never bumps and the peer's record disagrees with the
group's for the life of the region. A record that is wrong in a way nothing corrects is worse than
a retry.

**Leave it refused and record it again.** The user's standing order for this wave is that a named
bug is fixed rather than documented, and the refusal is genuinely wrong: it tells a member it is a
stranger.
