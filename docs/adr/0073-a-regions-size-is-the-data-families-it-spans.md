# 0073 — a region's size is the data families it spans

Status: accepted. Lane `split`, from the blocker `docs/plans/phase-16-mpp.md` §10 names as
"a SQL table that splits" — the first of the three things that would change that phase's verdict,
and the one every other line in it waits on. Plan: `docs/plans/split-region.md`.

## Context

A region is split once `esker_store::split::approximate_size` says it has passed a threshold
(`docs/DESIGN.md` §14, 96 MiB), and the boundary comes from `choose_split_key`'s scan of the same
data ([ADR 0012](0012-split-key-selection.md)). Both read one range in one column family:

```rust
db.approximate_size(cf::DEFAULT, Some(&['r'] ++ start), Some(&['r'] ++ end))
```

A region owns a range of the **user** key space. A store writes each user key into one of two
physical shapes: `'r' ++ user_key` for `RawKV`, and `'x' ++ enc(user_key) ++ !ts` for anything
transactional — which is every SQL row, every index entry and every catalog record, because a row
*is* the user key a transaction writes (`docs/DESIGN.md` §3, §8). Those live in `default` and
`write`, with the claim on each locked key in `lock`.

So the size of a region holding a SQL table was the size of the RawKV keys in it, which is none.
The mpp lane measured the consequence end to end: a region holding 200,000 rows reports `~0` bytes
to PD; asking for four regions with a 4 MiB threshold over ~20 MB of table produced exactly the one
region a 512 MiB threshold produced. **A SQL table occupied exactly one region whatever its size,
at every threshold** — so range sharding, the system's whole scale-out story, did not trigger for
SQL data at all, and MPP had one fragment to move things between.

This is [ADR 0032](0032-a-snapshot-carries-every-column-family.md) one layer over. That ADR was
written after snapshot format version 1 walked `default` under `'r'` alone and so shipped a region
without any transactional record it held. The same region shipped its records correctly from then
on and went on reporting none of them.

### The wrong fix, and why it is not obvious that it is wrong

Widening the range to `['r', 'y')` in `default` — one namespace byte, one line — leaves the
finding exactly where it was. `esker_txn::codec::SHORT_VALUE_MAX_LEN` is 255, and a value at or
under it is **inlined into the `write` record**; `default` gets no entry at all
(`percolator::Op::short_value`). Ordinary SQL rows are short, so for the workload that produced the
measurement essentially every byte of the table is in the `write` column family. A fix that does
not read `write` reports zero for the same table and looks like it closed the finding.

## Options

**A. Widen the range in `default`.** One line, and wrong above.

**B. Count every column family the store opens, over the whole region.** `default`, `lock`,
`write`. Symmetrical with the snapshot stream, which ships exactly those three.

**C. Count the families that hold data: `default` and `write`.** `lock` is excluded, `raft` stays
excluded.

**D. Keep a counter.** The applied-bytes hint phase 4b started with, extended to the transactional
path. Rejected before, for the reasons `approximate_size`'s doc comment still carries: it never
shrinks on a delete, never counts what was on disk before the process opened, and restarts at zero
— so two peers of one region disagree about its size and a restarted leader will not split.

## Decision

**C**, with the mapping shared rather than repeated.

**1. `default` and `write` count toward a region's size; `lock` and `raft` do not.** A lock is one
in-flight transaction's claim on one key, deleted by both commit and rollback. Counting it makes a
region's size a function of how many transactions happen to be open at the instant the checker
looks, and lets a burst of prewrites trigger a split of data that has not been written. `raft` is
this store's log, hard state and region records — keyed by region id rather than by user key, and
one peer's facts about a region rather than the region's contents, which is why `SNAPSHOT_CFS`
excludes it too.

The snapshot ships `lock` and this does not count it, and that is not an inconsistency: a snapshot
must reproduce the region's *state*, locks included, or the receiver serves a transaction that has
lost its claim. A size decides whether there is enough *data* to divide.

**2. Both physical namespaces in both families, from one mapping.** `snapshot.rs`'s
`physical_ranges` — the region-to-engine-ranges mapping ADR 0032 introduced — moves to
`crates/esker-store/src/keyspace.rs` and is used by the snapshot, the reclaim and the split alike.
Three copies of a key-space mapping is how the third one came to be wrong while the first two were
right.

Both namespaces are asked of both families rather than kept in a table of which family may hold
which. `write` holds only `'x'` today; asking it for the `'r'` range costs an overlap check that
matches no file. `snapshot.rs` already made this trade for the stream and states it: a cheaper
guarantee than the table, and one that cannot go stale.

**3. The boundary scan reads exactly the ranges the size counts.** Four cursors — two families ×
two namespaces — merged into one ascending stream of user keys. The failure this rules out is the
one a partial fix produces: a region whose size says "split" and whose scan finds nothing, which
`Store::spawn_split_checker` records as unsplittable and does not look at again until the region
has grown by another whole threshold.

**4. A key's versions are one key.** Each cursor advances past the rest of a user key's versions
before offering the next, and a key held by more than one cursor is offered once. ADR 0012's
sample is over keys by count; without this it would be over *versions*, so a table where one row
was rewritten a thousand times would put a thousand samples inside that row and choose a boundary
that divides no rows at all.

**5. The boundary is a user key.** It is checked against `start_key`/`end_key`, it travels in the
`Split` command and every peer applies it to its own copy of the range — none of which knows about
a namespace byte or a timestamp suffix. So each cursor decodes: `'r'` by stripping the byte, `'x'`
through `esker_txn::key::split`, which is where that encoding lives (`CLAUDE.md` invariant 7). No
key layout is parsed in `esker-store` and `esker-keys` needs no new accessor.

## Consequences

* **A SQL table can now occupy more than one region**, which is what
  `docs/plans/phase-16-mpp.md` §10 was waiting for. Every other item on its list becomes askable.
* **Clusters holding transactional data will start splitting on this build**, at 96 MiB, having
  never split before. The split path itself is unchanged phase-4 code with its own tests; what is
  new is that it now receives work.
* **A region's size includes superseded versions** until the collector drops them, so a
  heavily-updated table splits earlier than its live row count suggests. That is the direction
  `Db::approximate_size` already over-counts in, and it is the safe one: a split that fires early
  costs a split, one that fires late costs a region that has outgrown its bounds.
* **A split attempt costs up to four range scans** instead of one, on a blocking thread. For a
  region holding one namespace, three of them are a seek that fails its bound immediately. ADR
  0012 already prices a split at one full scan of the region.
* **A prewritten, uncommitted key is bytes but not a boundary.** Its value sits in `default` under
  `'x'` (when it is long) and its lock in `lock` (which is not counted), and there is no `write`
  record until it commits — so the size sees the value and the scan can too, but a region made
  *entirely* of uncommitted long values would be over the threshold with a boundary drawn only
  from those same `default` entries. Nothing is lost: they commit or they are rolled back, and the
  next check sees whichever happened.
* **A third physical namespace now has two places to be added**, not one: a range in
  `keyspace::physical_ranges` and a decoder in `split::user_key`. Both refuse rather than guess,
  and `keyspace`'s guard test says the ranges and the namespace list are about the same bytes.
