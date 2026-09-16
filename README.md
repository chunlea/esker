# Esker

A distributed, horizontally scalable, transactional key-value store written from scratch in
Rust: a log-structured storage engine, a home-grown Multi-Raft replication layer, range
sharding with a placement driver, and Percolator-style distributed transactions. Long term it
is the storage foundation of a serverless database that speaks PostgreSQL syntax.

An esker is a long, stratified ridge of sediment laid down in order by a stream — layers
deposited by a log, which is exactly what this system is.

## Start here

* **[`CLAUDE.md`](CLAUDE.md)** — the constitution: what the system must be good at, the
  non-negotiable invariants, how to work, and the dependency policy. Read it first.
* **[`docs/DESIGN.md`](docs/DESIGN.md)** — the design: key space, engine formats, consensus,
  store, placement driver, transactions, wire protocol, testing strategy.
* **[`prompts/`](prompts)** — the phases, in order. Each is a gate.
* **[`docs/adr/`](docs/adr)** — decisions a future reader might otherwise reverse.
* **[`docs/plans/`](docs/plans)** — the plan for each phase, written before the code.

Where the two disagree, `CLAUDE.md` wins over `docs/DESIGN.md`, which wins over `prompts/`,
which wins over existing code. If code and `docs/DESIGN.md` disagree, one of them is fixed in
the same change.

## Layout

| Crate | Responsibility |
|---|---|
| `esker-base` | CRC32C, varints, `hash64`, seeded PCG32 — the primitives we do not buy |
| `esker-keys` | memcomparable codec, reserved key-space layout, MVCC timestamps |
| `esker-engine` | WAL, memtable, SST, manifest, compaction, column families |
| `esker-raft` | Raft as a pure, deterministic state machine |
| `esker-store` | one process, many regions; apply loop, splits, snapshots |
| `esker-pd` | membership, routing table, timestamp oracle, scheduling |
| `esker-txn` | Percolator two-phase commit over the `lock`/`write`/`default` families |
| `esker-proto` | hand-rolled framed RPC over TCP |
| `esker-client` | region cache, routing, retries, transaction API |
| `esker-sim` | deterministic simulator, fault injection, checkers |
| `esker-cli` | bootstrap, inspection, benchmarks |

## Building

The repository pins the **stable** toolchain in `rust-toolchain.toml`; `rustup` installs it on
first use. `just`, `cargo-deny` and `cargo-nextest` are needed for the full gate.

```sh
just check    # fmt, clippy (warnings denied), cargo deny, tests, docs — what CI runs
just test     # tests only
just sim      # the deterministic simulator
just bench    # the benchmark driver (see docs/bench/README.md)
```

`just check` is the gate. It is exactly what CI runs on Linux and macOS, so that a green check
locally means something.

## Two rules worth knowing before reading the code

**It is 100% Rust.** Nothing in the dependency graph may compile C, C++ or assembly, including
transitively. Checksums, varints, framing, bloom filters, the block cache, the codec, the
random number generator, Raft and the PostgreSQL wire protocol are written here, on purpose.
`cargo deny` and a budget test enforce the graph; `docs/adr/0003-dependencies.md` lists every
crate that is allowed and why.

**Formats are hand-written and pinned.** No `serde` on disk or on the wire. Every layout is
documented in `docs/DESIGN.md` and frozen by golden test vectors, so a format change is a
visible act rather than a side effect of reordering two struct fields
(`docs/adr/0002-formats-are-hand-rolled.md`).

## Status

**`v1.1.1`** (2026-09-16; the tag's base is `0fc6fb18` and the behaviour it ships is batch #195's)
is the current release: the ActiveRecord suite
answers **10,118 of 10,134 = 99.84%** against a **real topology** — one placement driver, four
stores, one `esker-sql` node — over a 10.49 h pass with no stopped file, no wedge and no node
restart. What it carries, how to deploy it (**every store is upgraded before any SQL node**) and
what it does not claim are in [`docs/releases/v1.1.1.md`](docs/releases/v1.1.1.md); the numbers
behind it are [`docs/acceptance/v1.1.md`](docs/acceptance/v1.1.md) §11, and the open rows are
[`docs/plans/debts-v1.1.md`](docs/plans/debts-v1.1.md) §1.
