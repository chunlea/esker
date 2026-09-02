# Phase 11, lane c5: the simulator learns this week's mechanisms, then the engine's recorded debts

Lane `c5-engine`. Two halves that share nothing but a build directory:

* **U1** — the deterministic simulator grows a model of each of the four mechanisms this week's
  fixes pinned, so that the orderings nobody constructed get explored.
* **U2–U4** — the three `TODO(post-v1)` performance items in `esker-engine`, each with numbers,
  plus a measurement that decides whether the tiered-read block size gets a knob at all.
* **U5** — an ADR, design only, for the in-house skiplist that would remove `crossbeam-skiplist`.

Precedence, as always: `CLAUDE.md` > `docs/DESIGN.md` > this file > code.

## 0. The lane, by file path

ALLOWED: `crates/esker-sim/**`, `crates/esker-engine/**`, `crates/esker-base/**`,
`crates/esker-cli/**` (bench and tests only), new `sim_*.rs` files under
`crates/esker-{store,pd,client}/tests/`, this file, `docs/bench/phase-11-engine.md`, new ADRs,
and the paragraph of `docs/DESIGN.md` a unit changes.

FORBIDDEN: `crates/esker-{sql,keys,columnar}/**` and the `src/` of
`esker-{store,pd,client,proto,s3}` — `b3-scoreboard`'s live work — and `crates/esker-raft/**`,
which is a pure state machine this lane models and does not change (invariant 4).

Build directory: `CARGO_TARGET_DIR=/Users/chunlea/workspace/lab/esker/target-c5`, excluded in
`.git/info/exclude`, so that a second lane in the same checkout is not blocked on a cargo lock.

## 1. U0 — this file, and three documents that had gone stale

`c4-debts` closed with exact diffs for three documents outside its own file list. They are
applied in U0's commit, unchanged:

* `docs/plans/phase-4-pd.md` §12.3 — in-flight operators are no longer invisible; `Pd::Status`
  and `esker pd status` landed in C4 (e45e71f).
* `docs/plans/phase-4.md` §14.6 — `region ls` no longer walks one `GetRegion` at a time;
  `Pd::ScanRegions` landed in C4 (3ff1c1b).
* `docs/plans/debt-c1.md` §"What this lane did not do" — the simultaneous-claim race (05059a8)
  and `bench --adopt-sst-store` (f090674) are both closed.

## 2. U1 — the simulator learns this week's four mechanisms

### The argument

Each of the four fixes pinned its invariant with **one constructed ordering**. That ordering was
found by hand, and finding it is what took the day. A simulator's job is the rest of the space:
the orderings nobody thought to construct.

### The shape, and the seam that makes a red-first run possible

Each mechanism gets two pieces:

1. **A model and a checker in `esker-sim`** — deterministic, seeded, no wall clock, no sockets,
   no `std::thread::sleep`. The model is *generic over the decision under test*, expressed as a
   trait, because the model has to be able to ask two different revisions of the real code the
   same question.
2. **A binding in the real crate's `tests/sim_*.rs`** that implements that trait by calling the
   **real** code. This is the piece that makes "red against the pre-fix code" mean something: the
   model is copied into a detached worktree at the fix's parent, where the trait is implemented
   by the code as it was, and the checker has to fire.

`esker-sim` itself keeps a reference implementation of each trait so its own tests can prove the
model explores what it claims to explore. **A reference implementation is never the red-first
proof** — it is a transcription, and a transcription of a bug is decoration. Only the binding
counts.

### U1a — balance never touches a mid-repair region (548dd62, five-store seeded runs)

State space too large to enumerate: five stores, regions with three to five peers, stores going
down and coming back, repair adding a learner, the leader promoting it. Seeded fault-injected
runs, therefore.

* Model: `esker-sim::mech::placement` — a cluster of five stores and N regions; a seeded schedule
  of `StoreDown`, `StoreUp`, `RepairAdds(learner)`, `Promote`, `Tick`.
* Trait: `BalancePolicy { fn plan(&self, region, cluster) -> Option<BalanceOp> }`.
* Binding: `crates/esker-pd/tests/sim_balance.rs` → real `esker_pd::balance::balance_for`.
* Checker: **no operator is issued against a region that is mid-repair** — one holding a peer on
  a down store, or a plain `Learner`. A `ColumnarLearner` is not mid-repair (ADR 0022) and gets
  its own assertion that balance *does* still move such a region.
* Red at `ba8ed2e` (548dd62^): `is_mid_repair` counted down stores only, so a replica added on a
  live store and not yet promoted looked settled.

### U1b — the retry budget counts only no-progress attempts (9791e16, seeded runs)

* Model: `esker-sim::mech::retry` — a key whose region keeps splitting and moving, so every
  refusal teaches a strictly newer epoch; mixed with refusals that teach nothing (`NotLeader`
  around a region that is not changing) and stores that never answer.
* Trait: `RetryClient { fn call(&mut self, key) -> Outcome }`, driven with no wall clock.
* Binding: `crates/esker-client/tests/sim_retry.rs` → the real `esker_client::Router` over
  `esker_client::testing::{FakeTransport, FakeClock}`.
* Checker, both halves: **a client never gives up while progress is being made** (no
  `StoreUnavailable("gave up after N attempts")` while the epoch it learned is strictly newer
  than the one it asked with) **and never loops without it** (a store that teaches nothing ends
  in a bounded number of attempts, and an endless supply of newer epochs ends in
  `DeadlineExceeded` having spent the deadline, not the budget).
* Red at `1502d0f` (9791e16^): the first half fires — gives up with most of the deadline unspent.

### U1c — a removed peer is swept and its range reclaimed (92a5add, stateright)

Small state space: a store's hosted set, PD's record for the same range, and the three
conditions.

* Model: `esker-sim::mech::sweep` — a `stateright` model whose state is (hosted regions with
  epochs and ranges, PD's record per range, what has been reclaimed). Actions: PD advances a
  record, a split narrows a hosted parent, the store probes, the store applies a conf change.
* Trait: `SweepPolicy { fn retires(&self, hosted, pd_answer, store_id) -> bool }` plus
  `fn reclaims_on_operator_path(&self) -> bool`.
* Binding: `crates/esker-store/tests/sim_sweep.rs` — a real `Store` with a `FakePd`, driven
  through the orderings the model enumerates.
* Checker: **a removed peer's range is reclaimed exactly once, and never while any region this
  store still hosts overlaps it.** Both halves; the second is the one that catches a parent
  narrowed by a split retiring against the range it used to have.
* Red at `c31a8a8` (92a5add^): nothing runs on the operator path, so the range is reclaimed
  **zero** times.

### U1d — the snapshot ask waits for the record that names its peer (1502d0f, stateright)

* Model: `esker-sim::mech::ask` — states are (core membership, applied record membership,
  whether the change committed or was rolled back, elapsed wait). Actions: append, apply,
  roll back, ask, tick.
* Trait: `AskPolicy { fn answer(&self, core, record, peer, waited) -> Answer }` where `Answer` is
  `Serve | Refuse | Wait`.
* Binding: `crates/esker-store/tests/sim_snapshot_ask.rs` — the real store's snapshot path, using
  the `snapshot_refused` helper shape `tests/snapshot.rs` already has.
* Checker: **a snapshot is never served to a peer the applied record does not name**, and — the
  half that the recorded fix failed — **a peer the core names and the record will name is served
  within the bound rather than refused for ever.**
* Red at `c062f91` (1502d0f^): the second half fires; the ask reads only the record and refuses a
  member during the gap.

### Recorded

Seeds and run counts for every model go in §10 as they are produced, so a failure is a number to
rerun rather than a story.

## 3. U2 — the two-level iterator

`db/iter.rs:406`: `Db::iter` opens **one cursor per file, for every file, in every level**, so a
scan of a database with a full L4 opens every L4 file before it reads a byte. LevelDB's shape is a
two-level iterator: a level is one cursor over its file index, and it opens the file it has
reached. A level below L0 partitions the key space, so at most one of its files is open at a time.

* L0 keeps the current shape — its files overlap, so all of them can be live at once.
* New: `db/iter.rs` (or a sibling under 800 lines) gains `LevelCursor`, driving `TableCache` as it
  seeks. It is a `Cursor`, so `MergeCursor` is unchanged.
* Range tombstones are collected up front as they are now (ADR 0017 puts them beside the run, and
  the debug assertion that none appears below L0 stays), so the tombstone set does not depend on
  which file a cursor has reached.

Tests: the existing iterator property tests, plus **a new property test that a level holding N
files scans identically to the same keys merged from one file** — forward, backward, and with
seeks landing inside, between and outside file boundaries.

Numbers: `readseq`, `readrandom`, and a range scan across a level with many files, before and
after, in `docs/bench/phase-11-engine.md` with the exact command lines.

## 4. U3 — the table cache evicts by use, not by file number

`db/table_cache.rs:83`: the victim is `open.keys().next()` — the **lowest file number**, which is
the oldest file, which in a levelled engine is the one deepest in the tree and most likely to be
read again. A sharded LRU is already the block cache's shape (`cache/lru.rs`); the table cache
reuses it rather than growing a second one.

Tests: existing table-cache and `Db` tests, plus one that a working set larger than the capacity
does not evict the entry it is about to ask for again.

Numbers: hit rate and wall clock for a working set larger than the cache, before and after.

## 5. U4 — the tiered read block size, measured before it is a knob

`fs/tier.rs` reads a tiered SST in 4 KiB blocks. Against object storage that is one HTTP range
request per 4 KiB, and the round trip dominates. **Measure first**: cold `readrandom` and
`readseq` against `esker-minio` at 4, 16, 64 and 256 KiB.

The knob is added **only if the numbers say so**, with the winning default and the measurement
beside it in the ADR. If they do not, that is recorded and no knob is added — a configuration
option nobody can choose correctly is a liability, not a feature.

## 6. U5 — the in-house skiplist: ADR and design only

`memtable.rs:273` names it: `crossbeam-skiplist` hands out entries that borrow the map, so
`MemTableIter` remembers a key and re-finds it, `O(log n)` per step with a copy. An in-house arena
skiplist hands out an owned cursor and this goes back to `O(1)`.

The ADR covers the writer/reader model the memtable actually needs, the arena, the exact `unsafe`
surface with a `// SAFETY:` and a test per block, the concurrency test plan with the tools we
already have, the bench plan, and what the crate budget gains.

**No code without a coordinator GO.** The ADR is written, the lane reports, and the decision is
somebody else's.

## 7. What this lane will NOT do

* **Change `esker-raft`.** Invariant 4: it is a pure state machine. This lane models it and reads
  it; it does not touch it.
* **Change the `src/` of `esker-store`, `esker-pd`, `esker-client` or `esker-proto`.** The four
  mechanisms are already fixed and tested; U1 adds coverage, not a second fix. If a model finds a
  *new* bug in one of those crates, it lands as a red test in that crate's `tests/` and a report,
  not as a change to somebody else's live file (a red test is the handoff).
* **Write the skiplist.** U5 stops at the ADR.
* **Tune against a green test.** Correctness first, then numbers. No benchmark is a gate.
* **Add a dependency.** The runtime allowlist is closed; `stateright` and `proptest` are already
  dev-only dependencies of `esker-sim`.
* **Change an on-disk or wire format.** Nothing here needs one. If U4's numbers ask for a
  different tiered read size, that is a read-side knob, not a format.

## 8. Risks

* **A model that cannot see its bug.** The mitigation is procedural and not negotiable: every
  checker is run in a detached worktree at the fix's parent before it is committed, and the red
  output goes in §6.
* **The store bindings are heavier than the other two.** `esker-store`'s two mechanisms live in
  private async methods, so their bindings drive a real `Store` rather than calling a function.
  The model still owns the orderings and the checker; the binding is where it meets the code. If
  a binding cannot be made to compile at the pre-fix parent, the model is not landed with a
  claim it cannot support — the failure is reported instead.
* **`esker-engine` has reverse dependents.** U2 and U3 are gated on `esker-store`, `esker-cli`
  and `esker-pd` building and passing, not only on `esker-engine`.
* **Two lanes in one checkout.** Never stage a file this lane did not write.

## 9. Test list

| Unit | Where | Kind |
|---|---|---|
| U1a | `esker-sim/tests/mech_placement.rs`, `esker-pd/tests/sim_balance.rs` | seeded fault-injected runs |
| U1b | `esker-sim/tests/mech_retry.rs`, `esker-client/tests/sim_retry.rs` | seeded runs, no wall clock |
| U1c | `esker-sim/tests/mech_sweep.rs`, `esker-store/tests/sim_sweep.rs` | `stateright` + a real store |
| U1d | `esker-sim/tests/mech_ask.rs`, `esker-store/tests/sim_snapshot_ask.rs` | `stateright` + a real store |
| U2 | `esker-engine/tests/db.rs`, a new level-iterator property test | property, unit |
| U3 | `esker-engine` unit tests + a working-set test | unit |
| U4 | `esker-engine/tests/tier_minio.rs` (measurement only) | measurement |
| U5 | — | none; ADR only |

## 10. The runs, with seeds

Every model runs the same fixed seed list, so a failure is a rerun and not a story:

```
1 2 3 5 8 13 21 34 55 89 144 233 377 610 987 1597 2584 4181 6765 10946 17711 28657 46368 75025
```

### U1a — balance versus repair (`548dd62`)

| | |
|---|---|
| Model | `esker-sim/src/mech/placement.rs`, seeded five-store cluster, 8 regions |
| Binding | `esker-pd/tests/sim_balance.rs` → `esker_pd::balance::balance_for` |
| Runs | 24 seeds × 400 rounds = 9,600 rounds, every region queried twice per round |
| Reached | **24,953** policy queries against a region holding an unpromoted learner with **every store up** — the state the narrow rule could not see — out of 84,569 mid-repair queries in all |
| Also reached | 1,084 learners added, 1,052 promoted, 1,177 store-down events, 1,278 balance moves applied |

Green at `875d9c4` (this branch): 4 passed.

**Red at `ba8ed2e` (`548dd62^`)**, in a detached worktree with only `crates/esker-sim/` and
`crates/esker-pd/tests/sim_balance.rs` + `Cargo.toml` copied in — 3 of 4 fail:

```
test balance_never_touches_a_mid_repair_region ... FAILED
seed 1, round 0: balance planned AddPeer { region_id: 1, store_id: 4 } against region 1,
which is mid-repair (UnpromotedLearner { peer_id: 25, all_stores_live: true })

test leader_balance_has_a_repair_guard_of_its_own ... FAILED
the office moved out from under an unfinished repair
  left: Some(TransferLeader { region_id: 1, to_peer_id: 20 })
 right: None

test a_columnar_learner_is_not_a_repair ... FAILED
a plain learner must still stop balance
  left: Some(AddPeer { region_id: 1, store_id: 4 })
 right: None
```

The second is the one worth pointing at: `548dd62`'s trace is a `TransferLeader`, and the seeded
run cannot isolate it because `balance_for` asks `region_balance` first and it answers first. So
that shape is constructed — region counts dead level, leader counts far apart — and it fails at
the parent because `leader_balance` had no repair guard at all.

The fourth test, `the_checker_names_the_learner_when_it_fires`, passes at both revisions by
design: it is a guard on the checker's own output, not on the placement driver.

`esker-sim`'s own `tests/mech_placement.rs` proves the same three things about the *model* using a
reference policy, including that the checker fires on all 24 seeds against a deliberately narrowed
rule. That is not evidence about `esker-pd` and is not counted as any.
