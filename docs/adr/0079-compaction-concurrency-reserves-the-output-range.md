# 0079. Compaction concurrency reserves the output range, not only the input files

Date: 2026-09-05

## Status

Accepted.

## Context

`crates/esker-engine/src/db/compact.rs` runs compactions on a bounded but non-serial pool
(`docs/DESIGN.md` §4.7: two threads), and `Db::compact_range` runs one inline on the caller's
thread at the same time. The module's header stated the rule it implemented:

> **Two compactions must not touch one file** … Every plan therefore reserves its **inputs** by
> file number before it starts

That rule is necessary and it is **not sufficient**, which a reproduction found:

```text
Err(Corruption { context: "manifest",
                 detail: "column family 0 level 1: files 133 and 132 overlap" })
```

returned by `db.compact_range(cf::DEFAULT, None, None)` on a fresh database with a small write
buffer, a heavy L0, and a thread writing while the call ran. An A/B with a control, same shape and
same commit, twenty attempts per arm, the only variable being the writer:

| arm | corruption errors |
|---|---|
| **with** a concurrent writer | **1 / 20** |
| **without** one | **0 / 20** |

`reserve` constrains inputs; nothing constrained outputs. L0 files legitimately overlap each other,
so two `L0 → L1` plans can pick different L0 files; when L1 is empty or sparse neither pulls in an
L1 file, their input sets are disjoint, and both reservations succeed. Both then write outputs into
L1 covering overlapping key ranges, and L1 stops being a partition of the key space.

Nothing reached disk. `version::builder::check_disjoint` validates a level while the version is
built, so the bad edit was refused and the error surfaced. The cost was a **failed operation on a
legal workload** — writing during a `compact_range` is ordinary and supported — rather than lost
data, which is why this was written up rather than paged.

## Options

**A. Reserve the output key range as well as the input files.** A plan claims
`(column family, output level, smallest, largest)`; a plan whose range overlaps a running plan's
range in the same level does not start, exactly as a file conflict already works — dropped rather
than queued, because the picker produces it again against a version that has moved on.

**B. Forbid a second concurrent compaction into the same level.** Simpler to state and strictly
more serialising: two compactions into one level are ordered even when their ranges are far apart.

**C. Stop `compact_range` running inline alongside the pool.** Makes the caller's compaction take
the pool's slot instead. It removes one *source* of concurrency rather than the unsafety: two pool
threads can still produce the same collision.

## Decision

**Option A.** A plan reserves the key range it will write, per `(column family, output level)`, in
the same structure and under the same lock as its input files — both claims are taken and given
back together, because a plan holding one and not the other is a window where a third plan sees
half of one.

The claim is the union of the plan's inputs, as **user keys**, which is the widest its outputs can
be. User keys rather than internal ones: `check_disjoint` compares internal keys, which order by
user key and then by sequence number, and a compaction's outputs carry sequence numbers its inputs
did not — so an internal-key comparison of the *inputs* would be answering about keys that will not
exist. Comparing user keys claims a little more than the outputs occupy, which is the direction that
cannot be wrong.

The module's rule becomes **"two compactions must not touch one file, nor write overlapping ranges
into one level"**, which is sufficient as written.

`check_disjoint` stays. It is the backstop now rather than the first line: it refuses a bad edit,
which turns a race into a failed operation instead of a corrupt level — and this rule exists so
that it never has to.

## Consequences

**Two compactions into different levels never contend**, which is what keeps the pool parallel and
is the reason for option A over option B. The claim is per output level, so an `L1 → L2` job and an
`L0 → L1` job run together exactly as before.

**Two compactions into the same level are ordered when their ranges touch.** For `L0 → L1` that is
effectively one at a time, because L0 files overlap each other and so do the ranges taken from
them. That is the same place LevelDB and RocksDB arrive at, for the same reason, and it is the
throughput this buys the invariant with: a database whose whole workload is `L0 → L1` gets one
compacting thread rather than two. Levels below L0 partition the key space, so plans there have
disjoint ranges by construction and keep their parallelism.

**A refused plan costs a retry, not a queue.** `reserve` answering `false` already meant "drop it;
the picker will offer it again", and `compact_range` already waited on `compaction_done` rather than
spinning. Range conflicts are more frequent than file conflicts, so that wait is taken more often —
bounded by one compaction finishing, since a running plan always releases.

**`esker.compactions-running` is unchanged.** It counts reserved input *files*, not compactions and
not ranges; the property's doc in `db/mod.rs` says so, and this ADR does not alter what it reports.

## Tests

- `esker-engine/tests/db.rs::a_writer_alongside_compact_range_never_makes_it_report_corruption` —
  the reproduction, kept: twenty attempts with a writer, zero errors. One attempt would have proved
  nothing against a 1-in-20 rate.
- `esker-engine/tests/db.rs::a_sustained_writer_and_repeated_compactions_lose_no_key` — a writer,
  the pool and repeated `compact_range` calls working the same levels at once, asserting every key
  reads back its newest value. A rule that serialised the wrong thing could drop an output and lose
  a key while returning `Ok`.
