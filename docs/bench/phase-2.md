# Phase 2 — server, wire, and client benchmarks

Recorded as `docs/bench/README.md` describes: one file per phase, appended to rather than
overwritten. **Not a gate.** These exist so that a regression in a later phase is a fact
somebody has to explain.

## Run 1 — 2026-08-30, the phase-2 acceptance numbers

| Field | Value |
|---|---|
| commit | `c5703a6` |
| machine | Apple M4 Max, 16 cores, 128 GiB, built-in NVMe SSD (APFS) — same machine as phase-1 |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| build | `cargo build --release -p esker-cli`, default release profile |
| database | in-process: fresh temporary directory per run. Remote: one long-lived `esker-cli server` and data directory served all six `--remote` workloads in sequence (see methodology note) |
| keys / values | `key%016d` (19 bytes), 100-byte values, 1,000,000 keys, unsynced (no `--sync`) — same shape as phase-1 |
| notes | in-process numbers were re-measured at this exact commit/build rather than quoted from `phase-1.md`, so both columns come from the same binary; laptop on mains, no other deliberate load |

### Remote vs in-process

| Workload | Mode | Ops | ops/s | MB/s | p50 | p99 |
|---|---|---:|---:|---:|---:|---:|
| `fillseq` | in-process | 1,000,000 | 449,068 | 50.96 | 2.0 µs | 4.9 µs |
| `fillseq` | **remote** | 1,000,000 | 19,926 | 2.26 | 48.9 µs | 68.0 µs |
| `fillrandom` | in-process | 1,000,000 | 357,169 | 40.53 | 2.5 µs | 5.5 µs |
| `fillrandom` | **remote** | 1,000,000 | 19,533 | 2.22 | 50.0 µs | 69.1 µs |
| `overwrite` | in-process | 1,000,000 | 354,909 | 40.28 | 2.6 µs | 5.6 µs |
| `overwrite` | **remote** | 1,000,000 | 19,818 | 2.25 | 49.2 µs | 68.6 µs |
| `readrandom` | in-process | 1,000,000 | 392,606 | 44.56 | 2.4 µs | 3.2 µs |
| `readrandom` | **remote** | 1,000,000 | 20,057 | 2.28 | 48.7 µs | 66.3 µs |
| `readmissing` | in-process | 1,000,000 | 3,729,407 | 423.24 | 0.2 µs | 0.4 µs |
| `readmissing` | **remote** | 1,000,000 | 21,316 | 2.42 | 45.7 µs | 63.4 µs |
| `readseq` | in-process | 1,000,000 | 7,292,120 | 827.56 | 0.1 µs | 1.2 µs |
| `readseq` | **remote** | 1,000,000 | 1,562,237 | 177.29 | 0.6 µs | 0.8 µs |

### What the numbers say

Every point operation — `fillseq`, `fillrandom`, `overwrite`, `readrandom` — lands in the same
narrow band on the remote path, 19,500–20,100 ops/s at 46–50 µs p50, regardless of how
differently those same workloads perform in-process (357k–449k ops/s at 2.0–2.6 µs); that
clustering is the signature of a cost that is per-*request* rather than per-*workload*: one TCP
round trip, frame encode/decode, and the demultiplexer matching a response to its waiter, landing
at roughly the same 46–50 µs regardless of what the engine underneath is doing, which dwarfs the
engine's own microsecond-scale work. `readmissing` is the sharpest illustration of that split:
bloom-before-disk (`docs/bench/phase-1.md`, run 2) makes a missing key almost free in-process
(3.73M ops/s) but the remote number barely moves (21,316 ops/s), because the network floor does
not care how quickly the engine answered. `readseq` is the one workload where the gap shrinks to
~4.7× instead of ~18–22× (or `readmissing`'s ~175×), because a scan amortizes that same
per-request cost across up to a limit's worth of pairs in a single response instead of paying it
once per key. None of this is an `fsync` cost — every run here is unsynced, and the store's
`WalSyncMode::Never` plus the request's own `sync` flag (`docs/plans/phase-2.md` §9.0 item 10)
already measured a synced-vs-unsynced loopback gap of 216 vs 16,247 ops/s; the ~19.5–21k ops/s
measured here for full end-to-end unsynced writes over a real socket is the same order of
magnitude as that 16,247, confirming that once the `fsync` stall is removed, the write path is
bound by the network+framing+demux floor exactly like a read is.

### Methodology note

The six `--remote` workloads ran in sequence against one long-lived server and data directory, so
`fillrandom` (remote) wrote into a store `fillseq` had already fully populated with a million
keys, unlike its in-process counterpart, which got a fresh empty directory. In an LSM engine a
random-key write costs about the same whether or not the key already exists — phase-1 already
found `fillrandom` ≈ `overwrite` (341,561 vs 346,217 ops/s) — so this likely does not move the
number much, but it is a real difference in what the two `fillrandom` columns measured, recorded
here rather than presented silently as apples-to-apples.

### How to reproduce

```sh
cargo build --release -p esker-cli

# in-process (phase-1 mode)
./target/release/esker-cli bench fillrandom --value-size 100 --num 1000000

# remote
./target/release/esker-cli server --data-dir /tmp/esker-bench --listen 127.0.0.1:0 &
./target/release/esker-cli bench fillrandom --value-size 100 --num 1000000 --remote 127.0.0.1:<port>
```

`just bench <args>` runs the same driver through cargo, for the in-process form.
