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
  the reproduction, kept. **Sixty attempts, and the number is arithmetic**: the defect appeared once
  in twenty, so a twenty-attempt test would go green against the broken code about a third of the
  time — a test that lies at a rate. Sixty puts detection near nineteen in twenty and costs a couple
  of seconds on an in-memory filesystem. Verified red three times out of three with the range check
  neutered (1, 3 and 5 failures of 60), and green with it.
- `esker-engine/tests/db.rs::a_sustained_writer_and_repeated_compactions_lose_no_key` — a writer,
  the pool and repeated `compact_range` calls working the same levels at once, asserting every key
  reads back its newest value. A rule that serialised the wrong thing could drop an output and lose
  a key while returning `Ok`.

  It **asserts that it stressed anything**, and that is not ceremony. Two earlier versions of it
  were worthless in opposite directions. The first let the writer run unthrottled for the whole
  loop, so each round had more to compact than the last and `compact_range(None, None)` chased a
  database growing faster than it drained — 456 other tests finished while that one spun at 350%
  CPU past seventy seconds. Bounding the writer fixed that and introduced the opposite fault: the
  compaction loop now finished *first*, the writer was stopped after 718 writes, and the test passed
  in a tenth of a second having contended with nothing. So the loop's exit condition is the writer
  finishing, and the test counts the rounds that really did run alongside it and fails if that is
  zero. Eight consecutive runs, all green, all between 0.9 and 1.4 s.

## Read back on 2026-09-09 — implemented, and its scenario is no longer reachable

**Option A is built.** `reserve` claims `(cf, output level, smallest, largest)` beside the input
file numbers, under the same lock, in `Reservations.ranges`; `ReservedRange::overlaps` is the test;
`release` gives both back, and it runs **after** `run_compaction` has installed the edit, so there
is no window between the two. `check_disjoint` is where this ADR left it, as the backstop.

**And the scenario in the Context above cannot happen with today's picker.** It reads: *"two `L0 →
L1` plans picked different L0 files, which legitimately overlap each other"*. `Picker::assemble`
applies `l0_closure` to every L0 selection, so a plan's L0 inputs are **closed under overlap** —
two L0 plans therefore either share an input (the closure merged them, and the input reservation
refuses the second) or take L0 files that do not overlap, in which case their key ranges are
disjoint and so are their outputs. The L1 expansion does not change it: two plans that pull in the
same L1 file share an input, and two that pull in different ones stay apart.

So the shape this ADR was written from is either from before the closure or was mis-attributed.
**The rule stays** — it is cheap, it is correct, and it is the one that makes the module's sentence
true as written.

**Which left a live failure with no explanation** — and an hour later, with a diagnostic, it had
one.

`db::a_writer_alongside_compact_range_never_makes_it_report_corruption` refused once in sixty on a
loaded gate: `cf 0 level 1: files 124 and 121 overlap`, and that line was the whole of what the gate
kept. It did not reproduce on a quiet box, and the interleaving it would need could not be
constructed — for the closure reason above, which turns out to be true of the *concurrent* case
only.

So the refusal was made to say more. `check_disjoint` prints where the two files meet, because it is
the only place that can see it, and `db::compact` adds the plan that asked for the edit — inputs,
outputs, output range, manifest number — because it is the only place that knows it. **The tenth run
of the sixty-attempt loop under a six-thread arm caught it:**

```text
files 123 and 119 overlap — 123 ends at key-0199 and 119 starts at bg-000014
refused for the plan cf 0 level 0 -> 1, inputs [115, 112, 111, 116, 110],
outputs [123], output range bg-000000..key-0199
```

**119 is an L1 file and it is not among the inputs.** Had it been in L1 when this plan was picked,
`Picker::assemble` would have taken it as an overlapped input. It was not, so it arrived after the
pick — and the plan then wrote across it.

## The half a reservation cannot reach — 2026-09-09

**A reservation is about plans that overlap in time.** It stops two *running* plans writing into one
range, and `l0_closure` makes the case this ADR was written from unreachable on top of that. What
neither touches is a plan **picked before** another one's edit landed and **reserved after** it was
released: the ranges never meet in the reservation because the first plan is already gone, and
`log_and_apply_compaction`'s staleness check passes because the second plan's own inputs are all
still there — the file that arrived is not one of them.

So that check asks one more question: **has the output level gained a file that overlaps this plan's
output range and is not among its inputs?** If it has, the plan is stale and is dropped, exactly as
it is when an input has gone — the picker offers a fresh one against the version that moved on, and
that one takes the new file as an input the way it would have all along.

| | the sixty-attempt loop, ten rounds, six-thread arm |
|---|---|
| without the check | **1 of 10 rounds refused**, at load 12.9 |
| with it | **10 of 10 green**, at load 31.4 |

**`check_disjoint` is still the backstop and is still doing its job** — it refused a bad edit and
turned a race into a failed operation rather than a corrupt level, twice now, a month apart. This
rule exists so that it does not have to, and it now covers the second way in.

**One thing about how this was found, worth keeping.** The check above was written, could not be
shown red — the deterministic test built for it could not construct the interleaving, for the
closure reason — and was **thrown away** rather than committed: a fix whose test cannot be made red
is a fix nobody can show is needed. What replaced it was evidence, and the evidence arrived in one
run. The order was: refuse to guess, make the failure able to describe itself, and let it.
