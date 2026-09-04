# 0003 — The dependency allowlist

Date: 2026-08-30 · Status: accepted · Phase: 0

## Context

`CLAUDE.md` sets the rule: Esker is 100% Rust, and no crate that compiles C, C++ or assembly
may appear anywhere in the dependency graph, including transitively. Anything that is a few
hundred lines and part of what this project exists to learn is written in-house.

That rule needs three things to be more than an intention: a list of what is allowed and why, a
machine check that the list is respected, and a budget so the graph cannot grow one reasonable
crate at a time.

## Decision

The runtime allowlist below. Adding to it requires an ADR. Everything else is denied, and two
independent mechanisms enforce that.

### Runtime dependencies

| Crate | Why it is here | What replacing it would take |
|---|---|---|
| `tokio` | The network edge only: TCP, timers, the task runtime behind `esker-proto`. The engine and the Raft core stay synchronous and `std`-only. | An in-house reactor over `mio`-shaped polling, or blocking threads per connection. Weeks, and it is not what this project is about. |
| `bytes` | `Bytes`/`BytesMut`: cheap refcounted slices, so a value read from a block can reach a client without being copied at each layer. | An in-house `Arc<[u8]>` slice type. A few hundred lines, genuinely feasible; kept because the ecosystem convention is worth more than the exercise. |
| `thiserror` | Derives `Display` and `Error` for the per-crate error enums. Compile-time only; nothing of it survives into the binary. | Hand-written `impl Display` per enum. Mechanical, verbose, no behaviour change. Cheap to do if the proc-macro chain ever needs to go. |
| `tracing` | Structured spans per request, carrying region and peer ids. Log lines without those are unusable in a distributed system. | An in-house span-and-event facade. Doable; the collector ecosystem is the reason not to. |
| `tracing-subscriber` | Turns those spans into output, with `RUST_LOG`-style filtering. | A hand-written subscriber; `tracing` is the trait, this is one implementation. |
| `lz4_flex` | Pure-Rust LZ4 for SST block compression — the **only** compression codec. `zstd` and `snap` are banned because both are C. | An in-house LZ4 decoder (the format is simple) plus an encoder (harder to make fast). Or no compression, which costs disk. |

`crossbeam-skiplist` used to be the last row of that table — "the one piece of concurrent unsafe
code we buy rather than write" — and it is gone. The memtable is an in-house arena skiplist as of
[ADR 0041](0041-the-in-house-arena-skiplist.md), and with it went `crossbeam-epoch` and
`crossbeam-utils`: three runtime crates, and the budget below came down from 40 to 37 in the same
change. Nothing else in the workspace depended on any of them.

After it, **every piece of concurrent code in the engine is code in this repository with a test in
this repository** — which also means the memtable's tests can be run under Miri, which they could
not while `crossbeam-epoch` was in the graph (`docs/bench/skiplist.md` §3).

### Development dependencies

Not shipped, so the pure-Rust rule does not bind them; `proptest` depends on `rand`, which is
banned at runtime, and that is fine.

| Crate | Why |
|---|---|
| `proptest` | Property tests: round trips and order preservation over generated input, with shrinking. |
| `criterion` | Micro-benchmarks with confidence intervals, so a "regression" is distinguished from noise. |
| `stateright` | Model checking of Raft safety properties. |
| `tempfile` | Scratch directories for engine crash tests. |
| `sqllogictest` | Phase 6 only, for the SQL layer. Not yet in any manifest. |

### Written in-house instead

`crc32c` (slicing-by-8 plus a CRC32C instruction path), varints and every framing byte, bloom
filters, `hash64` for cache keys, the sharded LRU block cache, the memcomparable codec, the
seeded PCG32 generator, Raft itself, the RPC framing, and the PostgreSQL wire protocol. Each is
small, each is part of the subject matter, and each has a crate we are deliberately not using
(`crc32c`, `crc32fast`, `rand`, `lru`, `pgwire`).

### Explicitly banned

`rocksdb`, `sled`, `openraft`, `raft-rs` — the project is to build these. `tonic`, `prost`,
`serde` — see ADR 0002. `zstd`, `snap`, `openssl`, `ring`, `aws-lc-*`, `libz*`, `cc`, and
anything matching `*-sys` — C.

### Deferred, to be settled by ADR when reached

* **`sqlparser`** (phase 6a). Writing a full PostgreSQL parser is out of scope; this is the one
  large exception we expect to accept.
* **An S3 client and TLS** (phase 6b). The S3 calls are small enough to write. TLS is the hard
  case for the pure-Rust rule; the options — plain HTTP to a local MinIO or a terminating
  sidecar, `rustls` with a pure-Rust provider, or one vetted exception — are recorded in
  `docs/DESIGN.md` §13.

## Enforcement

Two mechanisms, because they fail differently.

1. **`deny.toml`**, checked by `cargo deny check` inside `just check` and CI. It names crates
   we already know about, denies duplicate versions and wildcard requirements, restricts
   licences, and excludes dev dependencies from the graph. Verified non-vacuous by temporarily
   adding `rand` to a crate and confirming the check failed.
2. **`crates/esker-cli/tests/dep_budget.rs`**, which walks the resolved graph from
   `cargo metadata` and checks names against the *patterns* — `*-sys`, `openssl*`, `aws-lc-*` —
   that a list of exact names cannot express. It also counts the graph.

**The budget is 37 transitive runtime crates**, recorded in a marked comment in `deny.toml` so
that both mechanisms read the same number. At the end of phase 0 the count is **7**: `bytes`,
`thiserror` and `thiserror-impl` with their proc-macro chain (`proc-macro2`, `quote`, `syn`,
`unicode-ident`). Lowering the budget is welcome; raising it needs an ADR.

## Consequences

* Some work that could be a `cargo add` is instead a module with tests. That is the intended
  trade, and it is the largest recurring cost of this decision.
* Adding a dependency is a visible act with a written justification, not a silent one.
* Build times stay short and the binary stays small, which makes the crash-test loops in later
  phases cheap enough to run often.
* When `tokio` arrives in phase 2 the count will jump — `mio`, `libc`, `socket2`,
  `pin-project-lite` and the macro chain. It should stay well inside the budget; if it does not,
  the first move is to trim `tokio`'s feature list, not to raise it.
