# Phase 0 — Scaffold the workspace

You are starting the Esker project. Read `CLAUDE.md` and `docs/DESIGN.md` first; they are the constitution
and the design. This phase produces no storage logic — it produces the skeleton that every later phase
builds on, so precision matters more than speed.

## Deliverables

1. Cargo workspace with these crates (all compiling, each with a `lib.rs` doc comment stating its
   responsibility in two sentences and its **invariants** list copied from CLAUDE.md where relevant):
   `esker-keys`, `esker-engine`, `esker-raft`, `esker-store`, `esker-pd`, `esker-txn`, `esker-proto`,
   `esker-client`, `esker-sim`, `esker-cli`. Shared settings in `[workspace.package]` and
   `[workspace.dependencies]`; `[workspace.lints]` with `clippy::pedantic` allowed selectively, and
   `unsafe_code = "deny"` in every crate except `esker-engine` (where it is `warn`).
2. `justfile` with `check` (fmt --check, clippy -D warnings, `cargo deny check`, test, doc), `test`,
   `bench`, `sim` targets. CI workflow (`.github/workflows/ci.yml`) running `just check` on Linux and
   macOS.
3. **Dependency guard** (the pure-Rust rule from CLAUDE.md): `deny.toml` banning `*-sys`, `cc`,
   `openssl*`, `ring`, `aws-lc-*`, `libz*`, `zstd*`, plus a test in `esker-cli` (`tests/dep_budget.rs`)
   that runs `cargo metadata`, counts transitive non-dev crates, and fails above the budget (40).
   `docs/adr/0003-dependencies.md` listing every allowed crate with a one-line justification and what it
   would take to replace it.
4. In-house primitives in `esker-keys` (or a tiny `esker-base` if cleaner): `crc32c` (slicing-by-8,
   hardware instruction behind `cfg`), `varint` (LEB128 u32/u64, zigzag i64), a `hash64` for cache keys,
   and a seeded `Rng` (PCG32). Each with golden vectors and proptests. These are used by every later phase;
   no crate may be added for them.
5. `esker-keys`: the memcomparable codec from DESIGN.md §3 (u64/i64/bytes/tuple encode+decode), the
   reserved-prefix constants, `enc_ts`/`dec_ts`. Tests: proptest round-trip and **order preservation**
   (`a < b ⇔ enc(a) < enc(b)` for every type, including the prefix-free property for bytes), plus golden
   vectors in `tests/golden/keys.txt` so the encoding can never silently change.
6. `esker-sim` skeleton: `Clock`, the seeded `Rng` (injected), `Network` trait with an in-memory
   implementation that can drop/delay/duplicate/reorder by `FaultPlan`, and a test proving that the
   same seed yields the same event trace twice.
7. `esker-cli` skeleton with `bench` subcommand that currently prints "not implemented" and a
   `--version` (argument parsing is hand-written — no `clap`). `docs/bench/README.md` describing how
   numbers will be recorded.
8. `docs/adr/0001-architecture.md` (why LSM + multi-raft + range sharding; alternatives considered:
   object-storage-first / no consensus, embedding RocksDB, openraft) and `docs/adr/0002-formats-are-hand-rolled.md`
   (why no serde on disk or on the wire). `docs/plans/phase-0.md` written before you start, updated when
   you finish.

## Rules for this phase

- Do not write engine, raft, or network code beyond the skeletons above.
- Every crate gets at least one real test so `cargo test` is meaningful from day one.
- Dependencies are added only from the list in CLAUDE.md; anything else needs an ADR.

## Acceptance

`just check` is green on a clean clone including `cargo deny check`; `cargo tree -e normal` for the whole
workspace shows only allowlisted crates and the budget test passes; `cargo test -p esker-keys` runs the
proptests with at least 1,000 cases each; `cargo run -p esker-cli -- --version` prints the version; the
simulator determinism test passes; the three ADRs and the plan exist. Report the final crate tree, the
transitive dependency count, and line counts.
