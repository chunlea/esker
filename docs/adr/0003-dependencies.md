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
| `crossbeam-skiplist` | The memtable: a concurrent lock-free skiplist. **This is the one piece of concurrent unsafe code we buy rather than write**, and it is deliberate — a subtly wrong lock-free skiplist would corrupt data in ways the test suite would find slowly and painfully. | An in-house arena skiplist behind the same `MemTable` trait. Planned as a post-v1 exercise, when there is a test suite good enough to trust the swap. |

### The TLS exception, behind a default-off feature

Added 2026-09-04 by the maintainer's ruling on
[ADR 0055](0055-the-tls-options-across-three-surfaces-measured.md), which measured the options
rather than arguing them. Both crates are **optional dependencies of `esker-sql` behind its `tls`
feature, which is off by default** — the default build links neither, and the graph the budget
measures does not move by one crate.

| Crate | Why it is here | What replacing it would take |
|---|---|---|
| `rustls` | TLS itself, `default-features = false, features = ["std"]`. The defaults are the whole problem and are switched off: `aws_lc_rs` pulls `aws-lc-sys` (C), and the `ring` feature is banned for the same reason. With neither, rustls has no crypto of its own and takes a `CryptoProvider` at runtime — which is the seam that makes a pure-Rust provider possible at all. | Writing TLS 1.3. ADR 0055 estimates 4,000-8,000 lines plus a review this project is not equipped to give itself: a bug in constant-time crypto keeps the tests green and leaks the key. |
| `rustls-graviola` | The `CryptoProvider`: pure Rust, no build script, no `links` key, `x86_64` and `aarch64` only — which is exactly `deny.toml`'s two targets. Chosen over `rustls-rustcrypto` **on measurement**: that crate fails this repo's own `cargo deny check` today on a banned `rand`, a stale duplicate `rustls-webpki` carrying four live RUSTSEC advisories, an unmaintained `paste`, and an unpatched RSA timing side-channel (RUSTSEC-2023-0071). graviola passes it clean. | The other pure-Rust provider, once one is fit to use; the swap is one `default_provider()` call in `pgwire::tls`. |

Its real cost, measured in this workspace rather than in isolation: **nine** crates, not the twelve
ADR 0055 measured standalone, because `once_cell`, `cfg-if` and `libc` are already here. With the
feature on the compiled graph goes 34 → 43 (`cargo tree -e normal`); with it off, 34, unchanged.

The nine: `rustls`, `rustls-graviola`, `graviola`, `rustls-pki-types`, `rustls-webpki`,
`getrandom`, `subtle`, `untrusted`, `zeroize`.

**The build-script audit `deny.toml` asks for, done.** That file says a build script "has to be
looked at by a human rather than discovered in a profile six months later", so: of the nine, two
have one and neither compiles anything. `rustls`'s is thirteen lines and sets one `cfg` for the
nightly-only `read_buf` feature, which is not enabled here. `getrandom`'s runs `rustc -vV` to read
the compiler's minor version and emits `cfg`s from it. **No `links` key on any of the nine**, and no
`cc` anywhere in the graph. Graviola, the one that actually contains assembly, has **no build script
at all**: its `Cargo.toml` says `build = false` and its x86-64 and aarch64 routines are `.rs` files
that rustc assembles inline — the same shape `CLAUDE.md` already blesses for `crc32c`'s
`std::arch` paths, and the reason a crate full of hand-written assembly is still pure Rust by this
project's definition.

**`Cargo.lock` now names `ring` and `cc`, and nothing builds them.** This is worth knowing before
someone greps for it and concludes the rule was broken. Cargo locks a version for every optional
dependency edge that could ever be selected, so `rustls-webpki`'s unused optional `ring` — and
`ring`'s own `cc` and `windows-*` — land in the lock the moment rustls is in a manifest. They are
not in the graph: `cargo tree --features esker-sql/tls --target all -e normal,build,dev -i ring`
prints nothing, `rustls-webpki` resolves with features `["alloc", "std"]`, and `cargo deny check`
— which builds its graph independently, through `krates` — says `bans ok`. The lock file is a
record of what was *resolved*, not of what is *compiled*, and the pure-Rust rule is about the
second.

**No root store is part of this exception.** `webpki-roots` verifies *someone else's* certificate,
which a server terminating TLS never does; it is owed by the S3 and RPC surfaces when they arrive,
and it costs a `CDLA-Permissive-2.0` line in `[licenses] allow` that this exception does not.

**If the feature is ever made default-on, the budget must be raised by ADR first** — and the two
defects in the budget test that `deny.toml`'s comment names have to be fixed before the number it
reports in that state means anything.

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
* **An S3 client and TLS** (phase 6b). The S3 client was written in-house and costs nothing
  (ADR 0025). TLS is **settled**: measured in ADR 0055 and accepted by the maintainer on
  2026-09-04 as the default-off exception above, PostgreSQL port first. The S3 and RPC surfaces
  still terminate outside the process until their own units land.

## Enforcement

Two mechanisms, because they fail differently.

1. **`deny.toml`**, checked by `cargo deny check` inside `just check` and CI. It names crates
   we already know about, denies duplicate versions and wildcard requirements, restricts
   licences, and excludes dev dependencies from the graph. Verified non-vacuous by temporarily
   adding `rand` to a crate and confirming the check failed.
2. **`crates/esker-cli/tests/dep_budget.rs`**, which walks the resolved graph from
   `cargo metadata` and checks names against the *patterns* — `*-sys`, `openssl*`, `aws-lc-*` —
   that a list of exact names cannot express. It also counts the graph.

**The budget is 40 transitive runtime crates**, recorded in a marked comment in `deny.toml` so
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
  `pin-project-lite` and the macro chain. It should stay well inside 40; if it does not, the
  first move is to trim `tokio`'s feature list, not to raise the budget.
