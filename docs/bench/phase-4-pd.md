# Phase 4 — the placement driver's two allocators

Recorded as `docs/bench/README.md` describes: one file per phase, appended to rather than
overwritten. **Not a gate.** These exist so that a regression in a later phase is a fact somebody
has to explain — and phase 5 is the phase that will care, because Percolator takes a `start_ts`
and a `commit_ts` from the oracle for *every* transaction (`docs/DESIGN.md` §8), which puts it on
the critical path of everything above it. Recorded by the `cl-p4-pd` lane.

The store-side phase-4 numbers — `fillrandom` with 1, 3 and 5 stores, region counts, split
distribution — belong to `docs/bench/phase-4.md` and the store lane.

## Run 1 — 2026-08-30, PD v1 (4a)

| Field | Value |
|---|---|
| commit | `0eba75d` |
| machine | Apple M4 Max, 16 cores, 128 GiB, built-in NVMe SSD (APFS) — same machine as phases 1–3 |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| build | `cargo build --release -p esker-cli` |
| topology | **in-process**: `esker-cli bench tso` / `allocid` open a `Pd` in a temporary directory and call it directly. No network — `--remote` is refused for these workloads, because `bench --remote` speaks `RawKv` to a store and a placement driver is not one |
| defaults exercised | `ALLOC_BATCH` = 1,000 ids per reservation; TSO save interval = 3 s (`docs/DESIGN.md` §14) |
| notes | laptop on mains, otherwise idle. `--num` counts **values**, so runs at different `--batch-size` are the same amount of work |

### The reference the other two numbers are read against

| Workload | Threads | Ops | ops/s | p50 | p99 |
|---|---:|---:|---:|---:|---:|
| `fillseq --sync` | 1 | 2,000 | 229 | 4,228.5 µs | 6,066.6 µs |

One `fsync` on this machine costs **≈ 4.2 ms**. That is the same number phase 1 recorded (231
ops/s, 4,225.6 µs) and phase 3 re-measured (226 ops/s, 4,259.3 µs), so the machine has not moved
and both numbers below can be checked against it by hand.

### `tso` — the oracle

| Batch | Threads | Ops | ops/s | p50 | p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 2,000,000 | **12,173,784** | 0.0 µs | 0.1 µs |
| 4 | 1 | 2,000,000 | 48,941,289 | 0.0 µs | 0.0 µs |
| 16 | 1 | 2,000,000 | 120,456,233 | 0.0 µs | 0.0 µs |
| 64 | 1 | 2,000,000 | 189,742,825 | 0.0 µs | 0.0 µs |
| 1,024 | 1 | 1,999,872 | 235,048,601 | 0.0 µs | 0.0 µs |
| 1 | 1 | **200,000,000** | **14,765,436** | 0.0 µs | 0.1 µs |

**The oracle's `fsync` is structurally free, and the batch sweep is why.** The mark is persisted
3 s *ahead* of what is handed out (`docs/adr/0010-pd-durable-state.md`), so a run pays one
`fsync` per three seconds of issued time whatever its rate: the 13.5-second run at the bottom of
the table crossed about four marks, spending ~17 ms of 13,500 ms — **0.13%** — on durability, and
came out *faster* than the 0.16-second run at the top. What the numbers measure is therefore the
lock and the arithmetic: **≈ 82 ns per call** and **≈ 4.3 ns per timestamp** once a call carries
1,024 of them.

For phase 5 the useful figure is the first row: **12.2 M timestamps/s single-threaded, one per
call**. A transaction needs two, so the oracle is not the thing that will limit it.

### `tso` and `allocid` — thread scaling, one value per call

| Workload | Threads | Ops | ops/s | p50 | p99 |
|---|---:|---:|---:|---:|---:|
| `tso` | 1 | 2,000,000 | 13,066,312 | 0.0 µs | 0.1 µs |
| `tso` | 2 | 2,000,000 | 9,522,081 | 0.0 µs | 4.4 µs |
| `tso` | 4 | 2,000,000 | 4,453,631 | 0.0 µs | 10.1 µs |
| `tso` | 8 | 2,000,000 | 5,254,606 | 0.0 µs | 22.6 µs |
| `allocid` | 1 | 2,000,000 | 223,300 | 0.1 µs | 0.1 µs |
| `allocid` | 2 | 2,000,000 | 216,982 | 0.0 µs | 9.1 µs |
| `allocid` | 4 | 2,000,000 | 211,399 | 0.1 µs | 25.3 µs |
| `allocid` | 8 | 2,000,000 | 214,075 | 0.1 µs | 82.6 µs |

**Threads make the oracle slower, not faster** — 4 threads is 2.9× *worse* than one. Allocation is
read-modify-write over state that must not interleave, so it is serialised behind one mutex
(`crates/esker-pd/src/pd.rs`), and at 82 ns of real work per call the contention costs more than
the work. This is a recorded property, not a defect to fix today: the answer for a caller that
wants more is a larger batch, which the sweep above shows scaling almost linearly, and that is
also how a real client uses a TSO. p99 climbing from 0.1 µs to 22.6 µs across the sweep is the
same fact from the latency side.

`allocid` is flat under threads because it is not bound by the mutex at all — see below.

### `allocid` — the allocator

| Batch | Threads | Ops | ops/s | p50 | p99 |
|---:|---:|---:|---:|---:|---:|
| 1 | 1 | 2,000,000 | 223,039 | 0.1 µs | 0.1 µs |
| 16 | 1 | 2,000,000 | 227,928 | 0.0 µs | 261.3 µs |
| 1,024 | 1 | 1,999,872 | 231,878 | 4.1 µs | 6.7 µs |

**The caller's batch size does not matter; the reservation's does.** All three rows do the same
number of `fsync`s — 2,000, one per 1,000 ids — because `ALLOC_BATCH` is what decides when the
allocator persists, not how many ids a caller asked for in one go. The arithmetic closes exactly:

> 229 synced writes/s × 1,000 ids per reservation = **229,000 ids/s**, against 223,039 measured.

So `allocid` is a direct measurement of "one `fsync` per 1,000 ids" and nothing else. 4b takes
ids for every split; at 223 k ids/s the allocator is roughly five orders of magnitude away from
being the thing that limits a split storm.

The p99 of 261.3 µs in the middle row is the reservation landing inside one caller's batch of 16
rather than being spread over 1,000 single calls — the same total cost, attributed to fewer
operations.

## What is not measured here, and why

- **The network.** These are in-process numbers. A `Pd` call over TCP costs the round trip
  `docs/bench/phase-2.md` already records (≈ 46 µs), which dominates everything above by three
  orders of magnitude — so the numbers here are the ceiling that round trip is subtracted from,
  not a prediction of what a client sees. When 4e makes PD three nodes, the interesting number
  becomes a Raft round trip per mark rather than an `fsync` per mark, and this file gets a run 2.
- **`GetRegion` and the heartbeats.** They are engine reads and writes of one small record; the
  engine's own numbers in `docs/bench/phase-1.md` describe them, and there is no amortisation
  story to tell. If the routing table ever stops being one seek, that is when they need a row.
- **A regression threshold.** There is none, deliberately. `CLAUDE.md`: benchmarks are not gates,
  and nothing is tuned before a profile.
