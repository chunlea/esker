# Phase 11, lane c5 — the engine's recorded performance debts, before and after

Recorded as [`README.md`](README.md) describes. **Not a gate.** One section per unit of
[`docs/plans/phase-11-engine.md`](../plans/phase-11-engine.md) that claims a number.

## Method, shared by every section

| Field | Value |
|---|---|
| machine | Apple M4 Max (`Mac16,6`), 16 cores, 128 GiB, built-in NVMe SSD (APFS) — the same machine as phases 1–4, 6b and debt wave C4 |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `cargo build --release` |
| **not idle** | Three other agent sessions were building and testing this workspace throughout; `uptime` load average ran 11.3–16.9 against 16 cores. Every absolute number is a lower bound |
| **interleaved** | before and after alternate within one script, five rounds, rather than running in two blocks — see the warning below |

Both binaries are built from clean detached worktrees at their commits, because this working tree
carries another lane's in-progress changes and the next unit's changes at the same time.

### The warning, because this lane walked into it

The first pass ran **all three BEFORE runs and then all three AFTER runs**, while this session was
also compiling. It produced this, for a workload that does not touch the changed code at all:

```
readrandom  BEFORE  310,544 ops/s   p99 13.2 µs
readrandom  AFTER   365,465 ops/s   p99  3.9 µs
```

A 17% throughput gain and a 3.4× tail improvement in a point-read path that never calls
`Db::iter`. It was not real. The BEFORE block ran while this session was compiling `esker-engine`
and the AFTER block ran after it had stopped, so the difference measured was the compiler.

Interleaved, the same workload reads **370,833 / 370,462 / … before** against **369,628 / 372,702
/ … after** — flat, which is what a change to the iterator ought to do to a point read.

`readrandom` is carried through every section from here on **as a control**, precisely because it
should not move. A section where it moves is a section to distrust.

---

## 1. The two-level iterator (plan §3)

`before`: `4d0e098`, with only this lane's bench flags copied in, so the two binaries differ by the
iterator and nothing else.
`after`: `07a3b97` — `perf(engine): one cursor per level, opening the file it has reached`.

### Workload

```sh
esker-cli bench <workload> \
    --num 400000 --value-size 100 \
    --write-buffer-size 262144 --target-file-size 262144 --compact \
    --dir <fresh> [--batch-size 20 for scanrange]
```

400,000 keys of 100 bytes with a 256 KiB memtable and 256 KiB output files, compacted before the
measured phase — which leaves a level below L0 holding roughly 190 files. Without `--compact`
everything is in L0, and L0 is the one level this change deliberately leaves alone; without the
small `--target-file-size` the compaction writes one output file and there is nothing to be lazy
about. Both flags are new in this commit for that reason.

The fill and the compaction are untimed.

### Result

Five interleaved rounds. Median of five, with the range, because the box was not idle and one
`readrandom` round and one `scanrange` round were badly disturbed — interleaving puts that
disturbance on both arms and the median absorbs it.

| workload | before (median) | before (min–max) | after (median) | after (min–max) | ratio |
|---|---|---|---|---|---|
| `readseq` | 4,593,789 ops/s | 4,221,168 – 4,649,289 | 6,923,984 ops/s | 6,712,432 – 7,108,010 | **1.51×** |
| `scanrange` (20/scan) | 37,834 ops/s | 20,605 – 38,047 | 177,515 ops/s | 154,371 – 177,968 | **4.69×** |
| `readrandom` *(control)* | 370,462 ops/s | 221,390 – 372,413 | 365,776 ops/s | 175,552 – 372,702 | 0.99× |

**The control at 0.99× is what makes the other two rows worth reading.** A point read walks the
levels by index lookup and never builds an iterator, so a change to iterator construction has to
leave it alone. It does.

The `scanrange` figure is a ratio of two medians on one machine at one shape — 400,000 keys of 100
bytes over roughly 190 files in one level, 20 entries per scan, block cache warm from the fill. It
is not a claim about scans in general: shrink the level to a handful of files and the ratio
collapses toward one, because there were never many files to open.

A second measurement in the same commit, from `tests/level_iter.rs` rather than the bench: creating
an iterator over a database with 3 files below L0 opened **3** table readers before the change and
**0** after (L0 was empty in that fixture). That is the mechanism the ratios above are the
consequence of.

### Reading it

`scanrange` is the workload this change is about and `readseq` is the one that flatters it least.
One scan of the whole database builds a single iterator and walks 400,000 keys with it, so whatever
building it cost is divided by 400,000; `scanrange` builds one per operation and divides by 20.
Every index lookup, prefix scan and `LIMIT` the SQL layer will issue is the second shape, and none
of them are the first.

`readrandom` is flat, as the control should be: a point read goes through `db/read.rs`, which walks
the levels by index lookup and never builds an iterator.

---

## 2. The table cache evicts by use (plan §4)

Measured **in the test rather than the bench**, and the reason is worth stating: `esker-cli bench`
has no workload whose working set of *files* exceeds the cache, because the cache holds 256 readers
and a benchmark database has fewer files than that unless it is enormous. Turning
`Options::max_open_tables` down is the only way to ask the question at a size that runs in
milliseconds, and that is a knob a test sets, not a flag.

`tests/table_cache.rs::a_file_read_every_time_is_not_the_one_evicted`: 2,000 keys over 7 files with
room for **3** readers; 200 rounds of (hot key, a wandering key, cold key). Two of the three files
touched per round never change.

| victim rule | hit rate | hits | misses | evictions |
|---|---|---|---|---|
| **least-recently-used** (after) | **0.992** | 595 | 5 | 254 |
| lowest file number (before) | 0.615 | 369 | 231 | 469 |

Both rows come from the same test binary; the "before" row is the same test with the old victim
selection patched back in.

The eviction count is the part that says the old rule was worse than random rather than merely
different: it evicts **1.85× as often**, because everything it discards is something that gets
asked for again. File numbers rise monotonically, so the lowest is the oldest file, which in a
levelled engine is the one that has survived the most compactions — the deepest and most-read one
in the tree.

No end-to-end throughput number is claimed. On a database whose files all fit in the cache the two
rules are indistinguishable, and that is most databases; the debt is what happens to the ones where
they are not.

---

## 3. The tiered read block size (plan §5)

The plan said: **measure first, and add the knob only if the numbers say so.** They say not to,
and they say something more useful instead.

### Method

| Field | Value |
|---|---|
| object store | `minio/minio` at digest `sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e`, in Docker on `localhost:19000`. **Loopback, not a network** — every absolute number is a floor and every *round trip* is far cheaper than it would be against real S3 |
| commit | `0460c9d` plus the `--block-size` flag |
| workload | `esker-cli bench <w> --num 50000 --value-size 100 --block-size N --sst-store s3://esker/<fresh> --sst-cache-bytes 0` |
| cold | `--sst-cache-bytes 0` keeps nothing locally, so **every block read is a ranged `GET`**. The fill, the upload and the drain are untimed |
| runs | one per cell. The effect sizes below are large enough that a single run separates them; the one cell that is a judgement call is called out |

### Result

| block | `readrandom` | ranged GETs | `readrandom` p99 | `readseq` | ranged GETs |
|---|---|---|---|---|---|
| 4 KiB *(default)* | 2,044 ops/s | 50,004 | 1,411 µs | 67,894 ops/s | 1,393 |
| 16 KiB | **2,284 ops/s** | 50,004 | 713 µs | 280,552 ops/s | 349 |
| 64 KiB | 2,151 ops/s | 50,004 | 729 µs | 823,364 ops/s | 91 |
| 256 KiB | 1,420 ops/s | 50,004 | 4,360 µs | **1,000,615 ops/s** | 26 |

### What it says

**A point read is one `GET` whatever the block size** — 50,004 for 50,000 operations at every
size, because `TieredFile::read_at` issues exactly one ranged request per call and a point read
touches one block. So the block size cannot buy a point read a round trip; it can only change how
many bytes that round trip carries. From 4 KiB to 64 KiB that is free (2,044 → 2,151, within
noise of each other), and at 256 KiB it stops being free: **1,420 ops/s and a p99 of 4.4 ms**,
because a quarter-megabyte transfer to read one 100-byte value finally costs more than the round
trip it saved nothing on.

**A scan is the opposite.** Its GET count falls exactly in proportion — 1,393 → 349 → 91 → 26 —
and its throughput rises with it, 14.7× from 4 KiB to 256 KiB.

### Decision: no new knob

The knob the plan contemplated is a **tiered-read block size**, separate from the SST block size,
so that a cold read could fetch a larger window than the file was written with. The measurement
says that is the wrong shape:

* the round-trip count for a **point** read is one regardless, so a larger read window buys it
  nothing and costs it bytes — which is the 256 KiB row;
* the round-trip count for a **scan** is already governed exactly by `CfOptions::block_size`, an
  option that exists;
* so a second size would be an option that duplicates the first for scans and actively harms
  point reads, and there is no setting of it that a reader could choose correctly without knowing
  which of the two workloads was about to arrive.

`docs/plans/phase-11-engine.md` §5 said a configuration option nobody can choose correctly is a
liability rather than a feature. This is one. **Nothing was added.**

### What the numbers did change: the tiered default (U6)

The measurement said 4 KiB is a poor default *when the SSTs are tiered*, and phase 11 U6 acted on
it. `defaults::TIERED_BLOCK_SIZE` is **16 KiB** and applies to a database whose filesystem has a
tier; the local default stays at 4 KiB, untouched.

16 KiB rather than 64 KiB, which is faster still for scans: the point-read column is already flat
between them (2,284 against 2,151, one run each — not a difference this table can resolve), and
256 KiB shows where flat ends. Between two sizes that are within noise for point reads, the smaller
one wastes fewer bytes on the read that only wanted one key.

It is expressed as `BlockSize::Storage` versus `BlockSize::Fixed(n)` rather than as a different
default number, so that a caller who deliberately asks for 4 KiB on tiered storage still gets it.
Overriding a value "when it happens to equal the default" would make an explicit choice
indistinguishable from no choice — `dd182cb`'s bug in a different field.

The end-to-end effect, from `tests/tiered_block_size.rs` rather than from this bench: the same
4,000 keys come to **54 data blocks at 4 KiB and 14 at 16 KiB**, a 3.9× reduction against the 4×
the size ratio predicts. A data block is one ranged `GET`, so that count *is* what a cold scan
costs.

### The honest limits of this table

* **Loopback.** A real S3 round trip is 10–100× this one, which makes the round-trip term larger
  and every "fewer GETs" row *better* than it looks here — and makes the 256 KiB point-read
  regression *smaller*, because the bytes would be a smaller fraction of a longer trip. The
  ranking of the scan rows is safe; the exact position of the point-read crossover is not.
* **One run per cell.** The scan column spans 14.7× and the point-read column 1.6×, so the shape
  survives ordinary noise; 16 KiB versus 64 KiB for point reads (2,284 against 2,151) does not,
  and is why the recommendation above names a range rather than a winner.
* **One value size.** 100 bytes. A database of 10 KiB values would put the crossover somewhere
  else entirely.
