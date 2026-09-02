# Debt wave C4 — what the recorded debts cost, before and after

Recorded as `docs/bench/README.md` describes. **Not a gate.** One section per unit of
`docs/plans/debt-c4.md` that claims a number.

## 1. The S3 transport keeps its connection (inventory #9)

`docs/bench/phase-6b.md` §3 said where a cold read's 692 µs goes — a fresh TCP connection per
request — and said connection reuse was "the single change with the most headroom behind it",
deliberately not done in that phase. [ADR 0039](../adr/0039-a-kept-alive-s3-connection.md) is that
change; this is the measurement it asked for.

### Method

| Field | Value |
|---|---|
| before | `258e0af` — the commit before the keep-alive |
| after | `097c2e5` — `perf(s3): the transport keeps its connection` |
| machine | Apple M4 Max (`Mac16,6`), 16 cores, 128 GiB, built-in NVMe SSD (APFS) — the same machine as phases 1–4 and 6b |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `cargo build --release` |
| object store | `minio/minio` at digest `sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e`, in Docker on the same machine, on `localhost:19000`. **Loopback, not a network** — every number is a floor |
| workload | `esker-cli bench readrandom`, 100-byte values, 10 bits/key bloom, `--no-sync`, `--sst-cache-bytes=0` (a cold cache), a fresh prefix per run. The populate phase is untimed |
| **not idle** | Other agent sessions were building and testing this workspace throughout; `uptime` load average ran 10.5–16.3 against 16 cores, higher than phase 6b's 4.0–6.2. Read every absolute number as a lower bound |
| **interleaved** | before and after alternate within one script rather than running in two blocks, so load drift is shared rather than assigned to one side. That is the reason the *comparison* survives a loaded box even though the absolute numbers do not match phase 6b's |

Both binaries were built from clean detached worktrees at their commits, because this working tree
carries another lane's in-progress changes to `esker-keys`, which `esker-cli` links.

```sh
export ESKER_S3_ENDPOINT=http://localhost:19000
export ESKER_S3_KEY=eskertest ESKER_S3_SECRET=eskertest123

esker-cli bench readrandom --num 200000 --threads 4 --dir DIR \
    --sst-store=s3://esker/PREFIX --sst-cache-bytes=0     # cold, four threads
esker-cli bench readrandom --num 20000 --threads 1 --dir DIR \
    --sst-store=s3://esker/PREFIX --sst-cache-bytes=0     # cold, one thread
```

### Cold `readrandom`, one thread — one round trip per read, nothing else in the way

| | ops/s | p50 | p99 |
|---|---:|---:|---:|
| before, run 1 | 1,098 | 829.7 µs | 2,247.6 µs |
| before, run 2 | 1,071 | 887.3 µs | 1,567.4 µs |
| before, run 3 | 940 | 949.9 µs | 2,779.7 µs |
| **before, median** | **1,071** | **887.3 µs** | **2,247.6 µs** |
| after, run 1 | 2,311 | 411.4 µs | 641.7 µs |
| after, run 2 | 2,361 | 393.9 µs | 763.8 µs |
| after, run 3 | 2,223 | 422.8 µs | 923.4 µs |
| **after, median** | **2,311** | **411.4 µs** | **763.8 µs** |
| **change** | **2.16× ops/s** | **−54%** | **−66%, or 2.9× lower** |

**The p99 is the headline, and it is the number the brief asked for: 2,247.6 µs → 763.8 µs.**

This configuration is the honest one for the question, because §2 of the 6b bench established that a
cold point read is 1.00008 ranged `GET`s — one round trip, and the run confirms it again with 20,004
ranged reads for 20,000 operations. So at one thread the p50 *is* the cost of one HTTP exchange, and
it falls from 887 µs to 411 µs. **A TCP handshake was roughly half of a cold read.**

The tail moves further than the middle, which is what a handshake removed should do: a connect is
where the variance was. Three runs a side, and the after distribution's *worst* p99 (923.4 µs) is
below the before distribution's *best* (1,567.4 µs) — the two do not overlap.

### Cold `readrandom`, four threads — the server saturates, and it saturates later

| | ops/s | p50 | p99 |
|---|---:|---:|---:|
| before, run 1 | 2,005 | 1,507.6 µs | 7,106.5 µs |
| before, run 2 | 1,910 | 1,580.4 µs | 7,314.4 µs |
| before, run 3 | 1,757 | 1,791.7 µs | 8,866.4 µs |
| **before, median** | **1,910** | **1,580.4 µs** | **7,314.4 µs** |
| after, run 1 | 3,379 | 885.9 µs | 6,229.5 µs |
| after, run 2 | 4,276 | 715.5 µs | 5,697.2 µs |
| after, run 3 | 4,501 | 689.3 µs | 5,316.6 µs |
| **after, median** | **4,276** | **715.5 µs** | **5,697.2 µs** |
| **change** | **2.24× ops/s** | **−55%** | **−22%** |

Throughput and p50 move as much as at one thread; the p99 moves less, and that is the interesting
part. §2 of the 6b bench read the four-thread plateau as *"the server saturating rather than the
client"*, and that reading survives: removing the handshake raises the plateau by 2.2× and does not
remove it, so what is left in the tail is `MinIO` queueing rather than anything this change owns.

### What this does not say

* **Nothing about real S3.** This build has no TLS and refuses `https://` at parse time
  ([ADR 0025](../adr/0025-s3-transport-and-tls.md)), so this is `MinIO` over loopback. A wide-area
  round trip is 10–100× a loopback one, and a handshake there is a *further* round trip on top of
  the request — so the saving is strictly larger, not smaller, and TLS would make it larger again.
* **Nothing about the warm path.** A resident tiered SST is the local file and makes no requests at
  all; 6b §1 measured warm and local as indistinguishable and nothing here touches that.
* **Nothing about the write path.** An upload is one request per flush, which is the shape the
  original per-request connection was written for. It is neither helped nor harmed measurably.
* The absolute numbers are **below** phase 6b's on the same machine (887 µs against 692 µs for the
  same one-thread p50, before the change) because the box carried 10–16 of load rather than 4–6.
  That is why the comparison is interleaved and why the medians, not the bests, are quoted.
