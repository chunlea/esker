# 0112 — Collecting a spilled value

Status: **Accepted**, 2026-09-12 — the user took **(b)**, the recommendation below. Proposed
2026-09-11, opened because [ADR 0111](0111-a-deleted-keys-versions-are-dropped-as-one-segment.md)
made the shape of #60 sharper: the records that named these values are now dropped outright, so the
values they named are orphans with nothing pointing at them at all.

**Implemented as (b)**: `gc::collect_spilled_values(db, safepoint)`, one snapshot for both families,
run by `collect::Sweeper` **between** the `write` family's compaction and the `default` family's —
which is why `COLLECTABLE` is ordered `[write, lock, default]` and not alphabetically. An entry is
kept when a surviving `write` record names its `start_ts`, when the key still holds a lock, or when
`start_ts >= safepoint`; the first of those is the ask this option is named for and the other two are
the states in which no `write` record can be expected to name it yet.

## The problem

A value longer than `esker_txn::SHORT_VALUE_MAX_LEN` (255 bytes) is not inlined in its `write`
record. Prewrite stores it in the `default` column family under
`esker_txn::key::value(user_key, start_ts)` — versioned by the transaction's **`start_ts`**, because
it is written before any commit timestamp exists — and the `write` record's `start_ts` field is the
link back to it.

`MvccCollector` is installed on `write` **and only on `write`**: `open_engine` says so, and says why
— the collector decodes a `WriteRecord` out of every value it is offered, and the other three
families hold something else. So nothing collects `default`, and `gc.rs`'s module doc claimed
otherwise until #60 measured it.

**The measurement (#60).** Eight keys, twelve versions each, 512-byte values, `esker admin gc` at a
safepoint above all of them against one at zero:

| family | before | after |
|---|---|---|
| `write` | 96 | **8** |
| `default` | 96 | **96** |

So collecting reclaims version *records* and not *bytes* — a version record is tens of bytes and a
value is as large as the user made it. Anyone measuring #58's space curve on a workload with real
values will quote that number.

## What ADR 0111 changed about it

Before 0111, a collected key kept one record: the newest version at or below the safepoint. A
reader walking the `write` family could therefore still find, for any surviving key, the `start_ts`
of the value it named — so "is this `default` entry referenced" had a witness in the family beside
it.

After 0111 a **deleted** key's `write` records go entirely. Its `default` entries lose their last
reference. That cuts both ways and both belong in this decision:

* it makes the orphan **unreachable in principle**, not merely unread — nothing in the database
  names it, so no reader can ever be shown it and its bytes are pure loss;
* it makes "unreferenced" **easier to decide**, because absence is now unambiguous: a `default`
  entry whose `(user_key, start_ts)` no surviving `write` record names is garbage, full stop, where
  before one had to reason about which versions were still visible.

## Options

### (a) The `write` filter deletes the `default` entry as it drops the record

It already decodes the record it is dropping, so it knows the `start_ts` whose value dies with it.

**The engine does not allow it.** A `CompactionFilter` returns a `FilterDecision` and nothing else;
`CompactionJob` writes to one `CompactionOutput`, which is one column family's files. There is no
path from a filter to another family, and adding one means a compaction writing into a family it is
not compacting — against a version of that family another compaction may be rewriting at the same
moment.

So (a) is really "a queue out of the filter that the store drains afterwards", and the queue has to
survive a crash mid-compaction or the deletions are lost silently. It is the smallest change to
*reason* about and the one with a durable side-channel to get right.

### (b) A pass over `default` that asks `write`

A sweep over `default` keeps an entry when some surviving `write` record names its
`(user_key, start_ts)`, and drops it otherwise. Self-correcting — it converges on the truth however
the `write` family got to its current state — and it needs no channel and no crash story: a pass
that dies half-way simply leaves work for the next one.

**Its cost is a read of `write` per candidate**, and its risk is the snapshot it reads against: the
`write` family is being rewritten by compactions while this runs, and a `default` entry judged
against a stale view of it deletes a live value. The pin is that the reference direction is
one-way and monotone — a `write` record that exists now may be collected later, but a collected one
never comes back — so **reading `write` at a snapshot at or newer than the one `default` is judged
at can only keep too much**, never too little. That is the direction this system takes everywhere
else.

This is the shape #70's `collect::Sweeper` already has: it compacts `default`, `lock` and `write` on
a rising safepoint, and it is the first thing in this repository to touch `default` deliberately.
Adding "and before compacting `default`, delete what nothing names" is a step in a loop that already
exists.

### (c) Offline, on `gc --safepoint`

`AdminReq::Gc` already raises the safepoint and compacts every family; the scan could go there and
nowhere else. It costs nothing on the ordinary path and it is an operator's explicit act.

But it makes reclaiming bytes something a human has to ask for, on a schedule nobody sets, and #58
is a complaint about a node that has *run for a long time* — which is precisely the node whose
operator has not run `gc` lately. It is the option that is easiest to land and hardest to rely on.

## Recommendation

**(b), on the sweeper's existing pass.** It is the only one of the three with no durable
side-channel to get wrong, its failure mode is "kept too much" rather than "deleted a live value",
and it lands in a loop that already runs on the event that makes the work necessary. (a) is smaller
in code and larger in what can go wrong at a crash; (c) is smaller still and does not answer the
debt it is for.

The one thing to decide before building it is the **snapshot discipline**: the pass must read
`write` at a snapshot no older than the one it walks `default` at, and that has to be stated in the
code rather than arranged by the order of two calls.

## Acceptance

The test exists and is `#[ignore]`d on purpose:
`crates/esker-store/tests/safepoint_collects.rs::a_spilled_value_is_collected_with_the_version_that_names_it`.
Removing the attribute is the acceptance, as it was for #60.

Two more belong with whichever option is chosen:

1. **A live value is never deleted.** A `default` entry whose `write` record survives the collection
   must survive it too — the counterfactual for the snapshot discipline, and the one that fails if
   the reference is read against a stale view.
2. **A crash mid-pass loses no value and leaks no reference.** For (b) this is nearly free, because a
   pass that dies leaves work rather than damage; for (a) it is the whole of the risk and needs the
   fault-injection shape `crash_compaction.rs` already uses.

## What does not change

No on-disk or wire format. The `default` key layout, the `write` record's `start_ts` field and the
spill threshold are all as they were; this is a decision about *what deletes an entry*, not about
what an entry is.
