# Phase 6b — SST tiering: what object storage costs

Recorded as `docs/bench/README.md` describes: one file per phase, appended to rather than
overwritten. **Not a gate.** Recorded by the `wy-p6b-tier` lane for
`prompts/06-sql-serverless.md` §6b item 3, which asks for `readrandom` with a cold local cache
and SSTs on `MinIO`, with the p99 and the cache hit rate.

## Method

| Field | Value |
|---|---|
| commit | `7da4762` |
| machine | Apple M4 Max, 16 cores, 128 GiB, built-in NVMe SSD (APFS) — the same machine as phases 1–4 |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| build | `cargo build --release` |
| object store | `minio/minio` at digest `sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e`, in Docker on the same machine, published on `localhost:19000`. **Loopback, not a network** — every latency below is a floor, not an estimate of what S3 would cost |
| workload | `esker-cli bench readrandom --num 200000 --threads 4`, 100-byte values, 10 bits/key bloom, `--no-sync`. The populate phase is untimed, as always |
| tier | `background: false` for the whole run and drained between populate and measurement, so the measured phase is a steady state rather than a measurement of the uploader interfering with it |
| **not idle** | Four other agent sessions were building and testing this workspace throughout; `uptime` load average measured 4.0–6.2 against 16 cores. Read every number as a lower bound |

The command, exactly:

```sh
docker run -d --name esker-minio -p 19000:9000 \
    -e MINIO_ROOT_USER=eskertest -e MINIO_ROOT_PASSWORD=eskertest123 \
    minio/minio:latest server /data
docker exec esker-minio mc alias set local http://127.0.0.1:9000 eskertest eskertest123
docker exec esker-minio mc mb --ignore-existing local/esker

export ESKER_S3_ENDPOINT=http://localhost:19000
export ESKER_S3_KEY=eskertest ESKER_S3_SECRET=eskertest123

esker-cli bench readrandom --num 200000 --threads 4 --dir DIR                        # baseline
esker-cli bench readrandom --num 200000 --threads 4 --dir DIR \
    --sst-store=s3://esker/PREFIX                                                    # warm
esker-cli bench readrandom --num 200000 --threads 4 --dir DIR \
    --sst-store=s3://esker/PREFIX --sst-cache-bytes=0                                # cold
```

## 1. `readrandom`, three configurations

| Configuration | ops/s | p50 | p99 | opens hit | ranged GETs |
|---|---:|---:|---:|---:|---:|
| **local** — no tier, phase-1 shape | 959,981 | 3.8 µs | 8.2 µs | — | — |
| local, repeat | 952,574 | 3.8 µs | 9.0 µs | — | — |
| local, repeat | 969,103 | 3.8 µs | 8.0 µs | — | — |
| **warm** — tiered, cache holds everything | 978,624 | 3.8 µs | 7.5 µs | 4/4 (100%) | 0 |
| warm, repeat | 967,617 | 3.8 µs | 8.2 µs | 4/4 (100%) | 0 |
| warm, first run *(outlier — see below)* | 614,723 | 5.0 µs | 32.4 µs | 4/4 (100%) | 0 |
| **cold** — tiered, cache holds nothing | 2,167 | 1,402.6 µs | 6,704.8 µs | 0/4 (0%) | 200,016 |
| cold, repeat | 2,311 | 1,415.2 µs | 6,468.5 µs | 0/4 (0%) | 200,016 |
| cold, repeat | 2,125 | 1,487.1 µs | 6,621.0 µs | 0/4 (0%) | 200,016 |
| cold, **1 thread**, 20,000 ops | 1,319 | 692.3 µs | 1,384.6 µs | 0/1 (0%) | 20,004 |

**Tiering costs nothing when the file is resident.** Warm and local are indistinguishable —
978k/967k against 952k–969k, with identical p50 and p99. That is the expected result rather than
a pleasant surprise: a resident tiered SST *is* the local file, and `TieredFileSystem::open`
adds one `exists` check per open, of which there were four in the whole run. The first warm run
measured 614k ops/s with a p99 of 32 µs; two later repeats did not reproduce it and the machine
was carrying four other build jobs, so it is recorded as an outlier and not as a cost.

**The open-level hit rate is a coarse number here, and the ranged-GET count is the useful one.**
`TableCache` holds an open reader for the life of the database, so a `readrandom` over four SSTs
opens four files and never opens them again — the hit rate can only be 0% or 100% at that
granularity. It is reported because the phase asks for it; the number that actually describes
the run is the 200,016 ranged `GET`s.

## 2. One cold point read is exactly one round trip

200,016 ranged `GET`s for 200,000 reads is **1.00008 per read**, and that is the whole story of
the cold column.

A point read touches a footer, an index block, a bloom filter and one data block. The first
three are the same blocks for every key in a file, so the block cache absorbs them after the
first read and the steady state is one network round trip for the one data block that differs.
That is what the tiered design is for: the alternative — fetching the whole 2.7 MiB SST to
answer a 100-byte read — would be a 27,000× amplification and is what `fetches 0` confirms did
not happen.

So the cold p50 of 692 µs single-threaded is **the price of one HTTP round trip to `MinIO` over
loopback**, not the price of a read. At four threads it rises to ~1,400 µs and throughput
plateaus at ~2,200 ops/s, which is the server saturating rather than the client.

## 3. Where 692 µs goes, and the obvious next move

692 µs on loopback for a 4 KiB range is not the network. `esker-s3`'s HTTP client sends
`Connection: close` and opens a **fresh TCP connection per request** (`http.rs`: "one connection
per request means no state to get wrong between them, and an upload per flush does not need
pooling"). That was the right call for the uploader, which makes one request per flush. It is
the wrong call for the read path, which now makes one per block.

**Connection reuse is the single change with the most headroom behind it**, and it is local to
`esker_s3::transport` — the trait exists exactly so this is one implementor's problem. It is
deliberately not done in this phase: `CLAUDE.md` says do not tune before correctness is proven,
and the number above is what makes the case for doing it rather than a suspicion that it might
help. Recorded as debt in `docs/plans/phase-6b.md`.

Two smaller ones, in order of expected value:

1. **A whole-file fetch on second use.** The tier already queues one and the governor already
   evicts; with a budget above zero the second visit to a file is local. This bench pins the
   budget at 0 and at ∞ precisely to bracket the two ends, and the interesting middle is a
   working-set question that needs a workload with locality to be worth measuring.
2. **A larger block for tiered reads.** 4 KiB blocks are tuned for a local SSD. A tiered SST
   pays a round trip per block, so a bigger block trades read amplification for round trips —
   a real knob, and one that should be measured before it is added.

## 4. What was not measured, and why

- **Real S3.** This build has no TLS and refuses `https://` at parse time
  ([ADR 0025](../adr/0025-s3-transport-and-tls.md)), so every number here is against `MinIO`
  over loopback. A wide-area round trip is 10–100× the 692 µs above, which changes the
  conclusion of §3 from "worth doing" to "the only thing that matters".
- **Upload throughput.** The phase asks for `readrandom`, and the write path is unchanged by
  tiering by construction: the upload happens after the manifest edit and no writer waits on it
  ([ADR 0024](../adr/0024-tiering-failure-semantics.md) decision 1). The phase-1 `fillrandom`
  numbers therefore still stand, and re-recording them would only measure this machine's load.
- **The mixed hit rate.** See §1: at open granularity there is nothing between 0% and 100% for
  a four-file database. A run with enough files and enough locality to produce a middling hit
  rate would be a better bench, and is worth building when there is a workload that justifies it.
