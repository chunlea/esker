# Phase 4 — coverage

Recorded by the `p4-accept` lane as part of the phase-4 acceptance battery
(`prompts/04-multiraft-pd.md` Acceptance, `docs/plans/phase-4.md`). Not a gate on its own — the
gate is the test suite passing — but a number future phases can regress against.

## Run 1 — 2026-08-30

| Field | Value |
|---|---|
| commit | `68ce62c7` (esker-store/esker-pd's own last commit; esker-sql/esker-txn continued committing after this without touching either crate) |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| tool | `cargo-llvm-cov 0.9.0` |
| command | `cargo llvm-cov -p esker-store -p esker-pd --no-fail-fast` |
| scope | default (non-`--ignored`) test set only — the two `esker-pd::balance` thousand-round sweeps and `esker-pd::crash_kill`'s `the_kill_loop` (hundreds of process spawns) are excluded; exercised deliberately elsewhere, not in a coverage pass, and would not change which lines run, only how many rounds do |
| caveat | the run printed `warning: 137 functions have mismatched data` — `cargo-llvm-cov`'s own known artifact of merging profile data across this many separate test binaries, not a failure. A second run at the same commit reproduced the same warning but different absolute counts, so treat the percentages below as accurate to a point or two, not to the region |

### Per-crate

| Crate | Regions | Region cover | Lines | Line cover | Functions | Function cover |
|---|---:|---:|---:|---:|---:|---:|
| `esker-pd` | 4,402 | 94.07% | 2,940 | 94.49% | 325 | 90.77% |
| `esker-store` | 11,821 | 85.65% | 7,711 | 84.92% | 852 | 76.53% |
| **TOTAL** (both) | 16,223 | 87.94% | 10,651 | 87.56% | 1,177 | 80.46% |

Both crates clear 80% line coverage. `esker-store` sits closer to the floor, which is expected: it
carries the whole region/split/snapshot/balance surface 4a–4d landed, against `esker-pd`'s
narrower and more heavily unit-tested routing/scheduling core.

### Five least-covered files (by line %)

| File | Line cover | Missed / total lines |
|---|---:|---:|
| `esker-store/src/rawkv.rs` | 59.57% | 169 / 418 |
| `esker-store/src/error.rs` | 68.87% | 33 / 106 |
| `esker-store/src/server.rs` | 72.86% | 431 / 1,588 |
| `esker-store/src/region.rs` | 75.44% | 83 / 338 |
| `esker-pd/src/routing.rs` | 77.46% | 32 / 142 |

**One file is under the 60% line-coverage floor this report is asked to flag:
`esker-store/src/rawkv.rs`, at 59.57%.** It is unchanged to the byte from phase 3's measurement
(`docs/bench/phase-3-coverage.md`: 59.57%, 169/418 missed) — phase 4 added regions and routing
*around* `RawKv`, not new behavior inside it, so the paths phase 3 left uncovered (uncommon
limitation and error branches in the eight method bodies) are the same paths phase 4 leaves
uncovered. Its own test module covers namespacing and bounds helpers directly; the method bodies
are exercised indirectly through `apply.rs`'s command tests and the `server`/`cluster` integration
suites, which do not reach every branch.

The other four are all above 60% but worth recording:

- **`esker-store/src/error.rs`** (68.87%) is mostly a `thiserror` conversion surface — one-line
  `From` mappings for engine and proto error variants a normal run never provokes, the same shape
  earlier phases found in other crates' `error.rs` files.
- **`esker-store/src/server.rs`** (72.86%) is the largest file in either crate (1,588 lines) and
  the store's whole request-dispatch surface; at this size a handful of uncommon limitation and
  error branches account for the gap rather than any one path being untested.
- **`esker-store/src/region.rs`** (75.44%) is invariant 5's epoch and range-check code — up from
  phase 3's 70.38%, consistent with 4b–4d actually exercising the epoch-mismatch-after-split paths
  that were dead code while there was ever only one region. Phase 3's own note on this file said
  "when phase 4 makes splits real, this is the code that has to already be right"; the coverage
  number moving is one piece of evidence that it now gets exercised as such.
- **`esker-pd/src/routing.rs`** (77.46%) is the `GetRegion` range-index seek; ordinary lookups are
  well covered and the gap concentrates in less-common boundary and corruption branches.

### How to reproduce

```sh
cargo llvm-cov -p esker-store -p esker-pd --no-fail-fast
```
