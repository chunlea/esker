# Phase 0 plan — scaffold the workspace

Status: **complete** (written before implementation; §8 records what changed).
Spec: `prompts/00-scaffold.md`. Constitution: `CLAUDE.md`. Design: `docs/DESIGN.md`.

## 1. Scope

Produce the skeleton every later phase builds on: a Cargo workspace, the lint/dependency guard
rails, the in-house primitives that are forbidden to come from crates.io, the memcomparable key
codec, a deterministic simulator skeleton, and a CLI shell. No storage, consensus or network
logic beyond those skeletons.

## 2. Crate layout

Eleven crates. Ten come from the prompt; `esker-base` is the "tiny crate if cleaner" the prompt
explicitly allows, and it is cleaner here for one reason: `crc32c`, varints, `hash64` and the
seeded RNG are needed by `esker-engine`, `esker-proto` and `esker-sim` alike. Putting them in
`esker-keys` would force `esker-engine` to depend on `esker-keys`, and invariant 7 says the
engine must not depend on key semantics. `esker-base` carries no key semantics, so the layering
stays honest.

| Crate | Phase 0 content |
|---|---|
| `esker-base` | `crc32c`, `varint`, `hash64`, `Pcg32` — the in-house primitives |
| `esker-keys` | memcomparable codec, reserved prefixes, `enc_ts`/`dec_ts` |
| `esker-engine` | doc comment + invariants + `crc32c` re-export (keeps DESIGN.md §4.5 true) |
| `esker-raft` | doc comment + invariants (pure state machine, no I/O) |
| `esker-store` | doc comment + invariants |
| `esker-pd` | doc comment + invariants |
| `esker-txn` | doc comment + invariants |
| `esker-proto` | doc comment + invariants + `WIRE_VERSION` |
| `esker-client` | doc comment + invariants |
| `esker-sim` | `Clock`, injected `Pcg32`, `Network` trait, `SimNetwork`, `FaultPlan` |
| `esker-cli` | hand-written arg parsing, `bench` stub, `tests/dep_budget.rs` |

Skeleton crates are not empty: each states its responsibility and the invariants from `CLAUDE.md`
that bind it, and each has at least one real test so `cargo test` is meaningful from day one.

## 3. File list

```
rust-toolchain.toml            channel = "stable" (this machine defaults to nightly)
Cargo.toml                     workspace.package / .dependencies / .lints
deny.toml                      bans + the dependency budget marker
justfile                       check / test / bench / sim / fmt / clippy / deny / doc
.github/workflows/ci.yml       just check on ubuntu + macos
README.md                      short overview, pointers to CLAUDE.md and DESIGN.md
docs/adr/0001-architecture.md
docs/adr/0002-formats-are-hand-rolled.md
docs/adr/0003-dependencies.md
docs/adr/0004-esker-base.md    why an eleventh crate exists
docs/bench/README.md           how benchmark numbers get recorded
docs/plans/phase-0.md          this file
crates/esker-base/src/{lib,crc32c,varint,hash,rng}.rs
crates/esker-keys/src/{lib,codec,prefix}.rs
crates/esker-keys/tests/{golden.rs,proptest_codec.rs}
crates/esker-keys/tests/golden/keys.txt
crates/esker-sim/src/{lib,clock,net,fault}.rs
crates/esker-sim/tests/determinism.rs
crates/esker-cli/src/{main,args}.rs
crates/esker-cli/tests/dep_budget.rs
crates/esker-{engine,raft,store,pd,txn,proto,client}/src/lib.rs
```

## 4. Public API sketch

```rust
// esker-base
mod crc32c { pub fn checksum(&[u8]) -> u32; pub fn update(u32, &[u8]) -> u32; }
mod varint { pub fn put_u64(u64, &mut Vec<u8>); pub fn get_u64(&[u8]) -> Result<(u64, usize)>;
             pub fn zigzag_encode(i64) -> u64; pub fn zigzag_decode(u64) -> i64;
             pub fn encoded_len_u64(u64) -> usize; /* u32 variants */ }
mod hash   { pub fn fnv1a64(&[u8]) -> u64; pub fn hash64(&[u8]) -> u64;
             pub fn hash64_with_seed(u64, &[u8]) -> u64; }
mod rng    { pub struct Pcg32; new(seed, seq) / from_seed(seed) / next_u32 / next_u64 /
             below(u32) / range(u64, u64) / chance(f64) / fill_bytes / shuffle }

// esker-keys
pub fn encode_u64(u64, &mut Vec<u8>);  pub fn decode_u64(&[u8]) -> Result<(u64, &[u8])>;
pub fn encode_i64(i64, &mut Vec<u8>);  pub fn decode_i64(&[u8]) -> Result<(i64, &[u8])>;
pub fn encode_bytes(&[u8], &mut Vec<u8>); pub fn decode_bytes(&[u8]) -> Result<(Vec<u8>, &[u8])>;
pub enum Value { U64, I64, Bytes }  pub enum ValueKind { .. }
pub fn encode_tuple(&[Value], &mut Vec<u8>);
pub fn decode_tuple(&[ValueKind], &[u8]) -> Result<(Vec<Value>, &[u8])>;
pub fn enc_ts(u64) -> [u8; 8];  pub fn dec_ts(&[u8]) -> Result<u64>;
pub mod prefix { RAW, TXN, SQL, META, SQL_ROW, SQL_INDEX + small key builders }

// esker-sim
pub struct Millis(u64);  pub struct Clock;      // logical time, monotonic
pub trait Network { fn send(&mut self, NodeId, Bytes) -> Result<()>; fn try_recv(&mut self) -> Option<Envelope>; }
pub struct FaultPlan { drop, duplicate, reorder, min_latency_ms, max_latency_ms, reorder_extra_ms }
pub struct SimNetwork { new(seed, plan) / node(id) / send / step / run_until / trace }
pub enum TraceEvent { Sent, Dropped, Duplicated, Delivered }
```

## 5. Test list

| Area | Tests |
|---|---|
| `crc32c` | golden `crc32c("123456789") == 0xE306_9283` + 5 more published vectors; hardware path equals software path on random input; `update` chaining equals one-shot; proptest |
| `varint` | golden byte vectors; round-trip proptest; boundary lengths (1/2/…/10 bytes); truncated and overlong input return errors, never panic; zigzag round-trip |
| `hash64` | published FNV-1a vectors; determinism; avalanche smoke test |
| `Pcg32` | the reference PCG stream for `(seed 42, seq 54)`; `below()` never returns `>= bound`; same seed ⇒ same stream |
| `esker-keys` | golden file `tests/golden/keys.txt`; proptest round-trip (≥1000 cases); proptest **order preservation** per type; explicit `len % 8 == 0` prefix-free cases; explicit `enc_ts` direction test (`a < b ⇒ enc_ts(a) > enc_ts(b)`); malformed input returns errors |
| `esker-sim` | same seed ⇒ identical trace twice; different seed ⇒ different trace (guards against a vacuous test); each fault kind is observable in a trace |
| `esker-cli` | arg-parser unit tests; `dep_budget` (runtime crate count ≤ budget in `deny.toml`, and no crate matches a ban pattern) |
| every other crate | one real test (invariant constants / doc-level assertions) so `cargo test` covers the whole workspace |

## 6. Risks and how they are handled

1. **Nightly default toolchain.** Handled first: `rust-toolchain.toml` pins stable, verified with
   `rustup show active-toolchain` inside the repo.
2. **Workspace lints that do not apply.** `lints.workspace = true` must be in *every* crate;
   otherwise clippy passes while the constitution is violated. Checked by grepping all manifests.
3. **Memcomparable prefix-freeness.** An input of length `k*8` still gets a trailing padded group.
   Property tests must include `len % 8 == 0` inputs explicitly, not just random ones.
4. **`enc_ts` direction.** Round-trip tests pass with the order inverted, so there is a separate
   explicit ordering test.
5. **Self-consistent crypto/PRNG.** `crc32c` and `Pcg32` are checked against *published* vectors;
   proptests alone would happily bless a wrong algorithm.
6. **Simulator nondeterminism.** No `HashMap` in the event loop, no `Instant`, no OS entropy; the
   queue is a `BTreeMap` keyed by `(deliver_at, seq)` so ties have a total order.
7. **A vacuous `cargo deny`.** Verified once by temporarily adding a banned crate and confirming
   `cargo deny check` fails, then reverting.
8. **Dependency counting across platforms.** `cargo metadata` without `--filter-platform` counts the
   Windows dependency tree too; the budget test filters to the host triple, and `deny.toml` lists
   the two targets we actually build.

## 7. Non-goals for this phase

- No WAL, memtable, SST, manifest or compaction code (phase 1).
- No Raft state machine, no RPC framing, no server (phases 2–3).
- No linearizability/bank checker in `esker-sim` (phase 3+), no link partitions yet.
- No benchmarks with real numbers; `esker-cli bench` prints "not implemented".
- No `sqlparser`, no S3/TLS decisions (phase 6).
- No performance tuning of any kind; correctness and guard rails only.

## 8. Changes vs plan

The plan held. Seven things came out differently, all of them discovered by building it.

1. **`unsafe_code = "warn"` in `esker-engine` could not be done in the manifest.** Cargo
   rejects a `[lints]` table that both inherits the workspace and overrides it
   ("cannot override `workspace.lints` in `lints`"). Since `lints.workspace = true` has to be
   in *every* manifest — without it a crate is silently unlinted — the relaxation is a
   crate-root `#![warn(unsafe_code)]` in `esker-engine`, which takes precedence over the
   workspace `deny`. Verified both directions: the same `unsafe` block warns in the engine and
   fails the build in `esker-keys`. Note that under `just check` (`-D warnings`) a warning is
   still an error, so an engine `unsafe` site needs a targeted `#[allow]` plus a `// SAFETY:`
   comment either way — which is what invariant 8 requires.

2. **`clippy::assertions_on_constants` had to be allowed.** Several skeleton tests deliberately
   pin relationships between compile-time constants ("the election timeout must dominate the
   heartbeat interval", "the inline-value cutoff must fit in one length byte"). Those are
   assertions on constants on purpose. Allowed at the workspace level with that reason written
   next to it, alongside `module_name_repetitions`, `missing_errors_doc` and
   `must_use_candidate`.

3. **`clippy::unwrap_used` and `expect_used` were added**, which the plan did not mention.
   `CLAUDE.md` invariant 9 is a rule about panicking on user input and on-disk data; making it
   a lint means each exception needs a comment rather than a habit. Test code opts out at the
   file or crate level.

4. **The dependency budget test needed a JSON parser.** `cargo metadata` emits JSON and `serde`
   is banned, so `crates/esker-cli/tests/dep_budget.rs` contains a small test-only reader.
   Two refinements the plan had not anticipated: the graph is filtered to the host triple
   (otherwise the Windows tree is counted, a dozen crates for a target nothing builds), and
   build edges are checked for banned *names* but do not count against the budget — a build
   script is how a C compiler gets into a "pure Rust" graph.

5. **`cargo-deny` cannot express glob patterns**, so `*-sys` and `openssl*` live only in the
   budget test, and `deny.toml` lists the concrete crates. The two mechanisms are complementary
   rather than redundant, and ADR 0003 says so.

6. **A fourth ADR** — `0004-esker-base.md` — records why there is an eleventh crate, since a
   future reader would otherwise reasonably try to fold it into `esker-keys` and break
   invariant 7 doing it.

7. **`cargo doc` needed `RUSTDOCFLAGS="-D warnings"`** to be worth running. Four intra-doc
   links were broken on the first full gate — links into a crate that is not a dependency, and
   one to a `#[cfg(test)]` module. A broken link to an invariant is a broken reference to the
   rule it states, so the `doc` recipe denies warnings.

8. **Two real bugs were caught by tests written from this plan**, both in code that looked
   obviously correct: `Pcg32::range_inclusive(0, u64::MAX)` overflowed computing `span + 1`,
   and `esker_client::backoff_ms` used `checked_shl`, which guards the shift width but not the
   value, so a large attempt count shifted every bit out and returned a *zero* delay instead of
   the ceiling. Both are now regression tests.

### Numbers at the end of the phase

| | |
|---|---|
| Crates | 11 |
| Tests | 115, all passing (`esker-keys` 32, `esker-base` 28, `esker-sim` 16, `esker-cli` 16, seven skeletons 3–4 each) |
| Property test cases | 1,000 per property, 15 properties, count checked by a test rather than asserted |
| Runtime transitive crates | 7 of a 40 budget |
| Rust source | 4,563 lines including tests |
| ADRs | 4 |

### For the coordinator

* `docs/DESIGN.md` §4.5 names the checksum `esker-engine::crc32c`. It lives in `esker-base`
  (ADR 0004) and `esker-engine` re-exports it, so the path in the design document resolves and
  a test asserts it. **No DESIGN.md edit is needed**, but if the layering table in `CLAUDE.md`
  is ever regenerated it should gain an `esker-base` row.
* `deny.toml` carries the dependency budget in a marked comment (`# esker-dep-budget = 40`)
  because cargo-deny rejects unknown configuration keys. Both the config and the test read that
  one line.
