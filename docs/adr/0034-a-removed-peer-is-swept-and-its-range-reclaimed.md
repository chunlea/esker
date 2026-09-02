# 0034 — a removed peer is swept by the placement driver, and its range is reclaimed

Status: accepted. Debt wave c3, units 1 and 1b, from `docs/plans/debt-c3.md` §1 — recorded as "a
retired region's data is never reclaimed".

## Context

The debt as it was written down is a disk leak: `Store::retire_region` stopped a peer the cluster
had removed and destroyed its Raft state, but left every key of the region's range on disk in all
three column families, under no region and served by nothing. Its own comment said why, and gave a
blocker — "the engine has no range tombstones in v1 (ADR 0006)" — that
[ADR 0017](0017-range-tombstones.md) retired two phases earlier.
`esker_store::snapshot::clear_range` had been doing exactly this operation, with a verification
pass, since phase 5.

Building the deterministic repro found that the leak was the *second* thing wrong, and that the
first one hid it.

**A removed peer is never told it was removed.** A configuration change takes effect when it is
**appended**, not when it commits — §4.1, and `esker-raft` asserts it in
`conf::tests::a_leader_removed_by_a_committed_change_steps_down` ("applied at append"). So the
instant a leader appends `Remove(n)`, its `Progress` has no entry for `n`, and the entry that says
`n` is gone is the first entry `n` is not sent. The commit needs a quorum of the *new*
configuration, which `n` is not in, so it commits with `n` never having heard of it, and nothing
after that is addressed to `n` either. This is not a race and does not depend on the group's size:
removing peer 3 of `{1,2,3}` commits on `{1,2}`, and peer 3 receives nothing.

`retire_region` had exactly one caller — a peer applying the conf change that removed it — so on
the operator path it never ran at all. The consequences are worse than the leak that was recorded:

* the region's keys stay, which is the recorded debt;
* the peer stays, with no leader, campaigning against a group that has replaced it, for the life of
  the process;
* **a restart does not heal it.** `docs/plans/phase-4.md` §6 race 3 tombstones a region record
  whose peer list does not name this store. This record still names it, because it is the record
  from before the change the peer never applied.

Every `RemovePeer` is one of these, and every rebalance is a `RemovePeer`.

## Decision

**1. The store asks the placement driver about regions that have gone leaderless, and retires the
ones it has been removed from.** `Store::sweep_orphaned_regions` runs on the store-heartbeat
schedule, beside `promote_caught_up_learners`, and for the same reason that one is there: an
`AddPeer` is not finished until the replica votes, and a `RemovePeer` is not finished until the
store it removed has heard about it.

A region is retired when all three of these hold of PD's answer for the region's start key:

* it is about the **same region id** — a different id means the range moved under a split or a
  merge, which is a different question;
* its `conf_ver` is **strictly greater** than the one this store holds, so it describes a change
  this store has not applied rather than the state it already knows; and
* it names **no peer on this store**.

Any other answer, and any failure to get one, leaves the region alone.

PD is the right authority for this and not merely the available one: it holds the routing table
(`docs/DESIGN.md` §7) and learns a region's membership from that region's **leader's** heartbeats,
so a newer `conf_ver` is an applied membership from a peer of the group this store thinks it is
still in. This is the same authority a client's routing already rests on.

**2. The probe is throttled by a leaderless count, and the count is not what makes it safe.** Only
a region whose peer has had no leader for `ORPHAN_PROBE_ROUNDS` (50) consecutive rounds is asked
about, and the counter resets when it is asked. Being leaderless is what a removed peer is
permanently and what an ordinary election is for a moment; the count keeps the question rare. A
probe during a real election is answered "you are still a member" and costs one round trip. All
of the safety is in the three conditions above.

**3. A retirement reclaims the range, in every column family, through `clear_range`.** Delete,
flush, discharge, verify — the same call the snapshot path makes, which already covers `default`,
`lock` and `write` across both physical namespaces since
[ADR 0032](0032-a-snapshot-carries-every-column-family.md).

**4. The Raft state goes first, synced; the range is emptied after.** A crash between them leaves
keys under no record — recoverable, never served, and exactly the state this store used to be in
permanently. The other order would leave a *record* pointing at a half-emptied range, and a peer
restarting into serving a partial region is the one outcome that must never happen
(`docs/plans/phase-4.md` §13.1).

**5. Two gates in front of the delete, because deleting a range this store still owns loses
acknowledged writes.**

* **The membership must not name a peer on this store.** Which record answers that depends on the
  caller, and taking it from the region map is wrong for one of the two: a peer that applied its
  own removal holds the post-change record, but a store the sweep found holds the record from
  before the change and always will. `retire_region` therefore takes the newer record when its
  caller has one.
* **No region this store still hosts may overlap the range.** This is the gate that catches a
  *stale* record — a parent narrowed by a split, retired against the range it had before, would
  delete the child's keys. The map is the authority on what is served here, and it is consulted
  after the removal so its answer cannot include the region going away.

Either gate failing is logged and skips the clear. The keys then stay, which is the old behaviour
and costs disk rather than data.

## Consequences

A rebalance now reclaims the shed replica's disk instead of leaking it, and a shed replica stops
campaigning. A cluster that rebalances is no longer unbounded in disk.

Data deletion now depends on an answer from PD. That is a real widening of what PD's correctness
buys, and it is why the three conditions are stated as conditions rather than as a lookup: a PD
that is behind, unreachable, or answering about a different region cannot cause a delete, only a
delay. The direction of every failure is "keep the region", which costs disk and a campaigning
peer — the state this ADR is fixing, reached only when something else is already wrong.

A store with no placement driver never sweeps. It also never receives a `RemovePeer`, so there is
nothing for it to sweep.

**The columnar runs go too, and the trait grew a capability to make that possible.** They live
beside the engine as immutable files under `<data_dir>/columnar/<region_id>/`, swept by their own
manifest, so nothing the engine reclaims can reach them. Removing a tree of them could not be built
out of `list` and `delete` — `delete` takes a file, `list` takes a directory, and nothing in
`FileSystem` said which a path is — so `FileSystem::remove_dir_all` is new, with no default, which
makes every implementation state its answer: the local one, the in-memory one, the fault injector,
the tiered wrapper, and the crash filesystem in `esker-engine`'s version test.

It is idempotent, because the caller may be running after a crash interrupted it, and it removes
the directory rather than emptying it, because "does this exist" is how the caller asks whether the
reclamation happened. The in-memory implementation matches by **ancestry** rather than by string
prefix, so `columnar/12` is not swept up beside `columnar/1`.

The slot in `Store::columnar` is dropped before the tree, because it owns the `RunSet` that owns
the manifest and a fragment arriving mid-removal would otherwise reopen the table and write a
manifest back into a directory being deleted. The removal runs unconditionally after the range
clear rather than chained onto its success: the copy is derived from the range and belongs to a
region that is gone either way, and a range clear that failed is a reason to keep the *keys*, never
a reason to keep a copy of them. Per region id, which is what makes it safe without a second look
at the region map — PD never reuses an id and a split child gets its own directory. The parent is
`fsync`ed after, since a directory removal is not durable until the directory that held the entry
is.

The fault injector deliberately does **not** count or fault it, on the same terms as `open`: this
removes files belonging to a region the cluster has already taken away, so every caller logs a
failure and carries on, and an injected failure would exercise a `warn!` rather than a recovery
path.

## Alternatives rejected

**Keep the leader sending to a removed peer until it acknowledges.** This is what would tell the
peer in band, and it is what several implementations do. It means the leader keeping `Progress` for
a node its configuration no longer has, which is state in the Raft core, and invariant 4 makes the
core the wrong place to put a delivery convenience. `esker-raft` stays a pure state machine.

**A new store-to-store "you are removed" message.** The leader knows the removed peer's store and
already has a connection to it. It is a wire addition — a new request, a new refusal, a new
retry — to deliver a fact PD already holds and already answers for, and a message the removed store
misses while it is restarting would need the sweep back anyway to be reliable.

**Retire on the region record alone, at restart.** Already the design (§6 race 3) and already
insufficient: the record a removed peer holds names that peer, which is the whole problem.
