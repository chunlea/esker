# The in-house arena skiplist against the one it replaces (ADR 0041)

Recorded as [`README.md`](README.md) describes. **Not a gate.** The question this file answers is
the one [ADR 0041](../adr/0041-the-in-house-arena-skiplist.md) left open: the in-house arena
skiplist buys a scan that is a pointer hop instead of a re-find, and it pays for it somewhere —
where, and how much.

## Method, shared by every section

| Field | Value |
|---|---|
| machine | Apple M4 Max (`Mac16,6`), 16 cores, 128 GiB, built-in NVMe SSD (APFS) — the same machine as phases 1–4, 6b, C4 and 11 |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `cargo build --release`: opt-level 3, 16 codegen units, **no LTO** (the workspace sets no `[profile.release]`) |
| **not idle** | Two or three other agent sessions were building and testing this repository throughout. `uptime` load average ran **9.6 – 23.1** against 16 cores. Every absolute number here is a lower bound on what an idle machine would give |
| **interleaved** | Five rounds. Round *r* runs arm A then arm B, round *r+1* runs B then A, so neither arm systematically gets the warmer or the busier machine. `docs/plans/phase-11-engine.md` §10 is the lane that learned why: a block-at-a-time run measured its own compiler |
| statistic | criterion's point estimate per round; the tables give the **median of the five rounds** |
| seeds | the skiplist draws node heights from a fixed seed (`DEFAULT_SEED`), so its shape is the same in every round and in every arm. `crossbeam-skiplist` draws from thread-local entropy and has no seed to fix — which is one of the things ADR 0041 set out to change |

### The two arms are one line apart, and it was checked

Both arms are built from the same working tree at the same commit. The only difference is
`memtable.rs`'s `type Selected = …`. That is cheap to claim and worth proving, so each binary was
checked for the symbols it should and should not contain:

```
bench-bought      crossbeam symbols: 39   skiplist symbols: 0
bench-skiplist    crossbeam symbols: 0    skiplist symbols: 9
cli-bought        crossbeam symbols: 45   skiplist symbols: 0
cli-skiplist      crossbeam symbols: 0    skiplist symbols: 14
```

### The control

`read/get_miss` is carried through as a control. A miss short-circuits on the user-key
comparison at the first entry it lands on, so it touches the store's seek and almost nothing
else, and it should barely move. A pass where it moves is a pass to distrust.

---

## 1. The memtable on its own — `cargo bench -p esker-engine`

Medians of five interleaved rounds. Both columns are absolute; the ratio is the last column and
is never the only thing said, per `README.md`.

**After the arena change in `8c9bd3b`** (`perf(engine): one arena resolution per node`):

| benchmark | `crossbeam-skiplist` | arena skiplist | skiplist ÷ crossbeam |
|---|---|---|---|
| `read/scan_full` — 100 k entries, end to end | 26.14 ms | **1.41 ms** | **18.55×** |
| `read/scan_short` — seek then 20 steps | 5.10 µs | **762 ns** | **6.69×** |
| `read/get_miss` | 179 ns | **137 ns** | 1.31× |
| `read/seek_only` | 712 ns | **613 ns** | 1.16× |
| `read/get_hit` | 632 ns | **574 ns** | 1.10× |
| `read/scan_back` — 1 000 steps backwards | 200.56 µs | **188.79 µs** | 1.06× |
| `fill/random` — 100 k inserts | **47.36 ms** | 57.47 ms | 0.82× |
| `fill/overwrite` — 100 k inserts over 1 k keys | **27.94 ms** | 38.65 ms | 0.72× |
| `fill/seq` — 100 k inserts | **18.10 ms** | 31.72 ms | 0.57× |

**Before it**, on the same harness, five rounds, hours earlier and so not comparable to the table
above in absolute terms — the ratios are what carry across:

| benchmark | `crossbeam-skiplist` | arena skiplist | ratio | after |
|---|---|---|---|---|
| `read/scan_full` | 26.86 ms | 1.46 ms | 18.38× | 18.55× |
| `read/scan_short` | 5.10 µs | 887 ns | 5.75× | 6.69× |
| `read/get_miss` | 175 ns | 163 ns | 1.07× | 1.31× |
| `read/seek_only` | 528 ns | 879 ns | **0.60×** | 1.16× |
| `read/get_hit` | 555 ns | 688 ns | **0.81×** | 1.10× |
| `read/scan_back` | 170.58 µs | 212.59 µs | **0.80×** | 1.06× |
| `fill/random` | 41.61 ms | 61.22 ms | 0.68× | 0.82× |
| `fill/overwrite` | 25.91 ms | 44.00 ms | 0.59× | 0.72× |
| `fill/seq` | 16.59 ms | 29.96 ms | 0.55× | 0.57× |

The first pass is the reason the second exists. Reading a node's header one word at a time paid
the arena's addressing cost — chunk index, bounds, directory load — four times where once would
do, and a seek compares thirty-odd keys. Reading all four header words in one resolution moved
every read benchmark from slower than `crossbeam-skiplist` to faster. `scan_full` did not move,
which is the check that the change did what it claimed: a scan compares nothing, so a cheaper
comparison path should do nothing for it.

**What is left is the insert**, and sequential insert most of all: 18.10 ms against 31.72 ms for
a hundred thousand, or **181 ns against 317 ns each**. Two arena resolutions per key comparison —
one for the header, one for the bytes — is the floor for a design with a separate byte arena and
word arena. Going below it means putting the key bytes inside the node's own allocation, which is
`LevelDB`'s layout and a redesign rather than a tuning. It was not taken here.

---

## 2. `esker-cli bench` — and why ADR 0041's bench plan cannot answer its own question

ADR 0041 asked for `fillseq`, `fillrandom`, `readseq` and `scanrange` "with a large
`--write-buffer-size`, so the run is memtable-dominated", and `overwrite` on top. Run that way,
three interleaved rounds, medians in ops/s:

| workload | `crossbeam-skiplist` | arena skiplist | ratio |
|---|---|---|---|
| `fillseq` | 444 432 | 441 605 | 0.99× |
| `fillrandom` | 338 938 | 342 978 | 1.01× |
| `overwrite` | 351 328 | 345 512 | 0.98× |
| `readseq` | 6 891 977 | 7 215 665 | 1.05× |
| `scanrange --batch-size 20` | 192 533 | 192 732 | 1.00× |
| `readrandom` (control) | 385 504 | 385 764 | 1.00× |
| `tso` (control, never touches the engine) | 10 685 973 | 10 534 170 | 0.99× |

Flat. **Four of those five workloads could not have been anything else**, and that is the finding
rather than the numbers.

`crates/esker-cli/src/bench.rs` calls `db.flush(cf::DEFAULT)` before the measured phase of
**every** read workload, so `readseq`, `scanrange`, `readrandom` and `readmissing` all run against
an **empty memtable** no matter what `--write-buffer-size` says — that flag reaches the fill and
nothing else. The ADR's line "`readseq` … so the whole scan is the memtable cursor" is false for
this driver, and it was false when the ADR was written.

The probe that caught it, before the source was read: `readseq` gives **6.50 M ops/s** against one
SST and **6.60 M ops/s** against fifty-five. A workload whose cost does not notice a
fifty-five-fold change in what it is reading from is not reading what it is supposed to be.

The two fill workloads *do* reach the memtable and are flat for a different reason: at ~2.3 µs per
operation they are a write-ahead-log append and a group commit, and the whole memtable insert —
181 ns or 317 ns of it — is a few per cent of that.

### 2a. The flush path, which is where a full memtable scan really happens

`db/flush.rs`'s `build_table` walks an immutable memtable end to end to build an SST. That is
`read/scan_full`, in production, on every flush. A one-megabyte write buffer over a million keys
makes flushes frequent enough to be the bottleneck; three interleaved rounds, medians in ops/s:

| workload | `crossbeam-skiplist` | arena skiplist | ratio |
|---|---|---|---|
| `fillrandom --write-buffer-size 1 MiB` | 198 035 | 195 251 | 0.99× |
| `fillseq --write-buffer-size 1 MiB` | 195 577 | 192 976 | 0.99× |
| `tso` (control) | 10 798 529 | 10 324 201 | **0.96×** |

**The control moved 4.4%, so nothing here below about 5% is real**, and both fill differences are
inside that. `tso` never touches the engine; the two arms' binaries differ in size and symbol
layout, and that alone is enough to move an unrelated workload by a few per cent. It is recorded
because a control that moves is the thing that tells you the floor.

So: end to end, on this driver, the two stores are **indistinguishable**. The 18× scan is real
and it is measured, but the paths that would show it are either flushed away before the clock
starts or drowned in the write-ahead log.

### What esker-cli would need to measure this properly

A read workload that does **not** flush first — a `--keep-memtable` flag on the read workloads, or
a `readmem` workload that fills and reads without the `db.flush` in between. That is a change in
`crates/esker-cli/`, which is not this lane's to make; it is written up here so that whoever owns
that crate has the reason as well as the request.

---

## 3. The thing that is not a benchmark: `crossbeam` cannot be run under Miri

ADR 0041 item 5 makes Miri **a gate on the code landing**, because for pointer arithmetic
"exercises every `unsafe` block" (`CLAUDE.md` invariant 8) means Miri. It turns out that gate was
never passable while `crossbeam-skiplist` was the memtable, and nobody had tried.

Running the memtable's own unit and property tests under Miri:

| store | Stacked Borrows (Miri's default) | Tree Borrows (`-Zmiri-tree-borrows`) |
|---|---|---|
| arena skiplist | **the whole `memtable` module passes — 37 tests: 36 run, 1 ignored**, 60 s | — |
| `crossbeam-skiplist` | UB in `crossbeam-epoch-0.9.20/src/internal.rs:562` — `&*local_ptr`, "that tag does not exist in the borrow stack" | UB in `crossbeam-skiplist-0.1.3/src/base.rs:124` — `dealloc(...)`, "deallocation through `<tag>` is forbidden" |

Both models reject it, in different crates and for different reasons. That is **not** a claim that
`crossbeam-skiplist` miscompiles or is unsound in practice: Stacked Borrows and Tree Borrows are
stricter than any settled aliasing model, and these are long-standing known complaints against
those crates rather than observed misbehaviour.

What it does mean is concrete. The engine's memtable tests could not be run under Miri at all, so
the arena skiplist's `unsafe` could never have been checked by the tool the ADR named for it —
the run would abort inside a dependency before reaching any of it. With the dependency gone,

```
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p esker-engine --lib -- memtable
```

runs the module through, which is what the gate should have been all along.

**`-Zmiri-disable-isolation` is part of the command, not a convenience.** Without it the run aborts
at `the_skiplist_answers_what_a_sorted_map_would`: proptest's default `FileFailurePersistence` wants
to write a `.proptest-regressions` file beside the source, so it calls `std::env::current_dir`, and
Miri refuses `getcwd` under isolation. The harness dies there with 22 tests unrun — a *failure to
run the gate*, which is easy to mistake for the gate failing. The ignored test in the count is
`relaxed_publication_is_a_data_race`, the red-first control below, which is meant to fail.

One thing had to change for that to be true, and it was a test rather than the code:
`readers_and_writers_run_concurrently` ran four readers over a two-thousand-entry list five
hundred times, which is millions of interpreted steps and never finished. Every other bulk test
here was already scaled under `cfg(miri)`; that one predates them and was missed. Twenty rounds is
still meaningful, because what Miri looks for is a missing happens-before edge and one insert
either has it or does not.

The red-first requirement of ADR 0041 item 3 is discharged the same way. Against a deliberately
`Relaxed` publishing store (`SkipList::with_relaxed_publication`), Miri says:

```
Data race detected between (1) non-atomic write on thread `unnamed-2`
and (2) retag read of type `[u8]` on thread `unnamed-3`
  --> crates/esker-engine/src/memtable/arena.rs:248   (the reader's slice)
  and (1) occurred earlier here
  --> crates/esker-engine/src/memtable/arena.rs:290   (the writer's copy)
```

which is exactly the writer's payload write racing the reader's payload read, and exactly what a
missing `Release` costs. With the real ordering, the same test suite is clean. A checker that has
never been shown red is evidence of nothing; this one has been.

---

## What the numbers say, in one paragraph

On its own the arena skiplist scans **18.6× faster** (26.14 ms → 1.41 ms for 100 k entries),
does a short scan **6.7× faster**, and is between 1.06× and 1.31× faster on every other read. It
inserts **1.2× to 1.8× slower** — 181 ns against 317 ns for a sequential insert — and that is the
real cost of addressing an arena by offset instead of dereferencing a pointer. End to end through
`esker-cli bench` the two are **indistinguishable**, at a noise floor of about 5% measured from a
control: the read workloads flush the memtable before the clock starts and so cannot show the
scan, and the fill workloads spend their time in the write-ahead log and group commit rather than
in the memtable. The reasons to prefer it are therefore not throughput: three fewer crates,
`unsafe` that Miri can actually check, heights that replay from a seed, and a cursor whose
lifetime is a lifetime rather than a copy.
