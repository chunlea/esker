# Benchmarks

Benchmarks are **not a gate**. Correctness is proven first; nothing here blocks a phase from
closing. What they are for is making a regression visible: a number that was recorded in phase
1 and is half as good in phase 4 is a fact somebody has to explain, and without a record
nobody would notice.

## How numbers get here

Two tools, for two questions.

* `just bench` runs `esker-cli bench`, the end-to-end workload driver — throughput and latency
  percentiles for a whole `Db` or a whole cluster, the numbers that describe the system. It is
  kept runnable at every phase, even when it only prints "not implemented", so that it never
  quietly rots into something that has to be revived.
* `cargo bench` runs the `criterion` micro-benchmarks that live beside the code they measure —
  a checksum, a block builder, an encoder. These answer "did this function get slower", which
  is a different question from "did the system get slower".

## Recording a run

One file per phase, `docs/bench/phase-N.md`, appended to rather than overwritten, so the
history stays readable. Each entry records:

| Field | Why |
|---|---|
| date and commit | so a number can be tied back to the code that produced it |
| machine | CPU, cores, RAM, and **what kind of disk** — an SSD and a spinning disk are different systems |
| toolchain | `rustc --version`; a compiler upgrade moves numbers on its own |
| build profile | `--release`, and any non-default profile settings |
| workload | key and value sizes, key distribution, thread count, sync mode, duration |
| result | throughput, p50/p99/p99.9 latency, and for writes the stall time |
| notes | anything unusual: a background compaction, a thermal limit, a noisy laptop |

A number without its machine and its workload is not a measurement, and two numbers from
different machines are not a comparison.

## Rules

* **Do not tune before correctness is proven.** A fast engine that loses an acknowledged write
  is not a faster engine (`CLAUDE.md`, "How to work").
* **Profile before optimising.** An optimisation without a profile in front of it is a guess,
  and guesses cost more in complexity than they return in speed.
* **Record the bad numbers too.** A regression that was noticed and accepted is engineering; a
  regression that was quietly not recorded is how a system gets slow.
