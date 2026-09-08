# Esker — project constitution

Esker is a distributed, horizontally scalable, transactional key-value store written **from scratch in Rust**:
a log-structured storage engine (WAL + memtable + SST + compaction), a home-grown **Multi-Raft** replication
layer, **range sharding** with a placement driver, and **Percolator-style** distributed transactions.
Long term it is the storage foundation of a serverless database that speaks PostgreSQL syntax
(TiDB-shaped, Postgres-flavored). An esker is a long, stratified ridge of sediment laid down in order by a
stream — layers deposited by a log, which is exactly what this system is.

Read this file, then `docs/DESIGN.md`, before touching code. Precedence: this file > `docs/DESIGN.md` >
`prompts/*.md` > existing code. If code and DESIGN.md disagree, fix one of them in the same change —
they must never drift.

## What it must be good at

- Ordered keys with cheap prefix and range scans (SQL tables and indexes become key ranges).
- MVCC with the version encoded in the key suffix; a point read is "first key under a prefix".
- Column families that are physically separate but share one WAL (atomic multi-CF writes).
- Write throughput from sequential I/O; predictable tail latency (no unbounded write stalls).
- Linear scale-out by splitting key ranges into regions, each replicated by its own Raft group.
- Crash safety on every layer: kill -9 at any instant must never lose an acknowledged write.

## Architecture in one screen

| Layer | Crate | Responsibility | Analogy |
|---|---|---|---|
| SQL (later) | `esker-sql` | Postgres wire protocol (in-house), parser, planner, executor; stateless | TiDB |
| Transactions | `esker-txn` | Percolator 2PC over `lock`/`write`/`default` CFs, TSO | TiKV txn |
| Client | `esker-client` | region cache, routing, retries, txn API | TiKV client |
| Placement | `esker-pd` | membership, routing table, TSO, scheduling | PD |
| Store | `esker-store` | one process, many regions; apply loop; split; snapshots | TiKV raftstore |
| Consensus | `esker-raft` | pure-state-machine Raft (`RawNode`/`Ready`) | raft-rs |
| Engine | `esker-engine` | WAL, memtable, SST, manifest, compaction, CFs | LevelDB/RocksDB |
| Wire | `esker-proto` | hand-rolled framed RPC over TCP: RawKV, TxnKV, PD, RaftTransport | kvproto |
| Keys | `esker-keys` | memcomparable codec, reserved key-space layout, **the row-value codec and the six stored types** (ADR 0030) | TiDB codec |
| Sim/test | `esker-sim` | deterministic simulator, fault injection, checkers | madsim/Jepsen-lite |
| Tools | `esker-cli` | bootstrap, inspect, sst-dump, wal-dump, bench | ldb / db_bench |
| Primitives | `esker-base` | crc32c, varints, hash64, seeded PCG32 — no key semantics (ADR 0004) | util/ |

## Non-negotiable invariants

1. **Log before state, fsync before ack.** A write is acknowledged only after its WAL/Raft-log bytes are
   durable, unless the caller explicitly passed `sync = false`. Never reorder these steps.
2. **Every on-disk byte is checksummed** (CRC32C) and every file has a magic + format-version header or
   footer. Corruption is returned as an error value, never a panic, never silently skipped.
3. **Immutable files, atomic pointers.** SSTs, Raft snapshots and closed WAL segments are never modified
   in place. The only mutable pointer is a manifest/CURRENT-style file replaced by atomic rename after fsync.
4. **Raft core is a pure deterministic state machine.** `esker-raft` contains no threads, timers, sockets,
   or file I/O. Time enters via `tick()`, messages via `step()`, effects leave via `Ready`. This is what makes
   it simulatable and model-checkable; do not compromise it for convenience.
5. **Every request carries a region epoch.** A stale epoch is rejected with a redirect hint; a store never
   serves a request for a range it no longer owns.
6. **Timestamps come only from PD's TSO.** No node uses its wall clock for ordering.
7. **Engine and Raft are byte-opaque.** Key semantics (tenant, table, MVCC suffix) live only in
   `esker-keys` and above.
8. **No `unsafe` without a `// SAFETY:` comment and a test that exercises it.** Prefer safe code even at a
   measurable cost; we optimize after a profile, not before.
9. **Never panic on user input or on-disk data.** `unwrap()`/`expect()` are allowed only on invariants
   proven in the same function, with a comment saying why.

## How to work

- **Phases are gates.** Work through `prompts/` in order. Do not implement a later phase's feature early
  "because it's easy"; put a `// TODO(phase-N)` and move on.
- **Plan first, in writing.** Start every phase by writing `docs/plans/phase-N.md`: scope, file list, public
  API sketch, test list, risks, and what you will NOT do. Then implement in vertical slices; every slice
  compiles, is tested, and is committed on its own (`feat(engine): sst block builder with restart points`).
- **Tests are the product.** For each component the required test kinds are listed in DESIGN.md §11.
  A component without its crash test or property test is not done. Bug fixes land with a regression test.
- **Write ADRs** for any decision that a future reader might reverse: `docs/adr/NNNN-title.md`
  (context, options, decision, consequences). Format changes, dependency additions, and protocol changes
  always get an ADR.
- **Keep files under ~800 lines** and modules single-purpose. Public items get doc comments that explain
  the invariant, not just the signature.
- **Benchmarks are not optional but are not gates.** Keep `esker-cli bench` runnable at every phase and
  record numbers in `docs/bench/` so regressions are visible; do not tune before correctness is proven.
- **Definition of done for a phase:** the phase's acceptance checklist passes, `just check` is green,
  DESIGN.md reflects what was built, the plan file is updated with what changed and why.

## Dependency policy: pure Rust, as few crates as possible

The project is **100% Rust**. No crate that compiles C/C++/assembly (`*-sys` crates, anything using the
`cc` crate or a `links` key) may appear anywhere in the dependency graph, including transitively. CI
enforces this with `cargo deny` (`deny.toml` bans `*-sys`, `cc`, `openssl*`, `ring`, `aws-lc-*`,
`libz*`, `zstd*`) plus a test that fails if the number of transitive **runtime** crates in the workspace
exceeds the budget in `deny.toml` (40 at the start, **37** since the memtable stopped buying its
skiplist; lowering it is welcome, raising it needs an ADR).

Build it ourselves by default. Anything that is a few hundred lines and part of what we are learning is
written in-house with tests: CRC32C (table-based; `std::arch` SSE4.2/ARM intrinsics behind `cfg`, no
crates), varints and all on-disk and wire framing, bloom filters, hashing for caches (a small
xxhash/FNV-style function), the sharded LRU block cache, the memcomparable codec, the seeded RNG for
the simulator (xorshift/PCG), the Raft implementation, the RPC framing, the Postgres wire protocol,
and **the memtable's arena skiplist** ([ADR 0041](docs/adr/0041-the-in-house-arena-skiplist.md)) —
single-writer, append-only, `u32` offsets into chunks that never move, and the last thing on this
list to stop being bought.

Runtime allowlist (each already justified in `docs/adr/0003-dependencies.md`; add to it only via ADR):
`tokio` (network runtime only), `bytes`, `thiserror`, `tracing` + `tracing-subscriber`, `lz4_flex`
(pure-Rust LZ4, the only compression), `jiff-tzdb` (the IANA zone table as bytes and nothing else —
the TZif reader is ours; ADR 0082). The list once ended with `crossbeam-skiplist` — "the one
piece of concurrent unsafe code we buy rather than write" — and ADR 0041 replaced it, so
**every piece of concurrent code in the engine is now code in this repository with a test in this
repository**. `tokio` stays the bought concurrency, and only at the network edge. Dev-only
allowlist: `proptest`, `criterion`, `stateright`, `tempfile`, `sqllogictest` (phase 6).
Explicitly **not** used: `tonic`/`prost` (RPC is hand-rolled framing over TCP, see DESIGN.md §9),
`serde` (no on-disk or wire use), `zstd`/`snap`, `crc32c`/`crc32fast`, `rand`, `lru`, `pgwire`.
Deferred decisions, ADR when reached: `sqlparser` (phase 6a — writing a full PostgreSQL parser is out
of scope, this is the one large exception we expect to accept) and the S3 client + TLS for phase 6b (the
pure-Rust TLS stack is the hard case; options are recorded in DESIGN.md §13).

## Toolchain and conventions

- Rust stable, edition 2024, workspace with one crate per layer (see table). `just check` runs
  `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo deny check`,
  `cargo test` (use `cargo nextest` if installed), and `cargo doc --no-deps`.
- Async only at the network edge (`tokio`). The engine and Raft cores are synchronous and `std`-only.
- Errors: `thiserror` enums per crate; `anyhow` only in binaries and tests. Logging via `tracing`.
- Bytes: `bytes::Bytes` at API boundaries; `&[u8]` internally. Serialization on disk and on the wire:
  hand-written little-endian framing documented in DESIGN.md, never serde.
- Property tests: `proptest`. Model checking: `stateright`. Benchmarks: `criterion` + `esker-cli bench`.
- Forbidden outright: `rocksdb`, `sled`, `openraft`, `raft-rs`, and anything on the ban list above. The
  whole point is to build these parts ourselves.

## Ask before doing

Stop and ask the human when you are about to: change an on-disk or wire format that already has a golden
test; add **any** dependency not on the allowlist; weaken any invariant above; delete or skip a failing
test; or spend more than a day on something not in the current phase's plan.

## Glossary

**CF** column family · **region** a contiguous key range replicated by one Raft group · **peer** one
replica of a region on one store · **epoch** `(conf_ver, version)` of a region; bumps on membership change
/ split · **TSO** timestamp oracle in PD · **seqno** engine-internal sequence number (snapshots) ·
**ts** MVCC timestamp from TSO, encoded into keys by `esker-txn` · **safepoint** the ts below which old
MVCC versions may be garbage-collected.
