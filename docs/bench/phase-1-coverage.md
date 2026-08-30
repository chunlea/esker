# Phase 1 — coverage

Recorded by the `p1-accept` lane as part of the phase-1 acceptance battery
(`prompts/01-engine.md` Acceptance, `docs/plans/phase-1.md`). Not a gate on its own — the gate
is the test suite passing — but a number future phases can regress against.

## Run 1 — 2026-08-30

| Field | Value |
|---|---|
| commit | `a34dbad` |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| tool | `cargo-llvm-cov 0.9.0` |
| command | `cargo llvm-cov -p esker-engine -p esker-keys -p esker-base --summary-only` |
| scope | default (non-`--ignored`) test set only — the 10,000-case model run, the 1,000-iteration SIGKILL loop and the 30 s concurrency run are excluded from this measurement (they are exercised separately, see the acceptance report) |

### Per-crate

| Crate | Regions | Region cover | Lines | Line cover |
|---|---:|---:|---:|---:|
| `esker-base` | 912 | 95.39% | 520 | 94.23% |
| `esker-engine` | 15,328 | 92.26% | 8,563 | 93.46% |
| `esker-keys` | 595 | 98.82% | 322 | 100.00% |
| **TOTAL** (incl. all three) | 16,835 | 92.66% | 9,405 | 93.73% |

All three crates clear 80% line coverage, so the "five least-covered files" list below is
supplementary rather than required by the acceptance checklist.

### Five least-covered files in `esker-engine` (by line %)

| File | Line cover | Missed / total lines |
|---|---:|---:|
| `src/db/ingest.rs` | 78.91% | 27 / 128 |
| `src/db/checkpoint.rs` | 79.80% | 20 / 99 |
| `src/db/table_cache.rs` | 80.33% | 12 / 61 |
| `src/sst/block.rs` | 81.33% | 70 / 375 |
| `src/db/iter.rs` | 81.63% | 45 / 245 |

Checkpoint/ingest and the table cache are the three CF/range-scoped and cache-eviction edge
cases with the thinnest direct test coverage in the crate; not a failure of the acceptance
gate, but the first place a future coverage pass would look.

### How to reproduce

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked
cargo llvm-cov -p esker-engine -p esker-keys -p esker-base --summary-only
```
