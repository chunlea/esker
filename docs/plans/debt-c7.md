# Debt wave c7: the load-sensitive tests, and the engine/store debts left at v1

Five tests in `esker-store`, `esker-cli` and `esker-client` failed only while four lanes gated in
parallel and the machine's load average sat at 8–10. The brief called them a family; they are not
one. Two were real product defects, one was a test measuring from a base it guessed, one was
already fixed at HEAD, and one is a race that is still open at the end of this wave.

## 0. Method: four loads, and read the curve

Taken from h1's fix of `snapshot::a_snapshot_replacing_a_held_region_routes_through_a_retire`
(that test is not in this record — h1 owns it). Run the same test at **idle / 14 / 40 / 80 busy
threads** before deciding anything. A test that is merely slow rises monotonically. A cliff, or a
curve that goes up and then down, is a race or a state change, and no amount of budget fixes it.

Every measurement below is in the Linux container (`ESKER_TARGET_VOL=esker-target-linux-c7`), with
the load made by `bash -c 'while :; do :; done'` processes inside the same container.

| test | 0 | 14 | 40 | 80 | read as |
|---|---|---|---|---|---|
| `cli::cluster_start` a driver that cannot listen | 0.38 s | 0.31 s | 0.32 s | **60.05 s FAILED** | cliff — a state change |
| `store::sim_sweep` the sweep reclaims on evidence | 15.7 s | ok | ok | **12.0 s FAILED** | fails *faster* than it passes |
| `store::promotion` a learner becomes a voter under load | 3.90 s | **188.5 s FAILED** | **75.8 s FAILED** | **202.6 s FAILED** | up, down, up — a race |
| `client::crash_through_the_client` | ok | ok | ok | 3.5 s ok | already fixed at HEAD |

## 1. `cluster_start`: two defects behind one 60-second budget

### 1.1 A port that answers is not a driver that answers

Sampling the stuck run — rather than re-running it — said what state it was in at once:
`cluster.state` written, `4 nodes started` printed, all four stores alive, and `cluster start`
parked in `epoll_pwait` inside `wait_for_interrupt`. The command had announced a cluster whose
placement driver could never bind.

`wait_until_listening` polled `TcpStream::connect`. That asks *is anybody listening here*, and
anybody is not the driver: the test's own squatter satisfied it, and so would a driver left over
from an earlier run or another cluster on the same base port. The probe is now a round trip only a
driver completes — the wire handshake, then `Pd::Status` — bounded by a 500 ms `request_timeout`
(which bounds the handshake too) and retried until the **unchanged** 20 s `PD_START_TIMEOUT`.

`the_wait_ends_when_the_port_answers` had bound a listener that was not the child and asserted
`Ok`. It pinned the defect, so it is gone; its replacement stands up a real `TestPd`.

### 1.2 A cluster is announced when its stores answer, not when none has died yet

Fixing 1.1 closed one door. Measuring the other — node 1's port taken, the driver's left free —
found it open: at no extra load the bind failure lands inside `first_child_that_died`'s 250 ms and
the command correctly refuses in 0.36 s; at eighty threads it does not, and the command announces
four nodes and writes a state file naming a pid that is already dead. `stop` reads those pids and
signals them.

`sleep(250 ms)` then "has anybody died yet" is a **negative assertion behind a wall clock**, the
shape `debt-c6.md` §9 names three times in this tree, and what it proves is equally true of a store
that has not finished opening. The gate now waits for every store to answer `Admin::Regions` on its
own port; a child that has exited is still reported as that, checked every pass, because an exit
status is the diagnosis and silence is only its symptom.

| | 0 | 14 | 40 | 80 |
|---|---|---|---|---|
| a driver that cannot listen, before | 0.38 s | 0.31 s | 0.32 s | **60.05 s FAILED** |
| after | 0.62 s | 0.71 s | 0.72 s | 0.74 s |
| a node that cannot listen, before | 0.83 s | — | **FAILED** | **FAILED** |
| after | 0.82 s | 0.92 s | 0.81 s | 1.11 s |
| four nodes register four stores (control) | 0.22 s | 0.58 s | 1.90 s | 1.26 s |

**Debt #6 is closed**, and it was not a flake: it was two product defects that a quiet machine hid.

## 2. `sim_sweep`: the store was right and the test was wrong, three times

c6 measured every clock in this test and concluded none was the mechanism. That was correct — it is
not a clock. The failure at eighty threads is `ReclaimedWrongly`: the sweep dropped a range, which
the test's own message calls losing acknowledged writes.

Printing the real numbers beside the assumed ones ended the question in one line per case:

```
LOAD=0   case "an older membership than this store's"  real_hosted_conf_ver=2  case_assumes=3  answer_conf_ver=3
LOAD=80  case "the same membership this store already has"  real_hosted_conf_ver=2  case_assumes=3  answer_conf_ver=3
```

1. **The wait watched the leader; the value was read from the follower.** `observe` waited for peer
   2 to be a voter in the *first* store's view — which proves the conf change committed, not that it
   applied here — then read `hosted` from the *second*. Under load the two diverged and every case
   ran against a **learner**.
2. **The base `conf_ver` was hard-coded 3.** So a case built at base 3 met a store holding 2, and
   "the same membership this store already has" became a record one `conf_ver` *newer* that does not
   name this store — positive evidence of a removal, and reclaiming on it is exactly ADR 0034. The
   test asked for `Keep` and blamed the store for obeying the ADR.
3. **`cases(2, 1, 0)` made "older" degenerate.** `0.saturating_sub(1)` is `0`, so *an older
   membership than this store's* was the same record as *the same membership this store already
   has*. That case had never once put an older record in front of the sweep, at any load, in any
   run. The table ran five cases and covered four.

The wait now watches the store the value comes from; `rebase` puts the table onto the `conf_ver` the
cluster actually reached; `cases` refuses a base of zero. Red first, and two of the three reds need
no load at all:

| red | against | in |
|---|---|---|
| `no_two_cases_put_the_same_thing_in_front_of_the_sweep` | `cases(2, 1, 0)` | 0.001 s, names both cases |
| `a_voter_here_and_not_only_on_the_leader` | the wait on the leader | 2.5 s at 40, 1.4 s at 80, prints `role: Learner` |
| the `ReclaimedWrongly` reproduction | the hard-coded base | needs load; it is why the two above exist |

After: 15.6 / 17.7 / 20.0 / 34.1 / 34.2 s across 0 / 14 / 40 / 80 / 80. A slope, not a cliff.

## 3. Debt #8: the Miri gate stops needing a flag to run at all

proptest's default `FileFailurePersistence` calls `std::env::current_dir`; Miri refuses `getcwd`
under isolation, so the memtable module aborted unless the caller passed
`-Zmiri-disable-isolation`. An abort inside a dependency's failure-persistence code reads like the
property test failing rather than the harness never starting, which is why three lines were worth
more than the sentence of documentation that had been standing in for them.

| `cargo +nightly miri test -p esker-engine --lib -- memtable` | result |
|---|---|
| without the fix | aborts in `proptest`'s `test_runner/failure_persistence/file.rs:89` |
| with the fix | **36 passed, 1 ignored, 0 failed**, 117.9 s |

`docs/bench/skiplist.md` §3 now carries the command without the flag and both measurements.
`docs/acceptance/v1.md` §0 states the flag in two places; that file is not this lane's and the flag
still works, so it is named in the handover instead of edited.

## 4. Debts that were not what the record said

`debts-v1.md` opens by saying every row was verified against the tree rather than transcribed.
Two of the rows this lane owns were not.

| # | the record | HEAD |
|---|---|---|
| 5 | "`crash_through_the_client` starves under load … the child is killed on a **wall clock**" | **Closed before the record was written.** `46166886`, *the crash loop kills after acknowledged writes, not after milliseconds*, landed 2026-09-04 06:45; the register was written at 09:48. The test kills after `1..8` acknowledged writes and has done since. Green at eighty busy threads in 3.5 s. |
| 3 | "`Db::ingest` refuses any overlap, tombstones included … c6 verified this as the one item of eight that HEAD still owes" | **The widening landed nine hours before the record.** `616954e8`, *an ingest is refused for a shared key, not for a shared range*, 2026-09-04 00:41. The module header states the key rule and argues it against range-disjointness explicitly, and `tests/ingest_overlap.rs` property-tests it — including point tombstones and range deletes — against a rule computed from the inputs rather than from the code under test. The site the record cites, `DbInner::place`, is documented as *"placement, not permission"* and returns a level. |

**ADR 0068** now records the rule, which is what #3 actually still owed. Refusing an ingest whose
keys collide, tombstones included, is **correct and should not be widened**:
a point tombstone is an entry under its key, so a sequence number would still have to decide between
the delete and the ingested value, and the two numberings are unrelated. What #3 still owes is the
ADR, because the rule is a decision a future reader might reverse — range-disjointness is what the
`RocksDB`-shaped intuition reaches for, and the argument against it lives in a module header rather
than in `docs/adr/`.

## 5. Still open

**`promotion::a_learner_on_a_fresh_store_becomes_a_voter_under_load` is a race and is not fixed.**
The curve is 3.90 / 188.5 / 75.8 / 202.6 s — up, down, up — and the failures are not one failure:

| load | what failed |
|---|---|
| 14 | `3 learners seen, 3 promoted, writer_done=false`. Promotion worked; the **load generator** never finished its 600 writes. PD's 64-entry history is entirely `TransferLeader`, one region appearing eight times. |
| 40 | `peer 34 of region 9 has been a learner for 30.1 s`, PD holding `[(10,1,Voter),(26,3,Voter),(34,2,Learner)]` at `conf_ver 4`, the learner's own store at `applied=75` and the same epoch. A stranded learner that is **not** the phase-14 duplicate-peer bug: it exists and it has applied. |
| 80 | as 14 |

Two distinct states under one deadline, so it is at least two questions. The next reader should not
start from the 180 s budget: start from `writer.is_finished()`, which the panic reports as a bare
boolean and which should report **how far the writer got** — the difference between key 899 of 900
(a clock) and key 350 (something is blocking) is the whole diagnosis, and the test does not say.
The leadership churn in the history is worth its own look regardless of this test: `CLAUDE.md` lists
predictable tail latency as a goal, and a region whose leadership moves eight times in one 64-event
window is not that.

## 6. The sixth sighting: `esker-sql::real_backend`, unreproduced, and why

Relayed from the coordinator's gate of `c6fa73b9`: `a_scan_reads_every_row_a_real_store_holds`
panicked on the `unwrap` of an `INSERT` of 250 rows into a real three-store cluster after 18.7 s,
and passed 3/3 alone in 0.5 s. The test file is `esker-sql`'s and was not touched; the suspicion was
a store or client budget, which is this lane's.

Four load models, **44 runs, zero failures**:

| model | runs | worst |
|---|---:|---:|
| 0 / 14 / 40 / 80 busy threads, one run each | 4 | 36.2 s at 80 |
| 8 copies of the binary at once, two batches | 16 | 2.5 s |
| 8 copies at once **plus** 60 busy threads, three batches | 24 | 9.8 s |

The single-run curve is **monotone** — 0.88 / 3.71 / 5.25 / 36.2 s — which is the shape of a test
that is merely slow, and a 41x rise is a lot of slow. But the run that took 36.2 s **passed**, and
the sighting failed at 18.7 s, so "slower than some threshold" does not explain it either.

**What stops the diagnosis is the capture, not the reproduction.** The gate log holds

```
thread 'a_scan_reads_every_row_a_real_store_holds' (29300) panicked at crates/esker-sql/tests/real_backend.rs:73:14:
failures:
```

— the `panicked at` line, and then the *next* line is gone. libtest prints the message on the line
after that one, and the log has no stdout/stderr blocks at all, so the capture is a whitelist filter
that matched `panicked at` and not the message it introduces. Line 73 is
`session.run(&format!("INSERT INTO t VALUES {}", …)).unwrap()`, so the discarded line is a
`SqlError` carrying its sqlstate — which is exactly the thing that would say whether this is a lock
wait, a commit budget or a transport timeout, and it is one line.

So the ask is the capture: keep the gate's output unfiltered, or at minimum `grep -A 2 "panicked at"`.
Until then a budget cannot be named honestly, and naming one from the timing alone would be a guess
of the kind `debt-c6.md` §9 spent a unit refuting.

## 7. Debt #7 `join_cost`: diagnosed, and it is not this lane's to fix

The brief said to leave `esker-sql::join_cost::a_materialised_join_costs_what_it_pairs_and_not_the_cross_product`
alone but to say so if unit 0's work explained it. It does, and this wave's own closing gate
produced the message the register says was never captured:

```
8x the rows cost 10.3x the time where the control cost 3.4x (3.0x as much growth):
1.620926ms at 250 rows, 16.701596ms at 2000.
```

Re-run alone 3 times: **3 of 3 green**. At four loads: green at 0, **red at 14**, green at 40 and
80 —

```
LOAD=14  8x the rows cost 16.7x the time where the control cost 4.0x
```

**Non-monotonic, so a race rather than a slow test**, and for a ratio test that has a specific
meaning. The four measurements are strictly sequential:

```rust
let (small, small_rows) = cost(250, JOIN);
let (large, large_rows) = cost(2_000, JOIN);
let (small_control, _) = cost(250, CONTROL);
let (large_control, _) = cost(2_000, CONTROL);
```

so the control is measured **after** the subject, not beside it. Load that arrives or departs
between the second and third measurement moves `growth` and `control` independently — and
cancelling load common to both arms is the one thing a control is for. That also explains the shape
of the curve: at eighty threads *both* arms are slow together and the ratio is stable again, while
moderate bursty load is where they diverge. The danger zone is the middle, which is why more load
did not make it worse.

The denominator makes it sharper. `small` is **1.6 ms**, and it sits under `large` in `growth`, so
one scheduler preemption of a few milliseconds is an error of over 100% in the numerator of the
ratio the assertion reads.

Two changes would close it, both in `esker-sql` and so not made here: **interleave** the arms —
alternate `JOIN` and `CONTROL` and compare medians, so shared load cancels — and lift the small case
off the noise floor so the ratio's denominator is not a 1.6 ms sample. `docs/plans/debt-c4.md` §9's
rule applies to the diagnosis as much as to the fix: this was found by making it deterministic at
one load, not by counting runs.
