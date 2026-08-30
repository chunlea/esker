# Phase 3 — coverage

Recorded by the `p3-accept` lane as part of the phase-3 acceptance battery
(`prompts/03-raft.md` Acceptance, `docs/plans/phase-3.md`). Not a gate on its own — the gate is
the test suite passing — but a number future phases can regress against.

## Run 1 — 2026-08-30

| Field | Value |
|---|---|
| commit | `91de89a` |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| tool | `cargo-llvm-cov 0.9.0` |
| command | `cargo llvm-cov -p esker-raft -p esker-sim -p esker-store` |
| scope | default (non-`--ignored`) test set only — the four thousands-of-seeds sweeps
(`thousands_of_membership_seeds`, `thousands_of_five_node_seeds`, `ten_thousand_seeds_with_faults`,
`every_reachable_state_with_messages_racing_is_safe`) are excluded from this measurement; they
are exercised separately (see the phase-3 acceptance report) and would not change which lines
run, only how many seeds do |

### Per-crate

| Crate | Regions | Region cover | Lines | Line cover | Functions | Function cover |
|---|---:|---:|---:|---:|---:|---:|
| `esker-raft` | 5,926 | 95.81% | 3,977 | 96.71% | 383 | 98.17% |
| `esker-sim` | 3,960 | 89.37% | 2,634 | 88.99% | 307 | 86.01% |
| `esker-store` | 4,956 | 82.97% | 3,207 | 82.38% | 370 | 73.51% |
| **TOTAL** (incl. all three) | 14,842 | 89.81% | 9,818 | 89.96% | 1,060 | 86.04% |

All three crates clear 80% line coverage, so the "five least-covered files" list below is
supplementary rather than required by the acceptance checklist — the same outcome as phase-1
and phase-2. `esker-store` is the closest to the floor (82.38%), which is expected: it is the
newest and largest of the three (3e landed the whole replicated-region code path this phase),
against `esker-raft`, whose 96.71% reflects two build lanes' worth of rule-by-rule tests plus
the property tests before `esker-store` ever touched it.

### Five least-covered files (by line %, across all three crates)

| File | Line cover | Missed / total lines |
|---|---:|---:|
| `esker-raft/src/error.rs` | 0.00% | 3 / 3 |
| `esker-sim/src/raft/checkers/observation.rs` | 29.17% | 17 / 24 |
| `esker-sim/src/raft/report.rs` | 35.56% | 87 / 135 |
| `esker-store/src/rawkv.rs` | 59.57% | 169 / 418 |
| `esker-store/src/region.rs` | 70.38% | 77 / 260 |

`esker-raft/src/error.rs` is a 3-line file by the coverage tool's count (it is 89 lines on disk;
the rest is doc comments and a `thiserror` enum with no branching body) — a size artifact, not a
coverage gap worth chasing, the same shape phase-2 found in `esker-client/src/transport.rs`.

`esker-sim/src/raft/report.rs` (the compact failure trace: "the seed, the event it failed at,
what the violation was, and the last hundred events", per its own doc comment) and
`checkers/observation.rs`'s less-common violation constructors are the *failure-reporting* code
— exercised only when a sweep actually finds a violation, which a green acceptance run by
definition does not do. Low coverage here is closer to a compliment to the rest of the suite
than a gap: the alternative would be tests that manufacture a violation just to watch it get
reported, which the checkers' own dedicated fault-injection tests (`raft_checkers.rs`,
`raft_persist_order.rs`) already do a version of for the checkers themselves, not for this
formatting layer.

`esker-store/src/rawkv.rs` and `region.rs` are the two genuine gaps worth a future look, not
artifacts. `rawkv.rs`'s own test file (`rawkv::tests::*`) covers its namespace and bounds
helpers; the eight `RawKv` method bodies it also holds get their coverage indirectly, through
`apply.rs`'s command tests and `server.rs`'s/`cluster.rs`'s integration tests, which do not
appear to reach every branch (particularly less-common limitation and error paths — this run did
not generate a line-level HTML report, so which branches specifically is not pinned down here).
`region.rs` implements `CLAUDE.md` invariant 5's epoch and range checks; its own doc comment
already says why it is exercised lightly today ("there is exactly one region, it covers the
whole key space, and its epoch never moves — so every check here always passes in normal use...
When phase 4 makes splits real, this is the code that has to already be right"), which is a
reason to keep the checks rather than a reason they are fully covered yet.

### How to reproduce

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --locked
cargo llvm-cov -p esker-raft -p esker-sim -p esker-store
```
