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
| U2 | `esker-engine/tests/level_iter.rs` (3) | differential against a single-file level, plus a laziness measurement |
| U3 | `esker-engine/tests/table_cache.rs` (3) | hit rate, capacity, the degenerate capacity of one |
| U4 | measurement only, against `esker-minio` | measurement |
| U5 | — | none; ADR only |

Every one of U2's and U3's assertions was run against the code as it was before the change, by
patching the old rule back in — see §12. Two of them did not discriminate on the first attempt and
both are recorded there rather than quietly fixed.

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

### U1b — the retry budget under a moving epoch (`9791e16`)

| | |
|---|---|
| Model | `esker-sim/src/mech/retry.rs`, 24 seeded scripts of 10 answers + a tail |
| Binding | `esker-client/tests/sim_retry.rs` → the real `RawClient` over `FakeTransport` + `FakeClock` |
| Runs | 24 scripts, plus three constructed shapes |
| Reached | 19 of 24 scripts oblige an answer **past** the old attempt budget; 5 oblige giving up; 21 mix both kinds of refusal — the shape neither hand-written test contains |

Green at `486fef9` (this branch): 4 passed. The endless-progress run ends at the deadline after
14 calls and 9,940 ms of a 10,000 ms budget.

**Red at `1502d0f` (`9791e16^`)** — all 4 fail, and the first reproduces the recorded production
failure to the millisecond:

```
seed 1: the client gave up on attempts having spent 2266ms of a 10000ms deadline.
The script was [fppppfpppf then a], which obliged Answer { on_call: 11 };
it answered OutOfAttempts { calls: 9, elapsed_ms: 2266 }

an_epoch_that_never_settles_ends_at_the_deadline
  seed 0: [ then p forever] obliged RunOutOfTime; it answered OutOfAttempts { calls: 9 }

the_two_kinds_of_refusal_are_told_apart_on_the_wire
  left:  OutOfAttempts { calls: 9, elapsed_ms: 2266 }
  right: Answered { calls: 11 }
```

`2.266s of a 10s deadline, with 9 calls made` is the number in `9791e16`'s own commit message,
arrived at from a different direction: that one was a hand-built script of ten uniform refusals,
this is a drawn mixture. `fppppfpppf` is the interesting part — a budget that resets on progress
and one that never resets agree on every *uniform* script, so a model that only drew uniform ones
would have proved nothing the fix's own tests had not.

One thing the model got wrong first, and it was the model rather than the code: a constructed
20-refusal script asserted an answer on call 21, and the real client ended it at call 14 with
`OutOfTime` after 9,940 ms. That is correct — twenty backoffs on a 10 ms base doubling to a 2 s
cap do not fit inside a 10 s deadline. The assertion became the right one (the deadline is what
stops a long run of progress, which is the half that makes "progress does not spend the budget"
safe), and it is why the drawn scripts are capped at ten: a longer one would let the deadline end
a run the model meant to end on attempts, and the checker would have to accept two answers where
it should accept one.

### U1c — the sweep of a removed peer (`92a5add`)

| | |
|---|---|
| Model | `esker-sim/src/mech/sweep.rs`, an **exhaustive** table of every answer PD can give |
| Binding | `esker-store/tests/sim_sweep.rs` → a real two-store cluster per case |
| Runs | 7 cases; 5 driven through the cluster, 2 skipped with reasons (below) |
| Reached | 1 case is evidence of a removal and reclaims; 4 must leave the range alone |

No seed, because there is nothing to draw: the state space *is* the answer table, and enumerating
it is cheaper and stronger than sampling it. `tests/retire.rs` drives the one ordering where the
sweep is supposed to fire; this drives the ones where it must not, and those are the expensive
direction — a store that keeps a region it was removed from wastes disk, a store that drops one it
still holds loses acknowledged writes.

The removed peer's state is built without a removal: store 1 is **stopped**, which leaves store 2's
peer leaderless against a group of two it cannot reach. That is what a peer the cluster has
replaced is permanently, and what the probe counts rounds of — and it is what lets a case decide
what PD says next, including answers a real removal would never produce.

Green at `8f62ab2` (this branch): 2 passed, 19 s.

**Red at `c31a8a8` (`92a5add^`)**:

```
case "a newer membership that does not name this store":
required Reclaim,
observed Observed { still_hosted: true, keys_left: 6, keys_before: 6 } (NotReclaimed)
```

Both halves of `92a5add` in one line. `still_hosted: true` is "nobody tells a removed peer" — there
is no sweep at that revision, so on the operator path `retire_region` never runs. `keys_left: 6` of
`keys_before: 6` is the leak it was hiding. The file compiles there because it counts its own keys
rather than calling `snapshot::key_counts`, which `92a5add` added.

#### Two cases are not driven through the cluster, and both say why

* **"a region this store hosts covers the range."** Two overlapping regions cannot both be in one
  `RegionMap`, so the state only arises from a *stale* record — a parent narrowed by a split,
  retiring against the range it used to have — and a leaderless store cannot split. The gate is
  asserted directly instead, against `RegionMap::overlapping`, which is the call the reclamation
  makes.
* **"PD has never heard of the range."** Not constructible through `FakePd`'s public surface:
  `get_region` walks back to the last record containing the probe key, the region under test
  starts at the empty key, and the bootstrap wrote a record there. Overwriting it with any range
  still leaves a range containing the empty key.

The second one is worth recording because of how it was found. The first version of this file
"covered" it by placing a record elsewhere and watching nothing happen — which it did, because PD
went on answering with the bootstrap record that names this store. **It passed, for the wrong
reason.** `place_and_verify` now asserts what PD will actually answer *before* the case is watched,
so a mis-set-up case fails loudly instead. That assertion is what turned a green test into a
documented skip.

### U1d — the snapshot ask's wait (`1502d0f`)

| | |
|---|---|
| Model | `esker-sim/src/mech/ask.rs`, the answer table over (region hosted?, core knows?, record knows?) |
| Binding | `esker-store/tests/sim_snapshot_ask.rs` → the real sender, one single-node store per case |
| Runs | 6 cases; 5 driven, 1 not constructible |
| Reached | exactly 1 case serves, exactly 1 case is waited for |

The addition this makes over `tests/snapshot.rs` is **timing on every answer**, not just on the one
the fix's test looks at. "Not a member, on sight" and "not applied here, after the wait" are the
pre-fix and post-fix answers to the same question, so a checker that reads only the words would
pass a pre-fix sender that happened to phrase its refusal well. `WAIT_FLOOR_MS` is 400 against a
500 ms bound and a 2 ms poll: a sender that consulted the record twice cannot answer under it, and
one that answered on sight cannot exceed it.

The run also asserts `waited == 1` in both directions. A run where nothing waited is the pre-fix
sender; a run where more than one thing waited is a sender that makes every wrong ask cost the
bound.

Green at `b37e264` (this branch): 1 passed, 3.9 s.

**Red at `c062f91` (`1502d0f^`)**:

```
case "the core has the peer and the change can never commit":
required Required { answer: NotAppliedHere, waits: true },
got      Outcome  { answer: NotAMember,     waited_ms: 0 }   (RefusedAMember)
```

`waited_ms: 0` is the half a words-only assertion would miss. `RefusedAMember` is the model's own
name for it: a correct sentence about the wrong membership.

#### One state is not constructible

"The core has the peer and the change **commits**" — the gap that should end in a snapshot being
served rather than refused. A learner's addition commits on the existing voters alone, so the gap
is microseconds wide and cannot be held open from outside the store. Its mirror — the record
knowing more than the core — does not exist at all, because the record is applied *from* the log.

The half of `1502d0f` that this therefore cannot reach through a cluster is the one the *recorded*
fix got wrong: serving on the core's word ships a header that does not name the peer receiving it,
and `tests/promotion.rs` failed 3 of 3 with a learner stranded. The checker names that case
(`Half::ServedAStranger`) and `mech/ask.rs`'s own unit tests exercise it, but no revision in this
repository's history has it — the recorded fix was bisected out before it landed — so there is no
worktree to run it red in. Written down rather than claimed.

#### One thing the gate caught that is not a regression

`cargo test -p esker-store --test sim_snapshot_ask --test snapshot --test promotion` runs the
three binaries **concurrently**, and `promotion.rs` failed there at 151.82 s. Alone it passes in
31.20 s, and the full `cargo test -p esker-store` (21 binaries, cargo's own scheduling) passed it
too. Three multi-node cluster tests competing for cores on a box already running two other agent lanes'
compiles is the cause; nothing in this lane touches any `src/`.

Worth writing down rather than dropping, because `548dd62`'s own body says this test was
intermittent and that widening the window with debug logging made the race deterministic. This is
a different mode with the same symptom — starvation rather than a race — and the way to tell them
apart is that the failure does not survive being run alone. Do not read a red `promotion.rs` under
a parallel gate as the return of that bug without re-running it by itself first.

## 11. U2–U5, as built

### U2 — the two-level iterator (`07a3b97`)

Built as planned, with two things the plan did not say.

**The tombstone walk was doing as much work as the cursors.** `Db::iter` opened every file at every
level twice over: once to push a cursor, and once — the same call — to ask whether the file carried
a range tombstone. ADR 0017 decision 6 says a compaction whose inputs carry one becomes a discharge,
so no tombstone is ever written below L0 and that question only ever had one possible answer below
L0. The collection is now an L0 walk. The `debug_assert` that checked the invariant moved into
`LevelCursor::open`, where it fires on every file actually opened rather than on every file in the
database.

**`esker-cli bench` could not express the shape.** Four flags were added, all bench-only:
`--write-buffer-size`, `--target-file-size`, `--compact`, and later `--block-size`. Without a
compaction everything is in L0, which is the one level this change leaves alone; without a small
`--target-file-size` a compaction writes one output file and there is nothing to be lazy about. A
benchmark that can only measure the default shape can only answer questions about the default
shape.

And a workload, `scanrange`: seek to a random key, read `--batch-size` entries, once per operation.
`readseq` builds one iterator and walks 400,000 keys with it, so whatever building one costs is
divided by 400,000 — it is nearly blind to the thing this unit changed.

### U3 — the table cache evicts by use

`db/table_cache.rs`'s victim was `open.keys().next()`, the **lowest file number**. File numbers rise
monotonically, so that is the oldest file, which in a levelled engine is the one that has survived
the most compactions — the deepest, largest, most-read file in the tree. The cache was
systematically discarding its best entry.

Now a `HashMap` beside a `BTreeMap` from a monotonic use tick to a file number: exact LRU, O(log n)
eviction, forty lines of safe code, one lock so the two views cannot disagree.

**Not the sharded LRU the plan named, and the reason is written into the module.** `cache/lru.rs` is
eight shards of an index-linked arena because it holds hundreds of thousands of blocks on the path
of every block read. This holds 256 entries and its critical section is a map lookup and an `Arc`
clone. Sharding it would be optimising before a profile, which `CLAUDE.md` forbids, and would trade
an exact LRU for a per-shard approximation. `cache/lru.rs` is named as the shape to copy if a
profile ever shows this mutex.

`Options::max_open_tables` is new, because the question "which entry goes" has no observable answer
on a database with fewer files than the cache has room for.

### U4 — the tiered read block size: measured, and **no knob added**

The plan said measure first and add the knob only if the numbers say so. They say not to.

`TieredFile::read_at` issues exactly one ranged `GET` per call, so a **point read costs one round
trip whatever the block size** — 50,004 GETs for 50,000 operations at 4, 16, 64 and 256 KiB alike.
The block size cannot buy a point read a round trip; it can only change how many bytes that trip
carries, which is free up to 64 KiB and costs 34% of the throughput and 6× the p99 at 256 KiB.

A **scan** is the opposite: its GET count falls exactly in proportion (1,393 → 349 → 91 → 26) and
its throughput rises 14.7× across the same range.

So the knob contemplated — a tiered read size separate from the SST block size — would duplicate
`CfOptions::block_size` for scans and actively harm point reads, with no setting a reader could
choose correctly without knowing which workload was about to arrive. Nothing was added.

What the numbers *do* support is that **4 KiB is a poor default for a tiered column family**: 16
KiB is better in both workloads and 64 KiB is defensible. That is a format decision affecting every
non-tiered database too, the local-disk numbers for it are not in this table, and it is explicitly
**not this lane's to take**. It is recorded with its measurement in `docs/bench/phase-11-engine.md`
§3 for whoever does.

The bench flag `--block-size` stays, because it is what made the question answerable and what will
make the follow-up answerable.

### U5 — the skiplist ADR

[ADR 0040](../adr/0040-the-in-house-arena-skiplist.md), **design only**, as the brief required. The
lane stopped at the ADR and did not write the code.

The argument the ADR makes that was not obvious going in: the memtable does not need a general
lock-free ordered map. Writers are serialised by group commit, the structure is append-only for its
whole life, and a reader outliving a table's retirement is already handled by the `Arc` that
`MemTable::iter` holds. `crossbeam-epoch` exists to answer "when may a node freed by one thread be
reclaimed" — a structure that frees nothing until the arena drops never asks it. That reduces the
job to LevelDB's single-writer arena skiplist, and the `unsafe` surface to three functions.

## 12. The two tests that did not discriminate, and what fixed them

Both are U2/U3 tests that passed against the code they were written to catch. Recorded because the
failure mode is the same one §2 is built to avoid, and it does not stop being possible because the
lane has a rule about it.

**The level-iterator differential compared the level cursor with itself.** It spread the data by
turning `write_buffer_size` down, which controls how many *L0* files a fill produces — and the
compaction then merged them into one output file either way. `target_file_size` is the knob that
decides how many files a level holds. The shape assertion (`many > one`) is what caught it, and it
stayed in the test: a differential whose two sides came out identical proves nothing, and it should
say so rather than pass.

**The table-cache hit-rate test scored 0.215 under both rules.** Capacity 2 with three distinct
files touched per round evicts the hot file whatever the victim rule is, so the two rules were
indistinguishable by construction. At capacity 3 they separate cleanly:

```
least-recently-used   0.992  (595 hits,   5 misses, 254 evictions)
lowest file number    0.615  (369 hits, 231 misses, 469 evictions)
```

The threshold was then 0.55 — below **both** numbers. Two mistakes stacked: a fixture that could
not discriminate, and a bound that would not have noticed if it could. It is 0.90 now, with both
measurements written into the test beside it so the next reader can see where the bound came from.

The old rule also evicts nearly twice as often, which is the part that makes it worse than random
rather than merely different: everything it discards is something that gets asked for again.

## 13. `DESIGN.md` was right and the code was behind

Worth recording because it inverts the usual drift. §4.9 has said *"a merge iterator over memtables
and per-level two-level iterators"* since phase 1. The code has never done that — it pushed one
cursor per file at every level — so U2 is not a design change, it is the code catching up with a
document that was correct for four phases. The `TODO(post-v1)` at `db/iter.rs:406` names LevelDB's
shape without noticing that this project's own design document had already specified it.

`CLAUDE.md` says code and DESIGN.md must never drift and that a change fixes one of them. The
disagreement here was in the direction nobody checks for: the document was ahead.

§4.9 gains the two paragraphs the change makes true — why L0 is the exception, and why the
tombstone walk is an L0 walk — and §14 gains a row for the open-reader bound, which had no default
recorded anywhere.
