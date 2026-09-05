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

## 5. `promotion`: the writer gate closed, one arm still open

The curve was 3.90 / 188.5 / 75.8 / 202.6 s — up, down, up — so a race, and the panic could not say
which one because it reported `writer.is_finished()` as a bare boolean. Giving it the writer's
position answered it on the first run:

```
the cluster never settled: 5 learners seen, 5 promoted, writer_done=false after 569 of 600 writes
```

**Promotion had finished.** The test spent the rest of its budget waiting for the last thirty-one
writes of its own load generator, because `writer.is_finished()` was in the break condition. The
break now takes the subject — every placed learner voting — and "under load" is asserted instead:
the load must still have been **in flight** when the cluster began to grow.

The first version of that assertion required the writer to *advance* between the first learner and
the last promotion, and failed at **zero load** (`16 writes when the first learner appeared, 16 when
the last one voted`) because a quiet box finishes the whole promotion inside one write — the same
defect pointed the other way. It is rate-independent now, and its wrong first version is the
evidence it is not vacuous.

| busy threads | 0 | 14 | 40 | 80 |
|---|---|---|---|---|
| before | 3.90 s | **188.5 s FAILED** | **75.8 s FAILED** | **202.6 s FAILED** |
| after | 3.84 s | 15.6 s | 72.9 s | 70.4 s |

### Still open: the stranded learner, seen once

One run at forty threads failed differently, and this is the whole of what is known about it:

```
peer 34 of region 9 has been a learner for 30.146219674s — the phase-4 acceptance stall.
the placement driver holds [(10, 1, Voter), (26, 3, Voter), (34, 2, Learner)]
  at epoch Epoch { conf_ver: 4, version: 6 }, led by peer 10.
the learner's own store says: ["applied=75 leader=false epoch=Epoch { conf_ver: 4, version: 6 }",
                              "applied=75 leader=false epoch=Epoch { conf_ver: 4, version: 6 }"]
```

It is **not** the phase-14 duplicate-peer bug: one peer per store, and the learner *exists* and has
*applied* to the same index and epoch as the voters. Not reproduced since — it has not recurred in
any of the eight runs of the four-load curve.

What was missing is the third view. PD reports a role and the learner reports its own applied
index; neither can show what the **leader** believes, and that is what the promotion decision reads.
`leader_progress` now prints it — `matched`, `next`, `is_learner`, `pending_snapshot`,
`recent_active`, from whichever store answers `RaftPeer::progress` (it is empty unless the peer
leads, so asking all three finds the leader without racing a transfer). A learner PD calls a
learner, that says `applied=75` itself, and that the leader records at `matched=0` is not a slow
promotion — it is two parties describing different peers, which is the shape phase-14 U2 found
twice. The next sighting will say which.

The leadership churn beside it is worth its own look regardless of this test: PD's 64-entry history
was **entirely** `TransferLeader` in the failing runs, one region appearing eight times, and
`CLAUDE.md` lists predictable tail latency as a goal.

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

## 8. `DROP DATABASE`: the reclaim, and the one thing it still needs

`catalog::drop_database` walks every key the tenant owns inside a single Percolator transaction —
`txn.scan(&start, &end, u32::MAX)` over 257 ranges, then `txn.delete` per key. `Txn::scan` answers
`Vec<(Bytes, Bytes)>` **with values**, so a database of *n* bytes is *n* bytes of coordinator memory
before one key is deleted, and every key then becomes a prewrite lock and a commit record in one
transaction. It does not commit for a database of any size, and each delete is an MVCC version, so
the data grows before it shrinks.

[ADR 0069](../adr/0069-a-dropped-database-is-reclaimed-by-range-not-key-by-key.md) decides the
shape: the catalog drop stays a small transaction and is what makes the database *gone*; the rows
are not deleted, their **range is reclaimed**, gated on the GC safepoint and resumable from a
cursor. The safepoint gate is the part that makes it correct rather than merely fast — a
transaction whose snapshot predates the drop may still legally read those rows, so the clear waits
until the drop's commit timestamp is below the floor PD publishes.

The store half is built. `snapshot::clear_range` was already this exact operation — delete, flush,
discharge, verify, over all three families and both physical namespaces — but only for a whole
`Region`, and a tenant's range is part of one or spans several. The region-shaped calls are now the
general ones (`physical_ranges_of`, `first_key_in_user_range`, `clear_user_range`), with
`clear_range` delegating.

Bounded and idempotent fell out of the operation rather than being added to it: the delete is six
range tombstones in one synced batch, `O(1)` in the range's size and atomic across `kill -9`
because the batch is either in the WAL or it is not; re-clearing an empty range takes the early
return. The **compaction** is the unbounded step, so a whole-tenant reclaim walks the range a chunk
at a time — and a test compares the chunked form against the whole one rather than against an
assertion, because a chunk boundary is where a range mapping gets an off-by-one wrong.

### Blocked on one wire message, deliberately

The trigger crosses the wire and `esker-proto` is the coordinator's to sequence. ADR 0069 names
what is needed and stops:

```text
TxnKvReq::ReclaimRange { start: Bytes, end: Bytes, below_ts: u64 }
```

a sibling of the `TxnKvReq::GcSafepoint` a store already answers — `below_ts` being the drop's
commit timestamp, so the safepoint condition is a property of the request rather than of the
caller's timing. Until it exists, `clear_user_range` is reachable in-process only and
`DROP DATABASE` keeps its current behaviour.

`esker-sql` also keeps a cheaper fallback that needs nothing from this crate: chunked *logical*
deletes across many transactions, re-driven by the schema-job machinery that already exists. It is
still `O(keys)` and still leaves the space to GC, but it is bounded, idempotent and crash-safe
today. If the wire message is not sequenced, that is the answer.

## 9. The seventh sighting: a panic inside `thread::scope`, and why it hung

`esker-cli::cluster_chaos::a_sigkilled_leader_process_never_costs_an_acknowledged_write` hung for
**29 minutes** in b4's gate with three lanes' containers up, having passed the four gates before it
and the two after. It is the only one of the seven that needed no reproduction at four loads: b4
sampled the wedged process, and every observation in that sample follows from one line of the test.

```rust
let took = converged.unwrap_or_else(|| panic!("… did not serve a write within …"));
worst = worst.max(took);
cluster.restart(leader);
…
stop.store(true, Ordering::Relaxed);          // after the kill loop, so: never
```

`std::thread::scope` joins every thread it spawned before it returns — **including while a panic is
unwinding through it** — and the four load generators run `while !stop.load(..)`. When
`time_to_serve` returns `None`, the main thread panics, the scope waits for four threads nothing
will ever stop, and the panic never finishes unwinding.

| what the sample showed | why |
|---|---|
| the test and its four threads in `futex_wait` | the scope's join, and the drivers still looping |
| the runtime idle in `epoll_pwait` | nothing is waiting on a socket; this was never a network stall |
| **the killed node gone and not restarted** | `cluster.restart` is two lines below the panic |
| `cluster start` alive and idle | nobody reached `cluster.stop` |

The supervisor was never at fault, and the brief's first reading — *a supervisor that does not
restart a killed node* — is not what `cluster start` does: it is documented to name a dead child and
keep going, and the **test** is what restarts nodes.

What it cost is worse than a slow gate. The test exists to catch a cluster that does not converge
after a `SIGKILL`, and a convergence failure is precisely the case where it stopped saying so — the
message naming the node and the budget is written, and never printed.

`ReleaseDrivers` is a guard whose `Drop` sets the flag, so every exit from the scope releases the
drivers rather than only the successful one. Red first, with a budget nothing can meet so the
failure is deterministic rather than load-dependent:

| | `CONVERGE_WITHIN` = 1 ms |
|---|---|
| without the guard | killed by `timeout` at 150 s, **no message** — the recorded symptom |
| with the guard | FAILED in 9 s: *after killing node 1, the cluster did not serve a write within 1ms* |

`CONVERGE_WITHIN` stays 5 s. No budget was widened; what changed is that expiring one is a failure
instead of a hang.

### The shape, for the next one

Six of the seven sightings were found by running the test at four loads and reading the curve. This
one was found by reading a sample — and it is the case where the curve would have said least,
because the failure is not slower under load, it is *silent* under load. The discriminator was that
the runtime was **idle**: a test that is waiting on a cluster has a runtime with something to do.

## 10. What the seven were, and the rule that found six of them

Seven tests were handed to this lane as one family — "fails only under load". They were not one
family. Two were product defects a quiet machine hid, five were test defects, one record was stale
before it was written, and one is still unreproduced.

| # | sighting | what it turned out to be | where |
|---|---|---|---|
| 1 | `cli::cluster_start` a driver that cannot listen | **two product defects**: a readiness check that could not tell the driver from a squatter, and an announcement gated on `sleep(250 ms)` + "has anybody died yet" | `58ed28af`, `2a82232e` |
| 2 | `store::sim_sweep` the sweep reclaims on evidence | **three test defects**; the store was obeying ADR 0034 and the test blamed it | `ea1135fb` |
| 3 | `store::promotion` a learner becomes a voter under load | **one test defect** — success was gated on the *load generator* finishing — and **one race still open** | `3e436086` |
| 4 | `client::crash_through_the_client` | **not a sighting**: fixed at HEAD three hours before the register that listed it as open (`46166886`) | — |
| 5 | `store::snapshot` a snapshot replacing a held region | a race, and **h1's**, not this lane's | h1 |
| 6 | `sql::real_backend` a scan reads every row | **unreproduced** in 44 runs across four load models; blocked on a capture, not on a reproduction | — |
| 7 | `cli::cluster_chaos` a SIGKILLed leader | **a test defect**: a panic inside `thread::scope`, which joins while unwinding | `d3698dca` |

Two product defects, five test defects, one stale record, one unreproduced. Beside them, debt #7
`sql::join_cost` was diagnosed and left to its owner (§7), debt #3 turned out to owe an ADR rather
than code (§4, ADR 0068), and debt #8's Miri gate was three lines (§3).

**The thing worth carrying out of the wave is that "load-sensitive" was the wrong category.** Not one
of the seven was fixed by changing a budget, and only one — promotion's remaining arm — is even
plausibly about speed. What load did was expose a wrong assumption that a quiet machine kept true.

### The standing rule: run it at four loads and read the curve

Before touching a test that fails only under load, run it at **idle / 14 / 40 / 80 busy threads**
and read the shape. The shape names the fault, and it named six of these seven:

| curve | what it means | seen in |
|---|---|---|
| rises monotonically | genuinely slower; the budget may be the honest fix | none of the seven |
| flat, then a **cliff** | a state change, not a slowdown — something stops happening | 1, `0.38 / 0.31 / 0.32 / 60.05 FAILED` |
| **fails faster than it passes** | a wrong verdict, not a slow test | 2, `12.0 s FAILED` against `15.7 s` passing |
| **up, then down** | a race; more load is not more failure | 3, `3.9 / 188.5 / 75.8 / 202.6`; and #7's `join_cost`, green at 0, red at 14, green at 40 and 80 |
| rises steeply, never fails | not reproduced; stop and fix the capture instead | 6, `0.88 / 3.71 / 5.25 / 36.2 s`, 44 runs, 0 failures |
| no curve at all — it hangs | the curve says least here; look at whether the runtime is **idle** | 7 |

Sighting 7 is the exception that sharpens the rule. It is not slower under load, it is *silent*
under load, and a curve would only have recorded a timeout. What found it was a sample: every
thread in `futex_wait` with the tokio runtime **idle in `epoll_pwait`** — a test genuinely waiting
on a cluster has a runtime with something to do. So the rule has a second half: **when a test hangs
rather than fails, sample it before running it again**, and read whether anything is waiting on I/O.

### Three habits this wave paid for, and one it corrected

* **Verify the record.** Two of the four register rows this lane owned were closed before the
  register was written, though it opens by claiming every row was verified against the tree. A debt
  record is a hypothesis (§4).
* **Show it red.** Every fix here has a red arm, and three of them were made deterministic rather
  than load-dependent — a degenerate case table at base 0, a convergence budget of 1 ms, a range
  mapping that ignores its end bound. A red that needs eighty busy threads is a red that will not be
  re-run.
* **Watch the test that passes too easily.** `the_wait_ends_when_the_port_answers` *asserted* the
  defect in sighting 1; `an older membership than this store's` had never once put an older record
  in front of the sweep in sighting 2. Both were green for years.
* **And read the exit code from the checker.** Three handovers in this wave reported `fmt ✅`
  through `cargo fmt --all --check | tail -3 && echo OK`, which reports `tail`'s status. The gate's
  first step was red and every lane was calling it green.

## 11. `TxnKv::ReclaimRange` — **applied**; this section is what it was staged as

ADR 0069's one wire message, sketched against the tree at `1228cf11` so that whoever applies it
after the ruling is transcribing rather than designing. `esker-proto` is the coordinator's to
sequence; **nothing below is applied.**

### The shape

```rust
// crates/esker-proto/src/txn.rs — TxnKvReq
/// Reclaim the storage under a user-key range whose owner has been dropped
/// ([ADR 0069](../../../docs/adr/0069-a-dropped-database-is-reclaimed-by-range-not-key-by-key.md)).
///
/// **Not a delete.** Nothing routes into a dropped tenant's key space, so this does not resolve
/// versions or take locks — it clears the range physically, which is only sound below the
/// safepoint. A store refuses until its own safepoint has reached `below_ts`.
ReclaimRange {
    /// Inclusive lower bound, a **user** key.
    start: Bytes,
    /// Exclusive upper bound. Empty means the end of the key space.
    end: Bytes,
    /// The commit timestamp of the drop this reclaim belongs to.
    below_ts: u64,
},

// TxnKvResp
ReclaimRange {
    /// How far the store got: the caller resumes from here rather than restarting.
    /// Equal to the request's `start` when the safepoint was too low and nothing was done.
    cursor: Bytes,
    /// Whether this store has nothing left of the range.
    finished: bool,
    /// The safepoint in force, so a blocked caller can say *why* rather than retry blindly —
    /// the same courtesy `GcSafepoint` already extends by answering with the safepoint now in
    /// effect rather than the one that was asked for.
    safepoint: u64,
},
```

### Every place it touches

| file | what | note |
|---|---|---|
| `messages.rs` | `TxnReclaimRange = 0x020A` | **`0x0209` is `TxnLatestCommit`** (ADR 0067), so `0x020A` is the next free code. Claim it out loud when taken |
| `messages.rs` | `Method::is_mutation` | **true.** It deletes data. The test `reads_are_not_mutations_and_everything_else_is` enumerates the mutating methods and must gain it |
| `txn.rs` header | the `0x02NN` table at the top | it currently stops at `0x0208` and is already missing `0x0209`; add both rows while there |
| `txn.rs` | `TxnKvReq::routing_key` | **`start`**, like `Scan` — a reclaim is addressed to the range's first region and the store clamps to its own end, exactly as `Command::DeleteRange` already does |
| `txn.rs` | request `encode`/`decode` | `put_bytes(start)`, `put_bytes(end)`, `put_varint(below_ts)` — length-prefixed, in that order |
| `txn.rs` | response `encode`/`decode` | `put_bytes(cursor)`, **`put_bool(finished)`** — the codec has one, and it is the byte the golden row below pins — then `put_varint(safepoint)` |
| `txn.rs` tests | `every_request_routes_by_a_key`, the method-order test | the order test asserts `0x0201 + index`, so the new variant goes **last** in that list |
| `server.rs` | the `TxnKvReq` match arms at `:2852` and `:2922` | where `GcSafepoint` is answered; this is where `reclaim::advance` is called |
| every crate matching on `TxnKvReq`/`TxnKvResp` | — | a new variant breaks matches in crates nobody edited; build the workspace, not the crate |

### Golden rows

`docs/DESIGN.md` §9's framing is unchanged — this is a body, not a frame. The bytes to pin, with
`start = "d"`, `end = "" `(the end of the key space), `below_ts = 300`:

```text
request  body   01 64            start:  len 1, 'd'
                00               end:    len 0  (the end of the key space, not an absent field)
                AC 02            below_ts: varint 300
response body   01 66            cursor: len 1, 'f'
                00               finished: put_bool(false)
                AC 02            safepoint: varint 300
```

Checked against the codec rather than remembered: `put_bytes` is `put_varint(len)` then the bytes,
and `esker-base`'s own test asserts `encode(300) == [0xAC, 0x02]`.

The empty `end` is the row worth having: it is a **length-prefixed empty string**, not an omitted
field, for the same reason `meta.rs` gives about a region's `end_key` — absence and emptiness would
be the same bytes and one of them means "to the end of the key space".

### What the store side already is

`crate::reclaim::advance` (`564dcc77`) takes the record, the hosted regions and the safepoint and
returns `Blocked`/`Advanced`/`Finished`. The handler is a translation:

* `Blocked { safepoint, .. }` → `cursor` unchanged, `finished: false`, that `safepoint`;
* `Advanced { cursor }` → that cursor, `finished: false`;
* `Finished` → `cursor: end`, `finished: true`.

The record is created on the first request for a range and removed by `advance` when it finishes,
so the message carries no id and the store needs no session state. A repeated request for a range
already reclaimed finds no record, creates one, finds no hosted region overlapping it, and answers
`finished: true` — which is the idempotence the caller needs and costs one pass with no writes.

### Region-addressed, ruled

A reclaim is **addressed to one region**, like every other request in this service, and not
broadcast to a store. Two reasons and they are the same reason: invariant 5 — every request carries
a region epoch — and a range that spans regions is the *driver's* job to split, which
`reclaim::next_chunk` already does one hosted region at a time. A store-addressed broadcast would be
fewer round trips and would need a way to address a store rather than a region, which `TxnKv` does
not have and should not grow for this.

### Three things the tree decided for us, found while staging it

* **`0x020A` is forced, not chosen.** `every_service_numbers_its_methods_without_a_gap` collects each
  service's method numbers and asserts they are exactly `1..=count`. Any code but the next one fails
  it — so the number is not a preference and cannot collide with a lane that picks differently.
* **The golden rows are additive by construction.** `golden()` finds a line by its
  `"{kind} {name} "` prefix, so two appended lines change nothing that already exists. That is worth
  stating rather than hoping: a format change to an existing row is the thing `CLAUDE.md` says to
  stop and ask about, and this is not one.
* **The older-peer refusal is already tested, and adding a method erodes it.**
  `an_unknown_method_is_an_error_not_a_skipped_frame` walks a list of unused codes and asserts a
  peer refuses them rather than mis-parsing. `0x020A` was one of the boundaries that list stood on,
  so it gains `0x020B` — the next code this service has not issued. Keeping the first *unused* code
  in that list is what makes it a test about an older peer meeting a newer method, rather than a
  test about four numbers that were free the day it was written.

The bytes above were **derived from the code and cross-checked**, not remembered: a request body is
`method:u16` little-endian ++ header ++ fields (a response has no header), `header()` is
`RequestHeader::new(1, Epoch::new(2, 3), 4)` → `01 02 03 04`, and the layout was verified against the
existing `txn-get` row before the new one was written. `esker-base`'s own test asserts
`encode(300) == [0xAC, 0x02]`. Mint by running `golden_request_bodies` once — it prints both sides on
drift, so a derivation error arrives as a diff rather than as a silent pass.

## 12. The reclaim, end to end

`TxnKv::ReclaimRange` is applied at **0x020A**, region-addressed, and `DROP DATABASE`'s rows are
reclaimed by range under the safepoint gate with a resumable cursor. The staging in §11 was
transcription: the minted goldens matched the derivation there byte for byte.

### The compiler found the reverse dependents, and there were four

`Method::ALL`, `esker-client`'s `txn_payload_size`, `esker-store`'s `txn_request_range` and
`txn_command::from_request` — plus the service classification, `name()`, and both enumerating
tests. None of them is in a crate this change is *about*, which is the argument for building the
workspace rather than the crate.

Two decisions in that list are reversible and so are stated rather than buried:

* **The span it is checked against is `[start, end)`**, like `Scan`. That is what the epoch guards,
  and it is why a range spanning regions is refused region by region rather than served wholesale.
* **It writes and is still not a `Command`.** Clearing storage under a range nothing can route to
  is housekeeping each replica does to its own copy — ADR 0034's shape. Through the log it would
  make one replica's compaction schedule the whole group's business and need a replicated format
  change to say nothing more.

### The end-to-end test, and the two things it found

`tests/reclaim_range.rs`: a store in a **separate process** on a real socket, split into three
regions so the range spans more than one chunk, `SIGKILL`ed between two of them, and finished by a
fresh process that has never seen the request. It asserts the kill is *genuinely* mid-reclaim —
three regions, one chunk per pass, so the first pass must leave work behind — because a crash that
lands after the work is done proves nothing.

**The safepoint gate is re-applied across the crash, and that is a safety property.** The safepoint
is PD's to publish and lives in memory, so a restarted store has not learned one and refuses to
carry on clearing on the strength of a record written before the crash. Worth stating because the
opposite reading is available and wrong: this is not the reclaim forgetting its progress. It resumes
from the persisted cursor — the test pins both halves, that the first answer after the restart is
blocked *and* that its cursor is where the dead process left it.

**And a leadership wait that cost twenty seconds of nothing.** The child waited on `Store::peer`,
which answers for one region of the three it hosts; on the restart it answered none, and the loop
spent its whole budget before serving anyway. Asked of `region_statuses` instead: **21.15 s ->
1.35 s.** The same shape as §5 and §9 — a wait on the wrong observable, paid for in a budget nobody
was reading.

### The gate, five steps for the first time

`fmt=0 clippy=0 doc=0 deny=0 tests=3378/3378 doctests=0`, each status read from its own command.
The wave's earlier gates ran three of those five: `cargo deny` and the doctests were missing,
because a container gate had been built beside `just check` rather than from it (h1's finding), and
`run.sh` exited 0 regardless until it was fixed. Both are why the fmt line in §10 was green for days
while it was red.

## 13. The durability assertion is sound, and the proof is in the code

`cluster::no_acknowledged_write_is_lost_when_the_leader_is_killed` failed once in h1's container run
at **1.8 s** — a normal-duration run, so an assertion fired during ordinary operation rather than at
the end of a slow one. The question was whether an acknowledged write is really lost or the test
mis-attributes an unacknowledged one. **It does not mis-attribute.** Every link is now checked
against the source rather than argued:

| link | how it is known |
|---|---|
| `propose` returning `Ok` means the entry **applied on the leader** | `complete_proposal` has one call site, at the end of the apply path; every other resolution — the unknown-command path, the truncated-index path, the append-failure drain — sends `Err` |
| applied ⟹ **committed** | `RaftLog::applied_to` clamps with `.min(self.committed)`, and `next_committed` hands out only `[applied + 1, committed + 1)` |
| committed ⟹ **a majority of voters hold it in their logs** | `maybe_commit` sorts the voters' `matched` descending and takes `matched[quorum - 1]`, with §5.4.2's term condition refusing to commit an earlier term's entry by counting |
| `last_acked` names **the acknowledged write itself**, not a later entry | measured: the leader's applied index moved by exactly 1 for each of the 12 writes, in 65 runs, with no exceptions |
| a follower's `last_index` is its **log tail**, and logs do not shrink below commit | `Status::last_index` is documented as the last index in its log; truncation is above the commit point |

So on three nodes `holders == 1` — the leader having applied an entry that **neither** follower holds
— cannot happen unless commit safety is violated. And at 1.8 s only `assert_quorum_holds` can have
fired: the convergence path spends ten seconds inside `eventually` before it panics.

**If that assertion fired, it is a real violation of invariant 1.** Not lag, not a stale read, not
the test naming the wrong index.

### What is not known, and it is the reproduction

233 sound runs on the macOS host produced nothing: 30 solo, 78 across four loads with a flat curve
(1.33 / 1.49 / 1.29 / 1.34 s — this test is not slowed by CPU load at all), 60 with ten concurrent
clusters, 65 instrumented. h1 saw it in the **Linux container**, which is the one environment this
lane has not been able to run in — different scheduler, different loopback.

The message would settle it in one line, and it is already diagnostic: `assert_quorum_holds` prints
every peer's `term/commit/applied/last` from before the kill, so the next occurrence says which
peers held the entry and which did not. Nobody has captured it yet — the same gap as §6's sighting,
and for the same reason.

## 14. The eighth sighting: a redirect with nowhere to follow it to

`esker-store::snapshot a_snapshot_replacing_a_held_region_routes_through_a_retire` failed in g1's
gate on 2026-09-04 at **35.854 s**, inside a 3421-test run that otherwise passed. The same test had
been seen twice that afternoon at ~65 s and both times the 60 s `AWAIT_DEADLINE` was named as the
suspect. This failure is under that deadline, so the deadline was never the mechanism.

The message said so on its own:

```
writing b"k00058" never succeeded after 9046 attempts in 30.000411395s;
last answer peer is not the leader of region 1;
last asked store 1, whose peer says leader=Some(Some(2))
```

Nine thousand attempts at ~300/s, every one to store 1, which named peer 2 as the leader every
time. That is not a slow write. It is a loop with nothing to do with the answer it is given.

### What was already right, and why it did not help

`put` follows `NotLeader`'s hint. That landed earlier in this file with
`a_write_follows_the_office_when_it_moves`, and the reasoning in its doc comment is correct: an
election moves no epoch, so re-asking the peer that just disclaimed leadership asks a question
already answered.

But the hint names a **peer**, and `put` can only ask the stores its *caller* hands it:

```rust
let store = group[at % group.len()];
```

`announce_a_snapshot_and_await_the_retire` wrote keys 40..60 through `&[&first.store]` — a group of
one — and it does so **after** `AddPeer` has made peer 2 a voter. So `at % 1` is 0 whatever the
hint says. There was a redirect, and nowhere to follow it to.

### Why the test that proves the fix could not catch the bug

`a_write_follows_the_office_when_it_moves` passes `&[&first.store, &second.store]`. It proves the
hint is *read*, and it hands `put` the group that already contains the answer — the one thing the
failing call site did not have. A green redirect test and a livelocked redirect are consistent:
the test exercises the branch, never the precondition it depends on.

This is the same shape as the memo *a test can pass on the mechanism it is not testing*.

### The audit

Every one-store `put` in the file, and whether a second voter can exist when it runs:

| site | group | second voter possible? |
|---|---|---|
| `a_write_follows_the_office_when_it_moves` (before) | `&[&first.store]` | no — `second` is not open yet |
| `a_region_reaches_a_store_that_never_had_it` 0..40 | `&[&first.store]` | no — before `open(second)` |
| the transfer test's `key(100)` | `&[&first.store]` | no — before `open(second)` |
| the retire test's 0..40 | `&[&first.store]` | no — before `open(second)` |
| **`announce_a_snapshot_and_await_the_retire` 40..60** | **`&[&first.store]`** | **yes — after `AddPeer`** |
| two single-node tests | `&[&node.store]` | no — one store is the cluster |

One at-risk site, and it is the one that failed.

### The fix, and the guard that would have found it first

1. The call site passes both stores.
2. `put` now fails **at once** when the hint names a peer hosted on a store it was not given,
   printing the peer, its store, and the group. No number of retries reaches a store a caller did
   not hand over, so spending the deadline only re-proves the first refusal.
3. `a_put_says_which_store_it_was_not_given` pins that, and the setup both redirect tests need is
   extracted into `two_voters_with_the_office_on_the_second` rather than duplicated.

### Red first, and with no load at all

The office moved on purpose, exactly as §5's method says. Without the guard:

```
writing b"k00001" never succeeded after 8398 attempts in 30.003246871s;
last answer peer is not the leader of region 1;
last asked store 1, whose peer says leader=Some(Some(2))
```

The gate's message, reproduced on demand in 32 s under **zero** spinning threads — the sighting
needed load only because it needed an election, and an election can be asked for. With the guard:
the three redirect tests pass in 2.37 s, and `esker-store` is 318/318.

**Sighting seven of eight was a test defect; so is this one.** Still nothing fixed by changing a
budget.

## 15. `promotion` at 300 s: my instrument, not a verdict

The unit-0 arm ran the five load-sensitive tests eight times each at 14 busy threads. Four were
clean:

| test | runs | failed | worst |
|---|---|---|---|
| `snapshot` retire | 8 | 0 | 5.07 s |
| `cluster_start` | 8 | 0 | 0.63 s |
| `sim_sweep` | 8 | 0 | 18.97 s |
| `crash_through_the_client` | 8 | 0 | 23.37 s |
| `promotion` | 5 | **1 (rc=124)** | 300.06 s |

`rc=124` is GNU `timeout` firing at my own 300 s cap. It is not the test's verdict, and it destroyed
the diagnosis: promotion's internal budgets are `PROMOTION_DEADLINE` 30 s, `put` 90 s, the split
wait 60 s, and the watch loop **180 s**. A legitimate slow path — 300 writes, a split, two stores
opening, then the watch — can exceed 300 s without any defect, so an outer cap of 300 s cannot tell
"failed" from "needed longer", and it pre-empted the `leader_progress` instrumentation added in §5
for exactly this moment.

The same test passed at 13.550 s in g1's gate and at 10.132 s in this lane's crate run. The next
measurement needs a budget **above the sum of the test's own deadlines** (600 s), so that whatever
fires is the test's own assertion with the leader's `matched/next/is_learner/pending_snapshot`
attached. Recorded as open, and deliberately not called a sighting: nothing has failed yet.

## 16. One test, three mechanisms, one assumed precondition

`a_snapshot_replacing_a_held_region_routes_through_a_retire` failed in **three of four gates** on
2026-09-04 and became the most frequent red on the machine — at 35.854 s, at 61.508 s with no load
arm running at all, and at 60.957 s in this lane's own gate **with §14's fix already in**. Three
failures, two different panic sites, one root.

`announce_a_snapshot_and_await_the_retire` reads `first` as the leader throughout: it measures the
gap against `first`'s applied index, and it sends the announcement from peer 1 with `first`'s term.
That premise is true when the helper is written and false as soon as `AddPeer`'s learner is
promoted — the region then has two voters, and a two-voter group on a box that will not schedule
its threads elects the other one, which this file's own `put` doc records as fifteen times in
twenty runs.

Nothing held the premise. Depending on *when* the office moves, it breaks in three places:

| when the office moves | what fails | how it looked |
|---|---|---|
| during the 40..60 write loop | `put` re-asks a follower for 30 s | `9046 attempts in 30.000411395s` (§14) |
| before the gap is measured | `old` is the *leader*, so it is never behind `leader`, and the gap never appears | `timed out after 60s waiting for a gap between the learner's applied index and the leader's` |
| between the gap and the announcement | `receive_raft`'s held-region branch is guarded by **`!peer.is_leader()`**, so an announcement aimed at the peer that now leads is correctly ignored | `timed out after 60s waiting for the old peer to be retired` |

The third is the one the store is *right* about: a leader does not accept a snapshot aimed at it.
The test was wrong to send one.

### Why §14's fix moved the symptom rather than removing it

§14 gave the write loop both stores, so writes now succeed after an election instead of
livelocking. The helper therefore no longer dies at 30 s — it proceeds, with a premise that is
still false, and dies at 60 s further on. That is why this lane's gate, holding the §14 fix, still
lost the test: **fixing one face of an assumed precondition promotes the next face.**

### The fix

The helper holds its own premise rather than assuming it: if store 1 is not leading when the helper
starts, the office is transferred back to peer 1 and waited for. Immediately before `receive_raft`
the premise is asserted again, so a future race says *"the office moved to the peer this
announcement is aimed at"* at once instead of sixty seconds of silence.

`a_held_region_is_retired_even_after_the_office_has_moved` drives the adverse state on purpose —
wait for peer 2's promotion to Voter, `TransferLeader` to it, then run the scenario. The setup both
retire tests need is extracted into `a_second_store_holding_region_one`.

**Red first, no load at all:** 60.74 s, `timed out after 60s waiting for a gap between the
learner's applied index and the leader's`. Green: both retire tests in **0.89 s**, `esker-store`
319/319, clippy clean.

### The general shape

A learner cannot hold the office, so the first attempt at this test could not move it and failed
with `timed out waiting for the second store to lead` — the promotion to Voter is the precondition
of the precondition. Driving a case means establishing every step of it, and the gates reached this
state only because their write loop ran long enough for PD to promote the learner first.

Three of the eight sightings in this wave now have the same shape: **a test whose premise is
established once and then assumed to hold.** Still nothing fixed by changing a budget.

## 17. The arm that was worth running, and a verification of mine that was not

One arm, 14 threads, GNU `timeout` proved against a fast success (rc=0) **and** a real hang
(rc=124) before it was trusted. Aborted early — see the last part of this section.

### promotion, with a budget above its own deadlines

§15 said the 300 s cap could not tell "failed" from "needed longer". At 600 s the test's own
assertion fires, and it is reproducible: **2 of 4 runs failed at 14 threads** (158 s, 204 s; the
passes took 107 s and 338 s).

```
promotion.rs:174: writing b"k000659" never succeeded; the last refusal was: None
```

`the last refusal was: None` is the whole finding. `last` is only assigned after `store.serve`
returns an error, so `None` means **no store was ever asked** — for ninety seconds, not one of the
three passed all three gates in the loop:

```rust
let Some(state) = store.regions().find(&key) else { continue };  // holds no region for this key
let Some(peer)  = store.peer_of(state.id())   else { continue };  // holds it, but has no peer
if !peer.is_leader() { continue }                                 // has a peer that does not lead
```

Three very different states, one silent `continue`, and a message that reports only the absence of
a refusal. So the message now names which gate each store stopped at and who it believes holds the
office. A leaderless region and a key no store admits to owning are not the same finding, and the
next occurrence will say which it is. **The measurement is not repeated here** — this is the
instrument being fixed, not the defect.

### The retire test's third face: the other half of the same guard

§16 fixed the office half of `!peer.is_leader() && peer.applied_index() < index` and this lane's
own crate run then lost the test again, at the same 60 s retire wait — with the §16 assert *not*
firing, which proves the receiver was not the leader. That leaves the applied-index half.

It closes on its own. The gap is measured, then the announcement is built and delivered, and in
that window the learner applies more of the log — `leader.status().await` sits **between** the
measurement and the send, which is an await in exactly the wrong place. Under load the learner
reaches the index that was chosen, `receive_raft` skips the branch, and a single-shot wait spends
its whole deadline.

The announcement is therefore retried rather than sent once: each round re-measures, names an index
the learner has not reached at the moment of sending, and watches briefly before announcing again.
The term is read once above the loop, so no await separates the measurement from the send. Losing a
round now costs one more announcement instead of the deadline, and the failure reports the rounds,
the announcements, both applied indices and the gap between them.

Four gates, three faces, one guard. §14 and §16 each removed a face and promoted the next.

### A verification of mine that was not a verification

The arm was stopped early: load average reached **196**, far outside the ≤14 rule. Killing the
spinners and checking them found two faults that both read as success:

* **zsh does not word-split an unquoted `$PIDS`.** `for p in $PIDS` runs the body *once* with the
  whole string, so `kill -9` fails `illegal pid: 49786 49787 …` and `kill -0` fails identically —
  and a `still-alive=0` counter built on that reports "all dead" while fourteen loops spin at 100%.
* **`kill -0` is not a liveness check.** It returns non-zero for permission denied as well as for
  no-such-process, so a failure never proves absence.

`ps -p <pid>` is the check that works, and it is what confirmed all fourteen were gone. The
previous arm's cleanup was sound — its script was bash with a real `PIDS=()` array, and it was
independently confirmed by a `ps` sweep — but **"verified per PID with `kill -0`" is what I reported
to the coordinator, and that sentence describes a check that cannot fail-closed.** It is now
corrected in the standing lane guidance.

The load itself was not the spinners alone: with all fourteen gone the average was still 196, and
macOS `StorageManagementService` and `ApplicationsStorageExtension` were burning 58% and 65%. Disk
is healthy (72% used, 1.0 TiB free).

## 18. Proving a negative and waiting for an event are not the same wait

§15 left `sim_sweep` open with a diagnosis and no fix, because it went 0/12 solo and this wave's
rule is to measure before touching. The diagnosis stands on the evidence rather than on a
reproduction, and it is readable straight off the failure:

```
case "a newer membership that does not name this store": required Reclaim,
observed Observed { still_hosted: false, keys_left: 6, keys_before: 6 }
```

`still_hosted: false` says the store **gave the region up** — the decision the case is about was
taken, and taken correctly. Only the six keys were still on disk. The sweep had not refused to
reclaim; it had not finished reclaiming.

`check`'s `Reclaim` arm failed on `keys_left != 0` alone, so it could not tell those apart, and
`observe` gave both kinds of case the same three-second clock. But they are different waits:

* a case that requires `Reclaim` is waiting for an **event** — the region gone and its keys with
  it. Its budget only has to be long enough that reaching the end means the reclaim *stalled*.
* a case that requires `Keep` is proving a **negative**. The only way to do that is to spend the
  window, and then check the throttle really got its rounds inside it.

Three seconds is right for the second and arbitrary for the first. So the positive case now waits
on its event (`RECLAIM_DEADLINE`, 30 s) and the negative keeps exactly the window it had
(`NOTHING_HAPPENS_WINDOW`, 3 s) together with the round guard, which is untouched. A reclaim that
never finishes still fails — at the deadline, and now saying which half is outstanding, because
`Half` gained `ReclaimUnfinished`: *the store gave the region up and its keys are still on disk*,
which points at ADR 0034's cursor rather than at the decision.

This is not a budget increase dressed up. The negative proof is unchanged; the change is that a
positive is no longer judged by a stopwatch.

### And the test I wrote two sections ago had the same bug

`a_put_says_which_store_it_was_not_given` (§14) failed once inside a loaded crate run and 0/8
solo. It calls `two_voters_with_the_office_on_the_second()` and then writes — assuming the office
*stays* on peer 2. A two-voter group can elect peer 1 back before the call starts, and then the
write simply succeeds, the redirect branch is never reached, and `#[should_panic]` reports the
absence of a panic rather than the reason for it.

That is precisely the fault §16 and §17 are about, committed by me while writing them up. It now
re-establishes the office and retries, bounded, and says so if it never got a clean attempt:

> the office returned to store 1 before each of 20 attempts, so the redirect branch was never
> reached and this test asserted nothing

**Nine sightings in this wave; the count of them fixed by changing a budget is still zero.**

## 19. A region with no leader for ninety seconds

The instrument fixed in §17 answered on the **first run** of the next arm, and the answer is not a
test defect.

```
writing b"k000733" never succeeded in 90.001262152s; the last refusal was: None
  store 1: peer of region 77, is_leader=false, believes leader=None
  store 2: peer of region 77, is_leader=false, believes leader=None
  store 3: peer of region 77, is_leader=false, believes leader=None
```

Read it against the three gates the old message could not distinguish:

* **not** "no store holds a region containing this key" — all three hold region 77;
* **not** "holds the region but has no peer of it" — all three have a peer;
* every peer says `is_leader=false`, and every peer says `believes leader=None`.

Nobody leads, and nobody believes anybody else does, for **ninety seconds**. This is region 77,
which the test's own load created by splitting. `put` is not failing to find the leader; there is
no leader to find.

That is a liveness finding in the store, not in the test. It is not invariant 1 — nothing
acknowledged was lost, the writes simply never happened — but a range that cannot elect anybody is
a range that cannot be written to, and `regions_reach_a_store_that_joins_and_none_is_left_without_a_leader`
is a sibling test whose whole subject is that this must not happen.

### What is not yet known, and how the next occurrence will say it

`leader=None` everywhere is still ambiguous between two very different states, and the message
could not separate them:

* every peer is a **follower** whose election timer keeps being reset, so nobody ever campaigns;
* every peer is a **candidate**, campaigning and losing, over and over.

And a third possibility the membership settles: a freshly split range whose peers are still
**learners** has no voters, and a group with no voters cannot elect anyone however long it waits.

So the diagnostic now carries, per store, the peer's `term`, its Raft `role`, its `voted_for`, and
the region's full membership with each member's Voter/Learner role. The next occurrence says which
of the three it is in one line, without another arm to set it up.

**This is handed over rather than fixed.** Diagnosing a leaderless region is store work — the apply
loop, the campaign path, or the split's conf state — and this lane's remit was the tests around it.
The reproduction is cheap: 14 busy threads, `a_learner_on_a_fresh_store_becomes_a_voter_under_load`,
1 of 1 on the arm that found it and 2 of 4 on the arm before.

### The arm, and the guard that stopped it

One arm, 14 threads, GNU `timeout` proved both ways first. It aborted itself before run 2: the
1-minute load average reached 108 against the ceiling of 80 written into the script after §17's
incident. Spinners were killed and verified with `ps -p` — the check that works. The guard cost
seven runs and prevented a repeat of the 196 that stopped the previous arm; the finding arrived on
run 1 regardless.
