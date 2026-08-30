# Phase 1 — engine benchmarks

Recorded as `docs/bench/README.md` describes: one file per phase, appended to rather than
overwritten. **Not a gate.** These exist so that a regression in a later phase is a fact
somebody has to explain.

## Run 1 — 2026-08-30, the phase-1 acceptance numbers

| Field | Value |
|---|---|
| commit | `489901a` (step 9, engine complete) |
| machine | Apple M4 Max, 16 cores, 128 GiB, built-in NVMe SSD (APFS) |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| build | `cargo build --release -p esker-cli`, default release profile |
| database | fresh temporary directory per run, default options, `wal_sync_mode = Never` so that only `--sync` decides durability |
| keys / values | `key%016d` (19 bytes), 100-byte values, 1,000,000 keys unless stated |
| notes | laptop on mains, no other load; each run creates and deletes its own database |

### Throughput and latency

| Workload | Ops | Threads | Batch | Sync | ops/s | MB/s | p50 | p99 |
|---|---:|---:|---:|:--:|---:|---:|---:|---:|
| `fillseq` | 1,000,000 | 1 | 1 | no | 457,372 | 51.91 | 2.0 µs | 4.8 µs |
| `fillrandom` | 1,000,000 | 1 | 1 | no | 341,561 | 38.76 | 2.6 µs | 5.8 µs |
| `overwrite` | 1,000,000 | 1 | 1 | no | 346,217 | 39.29 | 2.6 µs | 5.7 µs |
| `readrandom` | 1,000,000 | 1 | — | — | 387,967 | 44.03 | 2.4 µs | 3.5 µs |
| `readseq` | 1,000,000 | 1 | — | — | 7,472,038 | 847.98 | 0.1 µs | 1.3 µs |
| `fillseq --sync` | 10,000 | 1 | 1 | **yes** | 231 | 0.03 | 4,225.6 µs | 5,533.9 µs |
| `fillrandom --sync` | 10,000 | 1 | 1 | **yes** | 231 | 0.03 | 4,241.2 µs | 5,511.1 µs |
| `fillseq --sync --batch-size 128` | 100,000 | 1 | 128 | **yes** | 27,072 | 3.07 | 33.9 µs | 49.8 µs |
| `fillrandom --threads 8` | 1,000,000 | 8 | 1 | no | 203,480 | 23.09 | 36.8 µs | 79.9 µs |

The synced runs use 10,000 keys rather than a million, the way `db_bench` scales `fillsync`:
at 4 ms an `fsync` a million of them is an hour of waiting to learn what one of them costs.

### What the numbers say

**An `fsync` costs about 4 ms on this disk, and that is the whole story of the synced rows.**
231 ops/s is 1/(4.3 ms), so the engine is doing exactly one flush per write and adding almost
nothing to it. This is the number to watch: it is a property of the disk, and if it ever
improves without the disk changing, something has stopped syncing.

**Group commit is worth 117×.** The same synced workload at a batch size of 128 reaches 27,072
ops/s, because one `fsync` covers the whole batch — which is what `docs/DESIGN.md` §4.2 says
group commit is for, measured rather than assumed.

**Eight threads are slower than one** (203k vs 342k ops/s, and p50 fourteen times worse). Every
writer queues behind one group-commit leader, so with unsynced writes there is no `fsync` for
the batching to amortise and the queueing is pure cost. This is expected for `sync = false`
and is exactly the shape the design predicts; it is recorded because it is the kind of number
that looks like a bug later if nobody wrote down that it was understood. **No tuning has been done**
(`CLAUDE.md`: profile before optimising), and none should be until phase 2 gives the engine a
caller with a real concurrency profile.

**`readseq` is twenty times `readrandom`.** A scan walks the merge cursor's children in order
and never seeks; a point read seeks each level and, today, cannot use the bloom filter — see
the `TODO(post-v1)` in `src/db/read.rs`. That gap is the first thing to measure again once the
filter-aware seek lands.

**`fillseq` beats `fillrandom` by a third**, which is the memtable's skiplist: appending at the
end walks no towers, while a random key walks the whole structure.

### How to reproduce

```sh
cargo build --release -p esker-cli
./target/release/esker-cli bench fillrandom --value-size 100 --num 1000000
./target/release/esker-cli bench readrandom --value-size 100 --num 1000000
./target/release/esker-cli bench fillseq --sync --value-size 100 --num 10000
```

`just bench <args>` runs the same driver through cargo.
