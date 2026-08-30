# Phase 3 — replication cost benchmarks

Recorded as `docs/bench/README.md` describes: one file per phase, appended to rather than
overwritten. **Not a gate.** These exist so that a regression in a later phase is a fact
somebody has to explain. Recorded by the `p3-accept` lane as part of the phase-3 acceptance
battery (`prompts/03-raft.md` Acceptance, `docs/plans/phase-3.md`).

## Run 1 — 2026-08-30, the phase-3 acceptance numbers

| Field | Value |
|---|---|
| commit | `91de89a` |
| machine | Apple M4 Max, 16 cores, 128 GiB, built-in NVMe SSD (APFS) — same machine as phase-1/phase-2 |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| build | `cargo build --release -p esker-cli` |
| topology | **single store**: one `esker-cli server`, a one-voter region (no `--peer`) — the phase-3 replicated code path degenerated to *n* = 1, not phase-2's pre-Raft path, so the comparison isolates replication's own cost rather than mixing in "Raft exists at all". **cluster**: `esker-cli cluster start --nodes 3`, driven through the elected leader |
| keys / values | `key%016d` (19 bytes), 100-byte values |
| notes | laptop on mains; two other agents' `esker-cli cluster` processes (debug builds, different ports and data directories) were active on the same machine for part of this run — unlike phase-1/2, not a fully idle machine. Repeated pilot runs agreed to within a few percent regardless, so this is noted for the record rather than treated as a likely confound |

### Single store vs 3-node cluster

| Workload | Topology | Threads | Ops | ops/s | p50 | p99 |
|---|---|---:|---:|---:|---:|---:|
| `fillrandom` | single store | 1 | 300,000 | 19,338 | 50.4 µs | 70.4 µs |
| `fillrandom` | **cluster** | 1 | 600 | 45 | 21,652.0 µs | 30,043.0 µs |
| `fillrandom --sync` | single store | 1 | 5,000 | 226 | 4,259.3 µs | 6,345.2 µs |
| `fillrandom --sync` | **cluster** | 1 | 600 | 45 | 21,961.2 µs | 30,207.9 µs |
| `fillrandom --threads 8` | single store | 8 | 300,000 | 69,810 | 110.6 µs | 185.3 µs |
| `fillrandom --threads 8` | **cluster** | 8 | 800 | 170 | 46,800.0 µs | 70,505.4 µs |
| `fillrandom --sync --threads 8` | single store | 8 | 5,000 | 927 | 8,508.8 µs | 12,687.0 µs |
| `fillrandom --sync --threads 8` | **cluster** | 8 | 400 | 169 | 45,829.9 µs | 67,634.0 µs |

The single-store unsynced number (19,338 ops/s, 50.4 µs p50) lands within 3% of phase-2's
pre-Raft remote measurement (19,533–20,057 ops/s, `docs/bench/phase-2.md`), and the single-store
synced number (226 ops/s, 4,259.3 µs p50) lands within 2% of phase-1's local synced measurement
(231 ops/s, 4,225.6–4,241.2 µs p50). Both are cross-checks that a one-voter Raft group costs
close to nothing beyond what was already there — the replication cost below is a property of
having *more than one* voter, not of Raft's presence.

### The replication cost ratio

At one writer (`--threads 1`): unsynced single-store/cluster ≈ 19,338 / 45 ≈ **430×**; synced
single-store/cluster ≈ 226 / 45 ≈ **5.0×**. At eight writers: unsynced ≈ 69,810 / 170 ≈ **411×**;
synced ≈ 927 / 169 ≈ **5.5×**. Both ratios hold to within a few percent across the concurrency
change, so they are a property of the workload, not an artifact of one run's thread count.

### What the numbers say

**The two `--sync` rows tell the `fsync` story alone, and it is a 5× gap, not the unsynced
rows' 430×.** A single store pays one `fsync` per write (≈4.3 ms, matching phase-1's local
number almost exactly) and the cluster pays ≈22 ms. The reason the gap is so much smaller here
than in the unsynced rows is what `CLAUDE.md` invariant 1 and the driver contract
(`docs/raft-spec.md` §6 "The driver contract", rule D1) require: a leader must persist
`hard_state` and `entries` with `fsync` **before sending any `AppendEntries`**, unconditionally —
a Raft safety rule, not a per-request durability preference, so it fires whether or not the
client's own write asked to wait for it. That is also why the cluster's synced and unsynced rows
are statistically indistinguishable (45 ops/s either way; 21,652 µs vs 21,961 µs p50, 1.4%
apart): once an entry has to leave the leader at all, it has already paid the `fsync` the
client's `sync` flag would otherwise have asked for, and that flag stops being the thing that
decides throughput. Only the *apply* batch (data CFs + `apply_index`) is conditional on it
(`docs/plans/phase-3.md` §11.2), and the apply batch is not where the cost is.

**What is left over — roughly 22 ms minus the single store's ≈4.3 ms `fsync`, about 17 ms — is
quorum RTT in the broad sense**, and more than a bare localhost round trip would predict. Both
transport ends call `set_nodelay(true)` (`esker-proto/src/transport/{client,server}.rs`), which
rules out Nagle's algorithm as the explanation. Candidates that remain: two TCP hops
(leader→follower, follower→leader) for every proposal, a follower's own matching `fsync` before
it acknowledges, and a crossing of the driver's dedicated OS thread boundary in both directions
(`docs/plans/phase-3.md` §11.4 item 3: the Raft core runs off the async reactor so its `fsync`
never stalls the connection pool, which trades a thread hop for that isolation). Which of these
dominates is a question for a profile, not this table — `CLAUDE.md`'s rule is to profile before
optimizing, and nothing here has been tuned; this run is a baseline to profile against, not a
diagnosis.

**Concurrency recovers some of it, proportionally about as well as the single store's own group
commit does.** Eight threads lift the cluster from 45 to ~170 ops/s (3.8×) and the single store
from 19,338 to 69,810 (3.6×) — comparable scaling, so the extra ~17 ms is not obviously worse
under load than the `fsync` cost is. But the cluster's absolute latency floor still climbs (21.7
ms → 46.8 ms p50 at eight threads, versus the single store's 50 µs → 111 µs), which is the
signature of requests queueing behind each other's round trips rather than several folding into
one `fsync` the way phase-1's batched-write row showed (117× from one `fsync` covering 128
writes, `docs/bench/phase-1.md`). Whether the replication path can be given the same kind of
batching is a phase-4 question, not one this baseline answers.

### Methodology note: finding the leader

`esker-cli bench --remote HOST:PORT` takes exactly one address and, unlike `esker-cli raw`
(which accepts a repeated `--addr` and follows a `NotLeader` redirect to a peer it already has a
socket for), has no fallback address to retry against — pointing it at a follower fails outright
rather than redirecting. The leader was found operationally, the way an operator would: probing
each node with `esker-cli raw put --addr <node>` until one did not answer "peer is not the
leader of region 1". This is a real gap between `bench --remote` and `raw`'s redirect-following
for a replicated region, worth recording here rather than working around silently; it is not a
correctness problem (a wrong address just fails clearly) and fixing it is outside this lane's
verification-only mandate.

### How to reproduce

```sh
cargo build --release -p esker-cli

# single store
./target/release/esker-cli server --data-dir /tmp/esker-solo --listen 127.0.0.1:20170 --store-id 1 &
./target/release/esker-cli bench fillrandom --remote 127.0.0.1:20170 --value-size 100 --num 300000
./target/release/esker-cli bench fillrandom --sync --remote 127.0.0.1:20170 --value-size 100 --num 5000

# 3-node cluster — find the leader first (see the methodology note above)
./target/release/esker-cli cluster start --nodes 3 --data-dir /tmp/esker-cluster &
for p in 20160 20161 20162; do ./target/release/esker-cli raw put --addr 127.0.0.1:$p probe hi && echo "leader: $p"; done
./target/release/esker-cli bench fillrandom --remote 127.0.0.1:<leader-port> --value-size 100 --num 600
./target/release/esker-cli bench fillrandom --sync --remote 127.0.0.1:<leader-port> --value-size 100 --num 600
```

`just bench <args>` runs the same driver through cargo, for the in-process (phase-1) form.
