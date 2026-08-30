# Phase 2 — coverage

Recorded by the `p2-accept` lane as part of the phase-2 acceptance battery
(`prompts/02-single-node-server.md` Acceptance, `docs/plans/phase-2.md`). Not a gate on its own —
the gate is the test suite passing — but a number future phases can regress against.

## Run 1 — 2026-08-30

| Field | Value |
|---|---|
| commit | `c5703a6` |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| tool | `cargo-llvm-cov 0.9.0` |
| command | `cargo llvm-cov -p esker-proto -p esker-store -p esker-client --summary-only` |
| scope | default (non-`--ignored`) test set only — the 60-second, 64-client load test (`the_full_load_test`) and the 200-round crash loop (`the_long_crash_loop`) are excluded from this measurement (they are exercised separately, see the acceptance report) |

### Per-crate

| Crate | Regions | Region cover | Lines | Line cover |
|---|---:|---:|---:|---:|
| `esker-client` | 2,000 | 92.95% | 1,327 | 92.46% |
| `esker-proto` | 3,929 | 94.27% | 2,458 | 92.31% |
| `esker-store` | 1,573 | 92.24% | 956 | 92.15% |
| **TOTAL** (incl. all three) | 7,502 | 93.50% | 4,741 | 92.32% |

All three crates clear 80% line coverage, so the "five least-covered files" list below is
supplementary rather than required by the acceptance checklist — the same outcome as phase-1.

### Five least-covered files (by line %, across all three crates)

| File | Line cover | Missed / total lines |
|---|---:|---:|
| `esker-client/src/transport.rs` | 0.00% | 3 / 3 |
| `esker-client/src/raw.rs` | 78.49% | 54 / 251 |
| `esker-proto/src/transport/client.rs` | 79.17% | 65 / 312 |
| `esker-client/src/wire.rs` | 84.62% | 12 / 78 |
| `esker-proto/src/transport/server.rs` | 85.25% | 36 / 244 |

`esker-client/src/transport.rs` is a 3-line file — its 0% is a size artifact, not a coverage gap
worth chasing. `raw.rs` and `transport/client.rs` are the two files worth a look first if a future
coverage pass targets phase-2: both sit closest to the 80% floor and both are on the client's hot
path (the CLI-facing retry/dispatch surface and the TCP client's frame handling, respectively).

### How to reproduce

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked
cargo llvm-cov -p esker-proto -p esker-store -p esker-client --summary-only
```
