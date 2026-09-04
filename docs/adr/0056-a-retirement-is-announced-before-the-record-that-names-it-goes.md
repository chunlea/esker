# 0056 — a retirement is announced before the record that names it goes

Status: accepted. Debt wave c6, unit 1, from `docs/plans/debt-c6.md` §2 — the residual left by
[ADR 0034](0034-a-removed-peer-is-swept-and-its-range-reclaimed.md), found while verifying that
inventory #2 was closed.

## Context

ADR 0034 made a removed peer reclaim its range: `Store::retire_region` stops the peer, destroys the
region's Raft state and its `'m'` metadata record, and then clears the range in every column family.
That is **two durable steps**, and a crash lands between them.

The old code said what that left, and read as though it were benign:

> A crash in between leaves keys under no record, which is precisely the state this store was in
> permanently until now — recoverable, and never served, because there is no region record to serve
> them from.

"Never served" is true and is the safety half. "Recoverable" is not true of anything. The `'m'`
record is **the only thing on disk that says which range the region was** — its start and end keys
live nowhere else on this store — and the first step deletes it. After the crash the keys are in
`default`, `lock` and `write` under no region, and nothing can name them again:

* not the store, which hosts no region covering them and never will;
* not the placement driver, which knows what the cluster's regions are and nothing about what any
  particular disk still holds;
* not a later retirement of the same region, which needs the record that is gone.

So the window is not a delay, it is the original leak with a `kill -9` in front of it — one whole
region's data, permanently, on a path **every rebalance takes**. It is precisely the shape ADR 0034
was written to end, surviving inside the fix for it.

There is already a record in the `raft` column family for the other direction of this problem.
`'p' ++ region_id` says *a snapshot is part-way into this range and it must not be served*, and its
own documentation makes the argument this ADR needs:

> It carries the whole region rather than just its id, and that is what makes the recovery
> possible: the keys a partial receive left behind are in the region's **range**, and a restart
> that knew only an id could not find them.

Data arriving into an unowned range was given a durable name in phase 4. Data leaving one was not.

## Decision

**1. A fifth prefix in the `raft` column family: `'R' ++ region_id:u64` → the region being
reclaimed, whole.** The mirror of `'p'`, carrying a full region record for the same reason: the
range is the recoverable part. Upper case because it is the only one of the five that is not a
record of something this store *has*.

**2. It is written in the same batch as the deletion it announces, or not at all.** `raft_log::destroy`
takes the announcement as an argument and stages it beside the deletes of `'l'`, `'s'`, `'m'` and
`'p'`, in one synced write. Written before that batch, a crash in the gap leaves a retirement
announced for a region that is still on disk and still served; written after it, the gap is the one
it exists to close.

**3. The caller answers "may this store delete these keys" *before* the batch, because the batch
destroys the record the answer is computed from.** Gate 1 of ADR 0034 — the membership must no
longer name a peer on this store — therefore moves in front of the destroy, and its answer *is* the
argument: `Some(region)` announces, `None` keeps the keys and announces nothing. A store that may
still be serving a range must never schedule it for deletion, and after the batch there is no
membership left to ask.

**4. `Store::open` finishes every announced retirement before the store serves anything**, and it
runs **after** the regions are hosted, not before. Gate 2 — no region this store still hosts may
overlap the range — asks the region map, and the map is empty until the peers are started; run
first, the sweep would answer "nothing overlaps" for every announcement and empty a range under its
owner. Nothing can write into the range in the window: a peer only writes inside its own region, so
a peer that could reach these keys is one that makes gate 2 refuse.

**5. The announcement is removed only when the range is provably empty.** `snapshot::clear_range`
verifies emptiness and reports it; a clear that failed — a tombstone still above the compaction
floor because something holds an engine snapshot open — leaves the keys **and the announcement**,
and the next open comes back for them. Removing it on the strength of having tried is how a
reclamation that failed becomes the leak this record exists to end.

**6. A refused announcement is dropped rather than kept.** Gate 2 refusing means the range is
covered by a region this store serves, so the keys are not orphaned — they are somebody's. An
announcement kept for them would ask the same refused question at every open for the life of the
store.

## Consequences

**One implementation of the delete, with two callers.** `finish_retirement` is the whole of what a
retirement does to data — gate 2, the range, the columnar tree, the announcement — as one blocking
function that the live path and the open-time sweep both call. A second implementation of a delete
is a second chance to get a delete wrong.

**Every step is idempotent, because a crash may land anywhere inside it and the next open runs the
whole thing again.** `clear_range` returns early on an empty range, `FileSystem::remove_dir_all`
treats an absent directory as done, and dropping an announcement twice is one delete. A second pass
over a finished retirement is two reads and a write.

**The failure mode is now a retry rather than a leak**, including for the case that was already
"loud and harmless": before this, a `clear_range` that failed left the keys with nothing to come
back for them, which is the same permanent leak reached by a different route. It is now a
retry at every open until it succeeds.

**A wider format, and a compatible one.** The prefix is an addition beside `'m'` rather than a field
on it, on exactly the terms `'p'` was added: `'m'` has a golden test and a version byte, and a new
prefix costs nothing while a change to that record would cost an ADR of its own and a version bump.
A database written before this has no `'R'` records, so `load_retiring` returns nothing and an old
database opens unchanged — it keeps whatever it had already orphaned, which no format can recover,
and leaks nothing further.

**What this does not do.** It does not find ranges orphaned *before* this landed: they have no
record and no announcement, and reconstructing them would mean scanning three column families for
keys outside every hosted region — a different and much more dangerous operation, since "outside
every region this store hosts" is also what a store looks like for the instant before a snapshot
completes. An operator with such a store reclaims by removing it and letting the cluster refill it.

## Alternatives rejected

**Clear the range first, then destroy the record.** Reverses the crash window into the one outcome
that must never happen: a record pointing at a half-emptied range, and a peer that restarts into
serving a partial region (`docs/plans/phase-4.md` §13.1). The current order is deliberate and stays.

**Keep the `'m'` record until the range is empty, using it as its own announcement.** It would work
for the range, and it re-opens the hole the order was chosen to close — a record on disk is what
`Store::open` starts a peer from, so a crash mid-clear brings back a peer serving a range that is
being deleted underneath it. The announcement is a separate record precisely so that it is *not* a
thing anything hosts.

**Sweep at open by comparing keys against hosted regions, with no announcement at all.** No new
format, and it cannot distinguish an orphan from a region mid-arrival, whose keys are also under no
hosted record. It would delete the tail of an interrupted snapshot receive at every restart, which
`'p'` exists to handle correctly.
