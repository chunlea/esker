# Phase 5 — coverage

Recorded by the `p5-accept` lane as part of the phase-5 acceptance battery
(`prompts/05-txn.md` Acceptance, `docs/plans/phase-5.md`). Not a gate on its own — the gate is
the test suite passing — but a number future phases can regress against, in the shape
`docs/bench/phase-4-coverage.md` set.

## Run 1 — 2026-08-31

| Field | Value |
|---|---|
| commit | `9dd71b1` (`feat(store): the engine's filesystem is the caller's choice` — the phase-6b tier lane's own last commit; this pass ran with that lane's further work uncommitted in the tree, none of it inside phase 5's files) |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| tool | `cargo-llvm-cov 0.9.0` |
| command | `cargo llvm-cov -p esker-txn -p esker-client -p esker-store --no-fail-fast` |
| scope | `esker-txn` in full — the whole crate is phase 5's. Inside `esker-store` and `esker-client`, only the txn-owned files: `txnkv.rs`, `txn_command.rs`, `gc.rs`, and `txn.rs`. Both crates carry a large raw-KV/Raft/PD surface (routing, splits, snapshots, heartbeats) that phase 5 did not touch, so a whole-crate number would dilute the txn story rather than measure it; see "for reference" below for those wider numbers anyway. Default (non-`--ignored`) test set only, matching phase 3/4's precedent — the 1,000-seed bank sweep, the 1,000-version GC run and the 60-second acceptance run are exercised deliberately elsewhere (the acceptance report), not here |
| caveat 1 | `--no-fail-fast`, because `esker-client::txn::a_prewrite_that_meets_several_locks_clears_them_in_one_round` is a genuinely racy test — two lock-resolution groups race on separate threads (`fan_out`, `router.rs:359`) to record their call first, and the test asserts a specific order. It failed 3 of 6 fresh-process runs in isolation during this same pass; this particular coverage run happened to land on a passing interleaving. See the acceptance report for the full root cause |
| caveat 2 | the run printed `warning: 33 functions have mismatched data` — `cargo-llvm-cov`'s own known artifact of merging profile data across many separate test binaries (phase 4's report saw the same class of warning, at a different count). Treat the percentages below as accurate to a point or two, not to the region |

### `esker-txn`, in full

| File | Regions | Region cover | Lines | Line cover | Functions | Function cover |
|---|---:|---:|---:|---:|---:|---:|
| `codec.rs` | 474 | 96.41% | 275 | 96.73% | 37 | 94.59% |
| `error.rs` | 21 | 100.00% | 29 | 100.00% | 3 | 100.00% |
| `key.rs` | 277 | 98.19% | 140 | 97.86% | 22 | 95.45% |
| `lib.rs` | 54 | 100.00% | 34 | 100.00% | 7 | 100.00% |
| `mutation.rs` | 119 | 82.35% | 73 | 78.08% | 20 | 75.00% |
| `percolator.rs` | 446 | 92.83% | 314 | 93.63% | 29 | 89.66% |
| `snapshot.rs` | 171 | 79.53% | 101 | 85.15% | 22 | 77.27% |
| **subtotal** | **1,562** | **92.96%** | **966** | **93.48%** | **140** | **88.57%** |

`esker-txn` clears 93% line coverage as a crate. The two files under it:

- **`snapshot.rs`** (85.15% lines) is, by its own header comment, "an in-memory answer for
  tests" — a fake `TxnSnapshot` used across the whole protocol matrix, not production code on
  any request path. A fake's job is to answer every question the rules can ask, and some of
  those answers (uncommon combinations the matrix does not happen to construct) go untaken.
- **`mutation.rs`** (78.08% lines) is the smallest file in the crate — `Cf`, `Mutation`,
  `Mutations`, the `WriteBatch`-shaped output of a decision. At this size a handful of
  convenience methods (construction helpers, `Debug`/equality-adjacent code) account for the
  gap rather than any decision path being untested; the decisions themselves are
  `percolator.rs`'s, at 93.63%.

### The txn-owned files of `esker-store` and `esker-client`

| File | Regions | Region cover | Lines | Line cover | Functions | Function cover |
|---|---:|---:|---:|---:|---:|---:|
| `esker-store/src/txnkv.rs` | 582 | 85.57% | 351 | 90.03% | 40 | 70.00% |
| `esker-store/src/txn_command.rs` | 470 | 96.38% | 350 | 99.71% | 23 | 100.00% |
| `esker-store/src/gc.rs` | 450 | 82.44% | 266 | 83.83% | 39 | 71.79% |
| `esker-client/src/txn.rs` | 741 | 88.26% | 485 | 83.51% | 64 | 89.06% |
| **subtotal** | **2,243** | **88.10%** | **1,452** | **89.05%** | **166** | **81.93%** |

**Combined phase-5 scope (11 files): 3,805 regions at 90.09%, 2,418 lines at 90.82%, 306
functions at 84.97%.**

- **`esker-client/src/txn.rs`** (83.51% lines, the lowest function count covered at 89.06%) is
  the client's whole 2PC state machine — begin/get/scan/put/commit/rollback and lock
  resolution — at 1,017 lines (over `CLAUDE.md`'s ~800-line guideline; noted in the acceptance
  report's drift section, not a coverage finding). The uncovered lines concentrate in
  `resolve`'s less-common branches: `docs/plans/phase-5.md` §10.8 records that this exact
  function was rewritten once already after a close-out bug, and the four-step lease/primary
  logic it now runs has more branches than the retry-and-happy-path tests before it exercise.
- **`esker-store/src/gc.rs`** (71.79% functions) implements ADR 0021's retention formula and
  the compaction filter; the uncovered functions skew toward the table-override arithmetic
  (`RETENTION_FOREVER`, the `<<18` adjustment for a non-default retention) which the default
  (non-`--ignored`) test set exercises at the cluster-default path far more than at an
  overridden one.
- **`esker-store/src/txnkv.rs`** (70.00% functions, the lowest function-coverage number in
  scope) is the seven `TxnKv` handlers; §10.3's transient-condition table (`NotLeader`,
  `EpochNotMatch`, a live lock, a lost prewrite's idempotent replay, `Wait` under a lease) names
  more refusal shapes than the default test set constructs in one pass — the bank test and the
  crash-boundary suite construct several of them, but at `--ignored` scope, deliberately outside
  this run.

### Five least-covered files (by line %), in scope

| File | Line cover | Missed / total lines |
|---|---:|---:|
| `esker-txn/src/mutation.rs` | 78.08% | 16 / 73 |
| `esker-client/src/txn.rs` | 83.51% | 80 / 485 |
| `esker-store/src/gc.rs` | 83.83% | 43 / 266 |
| `esker-txn/src/snapshot.rs` | 85.15% | 15 / 101 |
| `esker-store/src/txnkv.rs` | 90.03% | 35 / 351 |

All five clear phase 3/4's 60%-line floor by a wide margin; none is flagged.

### For reference: the whole crates, non-txn code included

Measured in the same run (it is one invocation over both crates), not a phase-5 number:

| Crate scope | Regions | Region cover | Lines | Line cover | Functions | Function cover |
|---|---:|---:|---:|---:|---:|---:|
| all measured files (`esker-txn` + `esker-store` + `esker-client`, 38 files) | 18,275 | 90.12% | 11,778 | 90.12% | 1,390 | 83.96% |

The whole-crate number lands close to the txn-scoped one by coincidence of averaging, not
because the two measure the same thing — `esker-store/src/server.rs` (2,517 regions, the
largest file measured) and `esker-store/src/peer.rs` (1,757 regions) are almost entirely
non-txn dispatch and Raft-log bookkeeping, both already reported against in
`docs/bench/phase-4-coverage.md`.

### How to reproduce

```sh
cargo llvm-cov -p esker-txn -p esker-client -p esker-store --no-fail-fast
```

`--no-fail-fast` is required while `a_prewrite_that_meets_several_locks_clears_them_in_one_round`
remains racy (see the acceptance report); without it, a run that lands on the failing
interleaving aborts before most files are measured.
