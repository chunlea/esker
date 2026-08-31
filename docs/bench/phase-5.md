# Phase 5 — transactional benchmarks

Recorded as `docs/bench/README.md` describes: one file per phase, appended to rather than
overwritten. **Not a gate.** These exist so that a regression in a later phase is a fact somebody
has to explain.

The question this file answers is `prompts/05-txn.md`'s last acceptance line: *`txn-put` and
`txn-get` numbers against the RawKV numbers — the ratio is the cost of 2PC + MVCC; explain it.*

## Run 1 — 2026-08-31, the phase-5 acceptance numbers

| Field | Value |
|---|---|
| commit | `7da4762` (binary built there; the threaded pair below ran on the same binary while `HEAD` had moved to `9fb966f` under other lanes' commits) |
| machine | Apple M4 Max, 16 cores, 128 GiB, built-in NVMe SSD (APFS) — same machine as phases 1, 2 and 4 |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, macOS 27.0 |
| build | `cargo build --release -p esker-cli`, default release profile |
| store | one `esker-cli server`, single node, no Raft and no placement driver, one fresh data directory serving all five workloads in sequence |
| keys / values | `key%016d` (19 bytes), 100-byte values; 10,000 operations for the write workloads, 100,000 for the reads |
| timestamps | `CountingOracle` in the benchmark process, **not** PD. What is excluded is the network round trip to the oracle — `bench tso` measures the oracle itself, in-process, at 13M/s (`docs/bench/phase-4-pd.md`), so the excluded cost is the *wire*: two round trips per `txnput` and one per `txnget`, at phase 2's ~50 µs floor. On an 8.5 ms transaction that is about 1%, so it does not move the ratio below; on the read row it would double the latency, and that is worth knowing |
| notes | this machine is shared with other build and test agents; no other benchmark was running, but a load average of about 4 of 16 cores was present from neighbouring compiles. Every row is one long-lived server and one client process, one thread unless the row says otherwise |

### The five rows

| Workload | Durability | Ops | ops/s | MB/s | p50 | p99 |
|---|---|---:|---:|---:|---:|---:|
| `fillrandom --remote` | unsynced | 10,000 | 18,521 | 2.10 | 50.7 µs | 136.5 µs |
| `fillrandom --remote --sync` | one `fsync` | 10,000 | 213 | 0.02 | 4,343.9 µs | 7,326.6 µs |
| **`txnput --remote`** | two `fsync`s | 10,000 | **116** | 0.01 | **8,501.8 µs** | 11,066.2 µs |
| `readrandom --remote` | — | 100,000 | 15,403 | 1.75 | 62.1 µs | 120.8 µs |
| **`txnget --remote`** | — | 100,000 | **15,991** | 1.81 | **61.0 µs** | 101.0 µs |

### The ratio, and what it is made of

**Writes: 1.84× fewer operations per second, 1.96× the latency — and it is the second `fsync`,
not MVCC.**

The row to compare `txnput` against is `fillrandom --sync`, not the unsynced one. A `TxnKv`
request carries no `sync` flag: `RawKvReq::Put` has one and a caller may give up durability by
setting it, and a transaction has no such opt-out, so the store writes every transactional batch
durably (`crates/esker-store/src/server.rs`, the no-Raft path calls `rawkv::write(.., true)`).
Comparing a synced transaction against an unsynced put would measure `fsync` and call it 2PC.

Against the honest column the arithmetic is almost exactly two:

| | synced `RawKv` put | `txnput` |
|---|---|---|
| durable batches | 1 | 2 — the prewrite, then the commit |
| column families touched | `default` | `lock`, then `write` |
| round trips | 1 | 2 |
| measured p50 | 4,343.9 µs | 8,501.8 µs (**1.96×**) |

A 100-byte value never reaches the `default` column family at all: it is at or below the
255-byte inline limit, so it rides inside the `lock` record at prewrite and inside the `write`
record at commit (`docs/txn-spec.md` §3, §4). So the two batches are one record each, and what
the ratio measures is **the number of times the transaction has to be durable**, which is
Percolator's shape and not this implementation's overhead: prewrite must survive a crash before
the commit point exists, or the commit point would be a promise about bytes nobody kept.

MVCC's own bookkeeping — the memcomparable key, the eight-byte inverted timestamp suffix, the
extra column families — does not show up. At 4.3 ms per `fsync` it could not: the whole engine
write is microseconds either way, which is what the unsynced row's 50.7 µs (network) and phase
1's ~2.5 µs (engine) already said.

**Reads: no measurable cost at all.** `txnget` is 61.0 µs against `readrandom`'s 62.1 µs — the
transactional read is *inside the noise of* the raw one, and slightly ahead of it here. Both are
one round trip, and the round trip is 50–60 µs (phase 2, run 1: every remote point operation
lands in the same 46–50 µs band whatever the engine does). Underneath, a transactional read does
strictly more work than a raw one — a seek in `lock` to check for a lock at or below the read
timestamp, then a seek in `write` for the newest version at or below it — and at 100-byte values
the value is inlined in the `write` record, so there is no third seek. Two seeks against one
point lookup, both in the microseconds, both invisible behind the network.

The number that would move it is a **long value**: above 255 bytes the `write` record carries no
inline value and the read costs a third seek, into `default` under the writer's `start_ts`. That
is not measured here and is the obvious next row for whoever needs it.

### Concurrency: the transactional write path does not scale on one node

The same two write workloads at `--threads 4`, same binary, fresh store:

| Workload | Threads | ops/s | p50 | p99 |
|---|---:|---:|---:|---:|
| `fillrandom --remote --sync` | 1 | 213 | 4,343.9 µs | 7,326.6 µs |
| `fillrandom --remote --sync` | 4 | **446** | 8,607.4 µs | 13,607.8 µs |
| `txnput --remote` | 1 | 116 | 8,501.8 µs | 11,066.2 µs |
| `txnput --remote` | 4 | **110** | 34,305.5 µs | 82,233.9 µs |

Four concurrent synced `RawKv` writers get 2.1× the throughput of one, because the engine's group
commit amortises one `fsync` across whatever arrived while it was running. Four concurrent
transactional writers get **nothing** — 110 ops/s against 116 — and their p50 is four times the
single-threaded one, which is the signature of a queue rather than of contention.

That is the **exclusive write gate** on the single-node transactional path. A Percolator operation
is a read-modify-write: what to write depends on what is there, and on a store with no Raft there
is nowhere to make that decision atomically except under a gate that serialises every
transactional command (`docs/plans/phase-5.md` §10.1 explains why the *replicated* path does not
need one: apply is already sequential per region and every peer decides identically). So on one
node, transactional writes are serial by construction, and adding writers only adds queueing.

Two things follow, and neither is a tuning task for this phase:

* a replicated store may do better, because the decision moves into apply and the Raft log's own
  batching can make one durable write out of several entries. Unmeasured — it needs a cluster
  benchmark driver, which `bench --remote` is not.
* the gate is per **store**, not per region, so a store hosting many regions serialises across all
  of them. That is the first thing to look at if transactional write throughput ever matters, and
  the profile should come before the change (`docs/bench/README.md`, "profile before optimising").

### How to reproduce

```sh
cargo build --release -p esker-cli
./target/release/esker-cli server --data-dir /tmp/bench5 --listen 127.0.0.1:20168 &
./target/release/esker-cli bench fillrandom --remote 127.0.0.1:20168 --num 10000  --value-size 100
./target/release/esker-cli bench fillrandom --remote 127.0.0.1:20168 --num 10000  --value-size 100 --sync
./target/release/esker-cli bench txnput     --remote 127.0.0.1:20168 --num 10000  --value-size 100
./target/release/esker-cli bench readrandom --remote 127.0.0.1:20168 --num 100000 --value-size 100
./target/release/esker-cli bench txnget     --remote 127.0.0.1:20168 --num 100000 --value-size 100
```

`txnput` and `txnget` require `--remote`: a transaction's decisions happen at apply, inside a
store, and there is no in-process form of one to measure.
