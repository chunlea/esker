# Phase 14, lane f1: four load-sensitive tests, treated as races until proven otherwise

Four tests in this workspace fail intermittently under a full parallel run and pass alone. Each has
been recorded — twice, in three plan files — and each has been mitigated rather than diagnosed: two
by `.config/nextest.toml`'s serialised `cluster` group, one by a Gatekeeper warm-up, one not at all.
A mitigation that stops a test failing is not a mitigation that stops the thing it was failing
about, so this lane goes after the mechanisms.

The method is `docs/plans/debt-c4.md` §9's, which is the only one in this tree that has worked on a
flake: **make it deterministic first**. Debug logging widened a promotion race enough that it failed
8 of 8 and printed the trace that named it; counting runs would have taken all night and proved
less. So for each test here: a reproduction that fails reliably, a mechanism read off a trace, a
fix, and the regression shown **red against the pre-fix code** before it is shown green.

Two rules for the whole lane, from §4 of the brief and from `CLAUDE.md`:

* **No timeout is widened to make a test pass.** A budget raised until the race stops showing is a
  race that has moved into production.
* **`.config/nextest.toml` is not touched.** Serialising more binaries is the mitigation that
  already exists; this lane is the other half of that answer.

---

## 1. The four, with every sighting

### 1.1 `esker-sql::redrive::two_re_drivers_racing_take_each_step_exactly_once`

`crates/esker-sql/tests/redrive.rs`. Three nodes over one `MemoryBackend`; two of them re-drive the
same orphaned `CREATE INDEX CONCURRENTLY` job from two threads released by a `Barrier`, and the
assertion is a **count**: from `delete-only`, exactly two transitions exist on the way to `public`
and exactly two are taken, however many passes race.

| when | at | how it was seen | message |
|---|---|---|---|
| phase 13 (catalog lane), 2026-09-01/02 | `catalog` branch gate | relayed in this lane's brief: "fails about one run in N" | not captured |

Nothing in `docs/` records it; the brief is the only sighting on paper, and it names no assertion.
That is the first thing to fix — a flake with no captured message is a flake nobody can tell from a
different one.

**Hypothesis.** The name of the test says *racing* and the assertion says *exactly once*, so the
two failure families are far apart: a step taken twice or zero times is a correctness bug in the
re-drive code, and a race that did not happen is a test that proved nothing. The test itself
contains the discriminator — its last assertion is `overtaken + conflicts > 0`, "the two re-drivers
never actually collided, so nothing about racing was tested" — so which of the two it is can be read
straight off the panic line, once there is one.

**Reproduction plan.** Run the binary alone many times; if that is green, run many copies at once,
because the barrier synchronises the *start* of two passes and a pass is long — a loaded machine can
run one to completion before the other is scheduled at all.

### 1.2 `esker-store::promotion::a_learner_on_a_fresh_store_becomes_a_voter_under_load`

`crates/esker-store/tests/promotion.rs`. A real PD, three stores, real Raft over real sockets, load
running while the cluster grows; every learner PD places must become a voter within
`PROMOTION_DEADLINE` (30 s), timed from when that learner was first seen.

| when | at | how it was seen | message |
|---|---|---|---|
| debt wave c1 | `a035ba7`-ish | its writer asserted every error it saw was retryable; `Unknown` is not | fixed by teaching the writer that an ambiguous answer is ambiguous (`docs/plans/debt-c1.md`) |
| debt wave c3, unit 2 | the naive "serve on the core's membership" fix | **3 runs of 3**, learner stranded the whole 30 s | closed by ADR 0035; not a flake, a real stall |
| phase 10 (routing lane) | full workspace run, twice | passed standalone in 34 s both times | panic at `promotion.rs:308`, its own `PROMOTION_DEADLINE` |
| debt wave c4, closing gate | `f090674`, 2026-09-01 19:43 | 2,383 of 2,384 | `server is busy: leadership transfer to 40 is in progress`, then `region epoch does not match` |
| debt wave c4, unit 9 | with `RUST_LOG=esker_store=debug,esker_pd=debug` | **0 of 8 passed** — the race made deterministic | trace in `docs/plans/debt-c4.md` §9 |
| phase 11 (engine lane) | `--test sim_snapshot_ask --test snapshot --test promotion` concurrently | failed at 151.82 s; **31.20 s alone** | starvation, not the c4 race — "the way to tell them apart is that the failure does not survive being run alone" |
| after `548dd62` | d1 and e2 lanes, per the brief | at least once each under a parallel gate | not captured |

`548dd62` (2026-09-01 20:02) fixed one mechanism — `balance::is_mid_repair` counted a peer on a down
store and nothing else, so a replica added on a live store and not yet promoted looked settled, and
a `TransferLeader` against it blocked the very `AddVoter` that would have finished the repair. After
it: 8 of 8 with debug logging, 5 of 5 plain.

**Hypothesis, in three parts, to be separated rather than assumed.**

1. A second ordering bug of the same family as `548dd62` — a scheduler reaching a state repair
   passes *through*. This is the one worth the most; `is_mid_repair` was the fourth instance of
   "voters, not peers" in that one module.
2. Starvation, as phase 11 recorded: nothing wrong with the cluster, the machine is simply not
   giving it cores. Distinguishable exactly as phase 11 says — it does not survive being run alone.
3. The test measuring a clock rather than an event. `PROMOTION_DEADLINE` is an assertion about wall
   time on a box whose load the test does not control, and even a correct cluster fails it if the
   box is slow enough. If (1) is excluded, the honest fix is to make the test wait on an
   **observable** — the AddVoter proposal, the conf change applying — rather than on 30 seconds.

**Reproduction plan.** `RUST_LOG=esker_store=debug,esker_pd=debug` first, since that is the
configuration that made this test deterministic once already; then an interleaved A/B against
`548dd62` with a control arm, in a detached worktree, under a load generator that is the same in
both arms.

### 1.3 `esker-cli::cluster_start::a_four_node_cluster_with_a_driver_registers_four_stores`

`crates/esker-cli/tests/cluster_start.rs`. Starts a placement driver and four stores as **real
processes** and asserts, from the driver's side, `stores (4)` within 60 s.

| when | at | how it was seen | message |
|---|---|---|---|
| debt wave c3 | before `c3b64d9` (2026-09-01 18:05) | **three times in one session**, every time at 60.2 s, every time on the run right after a build, passing in 0.33 s straight afterwards | `the driver never saw 4 stores` |
| phase 10 (routing lane) | full workspace run, twice | once before `.config/nextest.toml` serialised the CLI binaries and once after; standalone 0.57 s | its own 60 s budget |

`c3b64d9` diagnosed the first three as macOS `syspolicyd`: the first execution of a freshly linked
binary is evaluated at `_dyld_start` at 0% CPU, and the test spawned the binary and *then* started
its clock. It now runs `--help` once before the clock starts.

**Hypothesis.** The warm-up pays Gatekeeper for `esker-cli`, and `esker-cli` is the only binary this
test spawns directly — but `cluster start` re-spawns `current_exe()` per node, and `pd inspect` is a
fresh process per poll. Either the warm-up does not cover what actually pays, or the remaining
sightings are a different mechanism: four stores and a driver, five processes, each doing a
`Store::open` on a cold page cache, against a 60-second budget that is a wall clock.

**Reproduction plan.** Run it against a freshly linked binary under load and watch the processes:
`sample`/`ps` on a store that has not registered says at once whether it is at `_dyld_start`, in
`Store::open`, or connecting. Distinguish "the OS has not started it" from "it started and did not
register", which is the split the recorded evidence never made.

### 1.4 `esker-sql::pd_wiring::an_alter_reports_every_range_that_wants_columnar_replicas`

`crates/esker-sql/tests/pd_wiring.rs`.

| when | at | how it was seen | message |
|---|---|---|---|
| debt wave c4, closing gate | `548dd62`, with the type lane's `ba8ed2e` beneath it | 1,845 of 2,397 run, 1 failed (nextest cancels the rest) | not captured |
| the same, in isolation | `548dd62` | **5 of 5 green**; green at `ba8ed2e` too | — |

**Hypothesis.** Unknown, and the sighting has no message, which is why it is last. It is in the
serialised `cluster` group already, so whatever it is competes with the rest of the suite rather
than with another cluster.

**Reproduction plan.** As 1.1: alone, then many at once, then under a full workspace run with the
message captured this time.

---

## U1 — `redrive::two_re_drivers_racing_take_each_step_exactly_once`

### The reproduction, and it is not the one the name suggests

Alone, **60 of 60 green**. Sixteen copies of the binary at once, five times over: **68 of 80 red**,
every one on the same line and none of them on the count:

```
two re-drivers: 2 transitions, 0 overtaken, 0 rolled back
panicked at crates/esker-sql/tests/redrive.rs:574:5:
the two re-drivers never actually collided, so nothing about racing was tested
```

`moved` was `["write-only", "public"]` in all 140 runs. So the recorded flake is **not** a step
taken twice or zero times; it is the test's own guard saying it proved nothing — which is the guard
working, and a gate that is red one run in N for a correct system.

### The mechanism, off a per-round trace

Printing each round's two `Pass` values under the same load says it in three lines:

```
round: b=Ok(Pass { jobs: 1, steps: [], failed: [] })
       c=Ok(Pass { jobs: 1, steps: [Step { said: "write-only", moved: true, batches: 0 }], failed: [] })
round: b=Ok(Pass { jobs: 1, steps: [Step { said: "public", moved: true, batches: 3 }], failed: [] })
       c=Ok(Pass { jobs: 1, steps: [], failed: [] })
```

B took **no step at all** in the round C stepped, and C took none in the round B stepped. A
`std::sync::Barrier` synchronises the moment two passes *start*, and a pass is not a step: it is a
catalog scan, a table read and, at write-only, a whole backfill. On a loaded box one pass runs to
completion before the other is scheduled — and the second then finds the job's fingerprint changed,
resets its wait and skips (`redrive.rs`: *"Somebody stepped it since the last pass ... the wait
starts again from here"*). Nothing overlaps, so nothing is refused, so nothing is counted.

Aligning two threads is not the same as overlapping two transactions, and only the second is what
this test is about.

### The fix: an interleaving the test builds

`Gate` in `tests/redrive.rs`, in the seam the file already owns — `NodeBackend` — so no production
code learns about the test. Every transaction reports its two edges, and one node's driver is held
at whichever the test names: **before its next snapshot** (`Hold::Begin`), or **between its last
read and its commit** (`Hold::Commit`). The other driver's *whole pass* then runs inside that
window, and only then is the held one let go. There is no window to lose and no order to get lucky
about; `wait_until_parked` panics rather than proceeding if the driver never arrives, so a schedule
that stops being the built one fails loudly instead of quietly going back to hoping.

The racing test now collides on **both** of its rounds, on every run, and keeps
`overtaken + conflicts > 0` plus the exact `(0, 2)` it now constructs.

### What the forced race then found, which 140 runs of the old one had not

Two windows the barrier version never reached, both **red first**:

| held at | what it read | before |
|---|---|---|
| the read that chooses its step | `XX000 internal error: index 2 has no schema-change job in flight` | `verbs::step_job` |
| the batch it was about to run | `XX000 internal error: no schema-change job for index 2` | `job::backfill_batch` |

Both are the same fact: the **other** driver ran the backfill out, took `public` and forgot the job,
and this one's next read found nothing. Which is the most ordinary outcome in this module — two
nodes re-driving one job is what it is *for* — reported as an internal error.

The second one matters more than it looks. `adding_step` treats anything but `40001` out of a
backfill batch as a change that failed on **data** and calls `job::unwind`, which walks the index
back to `absent`. What stops it tearing down an index that is already `public` is that `unwind`
finds no job and returns early — a guard, not a reason, and one transaction of distance from a
`CREATE INDEX` that undoes itself because two nodes both did their job.

**The fix, three reads on the step path:** a job that is gone is `Overtaken` — "somebody else did it
first", the same sentence one step further on.

* `verbs::step_job` — absent job → `Ok(Stepped::Overtaken)`. `esker_schema_step` still answers a
  human who names an index with no job at all; it checks in the same transaction before it gets
  here, so this arm is only ever the race.
* `job::advance` — absent job → `Ok(false)`, which is what its own contract already called "somebody
  else did it first".
* `job::backfill_batch` — absent job → `Ok(true)`; a job that is gone has no backfill left, and the
  advance that follows answers `overtaken`.

ADR 0020's re-driver amendment said `overtaken` covered a state that *has moved*; it now says a
change that has *finished* too.

### Numbers

| | before the fix | after |
|---|---:|---:|
| the racing test alone | 60 of 60 | 10 of 10 tests, every run below |
| 16 copies at once × 5 (the reproduction) | **12 of 80** | **80 of 80** |
| 16 copies at once × 4, after the rename | — | **64 of 64** |
| `a_driver_whose_job_is_finished_before_its_step…` | **red**, `XX000` | green |
| `a_driver_whose_job_is_finished_inside_its_step…` | **red**, `XX000` | green |
| `cargo nextest run -p esker-sql --all-features` | — | **597 of 597** |

144 concurrent runs of the whole binary is 1,440 test executions, on the load that produced 68
failures out of 80 before. The one new test that was **green against the unfixed code** is
`a_re_driver_that_reads_after_the_winner_is_overtaken_rather_than_stepping`, and it is written down
as such: it pins the sequential half of the race, which already worked and had no test that forced
it.

---

## U2 — `promotion::a_learner_on_a_fresh_store_becomes_a_voter_under_load`

### `548dd62`'s lever is spent, so the reproduction had to be a new one

`RUST_LOG=esker_store=debug,esker_pd=debug`, alone: **3 of 3 green**, 23–28 s. That was the
configuration that failed 8 of 8 before `548dd62`, and it no longer fails at all — the balance/repair
race is fixed and is not what is left.

What does reproduce it is **load**: four copies of the binary at once, twelve stores and four
placement drivers on one machine. **2 of 4 red**, with the traces below. Both reds are the same
state and neither is a slow cluster.

### The mechanism, in four log lines

```
WARN  transport: no store known for a peer of this region; the message was dropped
      region_id=5 peer=27                                       (x51, over 9.5 s)
WARN  transport: a message was addressed to a peer on this store; it was dropped
      region_id=5 peer=27                                       (x31, for the rest of the run)
DEBUG not promoting: the learner has not caught up region_id=5 learner=27
      matched=0 next=88 leader_matched=88 pending_snapshot=0 recent_active=false
```

`matched=0` with `next` tracking the leader exactly, `recent_active=false`, and both of the region's
stores fully applied at `applied=92`. The learner is not behind; **it does not exist**. The second
warning says why: the route for peer 27 resolves to the sending store itself, so region 5 has *two
peers on one store* — and `RegionMap::insert` refuses a second outright, "a store never holds two
peers of one region". A peer that can never be created is a learner for ever, and PD, counting a
replica that exists only on paper, stops seeing the region as short and never repairs it.

### Where the second peer came from

An `AddPeer` is answered when its conf change **applies**. A leader that steps down with the
proposal in its log answers `it may still commit` — which is not *it did not*:

```
10:35:33.449  stopped leading; answering what this peer can no longer promise region_id=5 applied=69
10:35:33.449  an operator did not apply ... node=27 error=... it may still commit
```

PD observes nothing, times the operator out (`operator timed out with nothing observed; it will be
re-derived ... sends=3`), re-derives the same plan onto the same store — and both changes commit.

The store is the party that could have refused, and its check was on the wrong key:

```rust
esker_proto::Operator::AddPeer { store_id, peer_id, .. } => match state
    .region().peers.iter().find(|peer| peer.peer_id == *peer_id)
```

**PD mints a fresh peer id every time it issues one.** `esker-pd`'s `repair.rs` says so and is right
to: *"a fresh peer id, from the persisted allocator, every time an `AddPeer` is issued ... reusing
the id of an operator PD has forgotten would risk two peers with one id"*. So the id differs by
construction on every re-derivation, and the guard answered "not a repeat" to every one of them. The
comment directly above it already described the intent correctly — *"arriving here for a peer that
is already a learner is a repeat rather than a second step, PD re-deriving after a timeout"* — and
the check could not see one.

The same family as `balance::is_mid_repair`'s "**voters, not peers**", which that module names four
times: an identity read off the wrong field, turning a transient ambiguity into a permanent state.

### And the record is not the whole membership — the second half, found by the fix's own test

`already_placed` asked the region **record**, and the very next run said:

```
region 3 has peers 16 and 27 both on store 3 — a store hosts one peer per region, so the second
can never be created and never votes
```

with, ten lines above it, ADR 0035's own sentence:

```
WARN a snapshot did not arrive; the leader will offer it again region_id=3
     error=unsupported: peer 16 is in region 3's configuration but the change that put it there
     has not applied here within 500ms
```

**In the configuration, not in the record.** A conf change is in force from the moment its entry is
on disk; the record only moves when it applies — and the re-derived operator arrives at the *new*
leader in exactly that window, because the old one stepping down with the change in its log is why
PD re-derived at all. So the guard reads the routing table too: `learn_routes` fills it from the
change's own context in the `Ready` that persists the entry, on every peer that appends it,
precisely so a brand-new peer is addressable before the record moves. `RegionTransport::hosts_store`
asks it the question `store_of` answers backwards.

The same lesson ADR 0035 records one path over: *the core's membership decides whether the caller is
a stranger; the record decides when it is served.*

### What is red first

| test | red against | with |
|---|---|---|
| `a_re_issued_add_peer_does_not_place_a_second_peer_on_a_store_that_has_one` | the peer-id guard | assertion, `conf_change_for` proposed a second peer |
| `a_re_issued_add_learner_does_not_place_a_second_peer_on_a_store_that_has_one` | the peer-id guard | the same, through the columnar operator |
| `an_add_peer_for_a_peer_id_already_here_still_proposes_nothing` | — | **green throughout**, so widening the guard cannot lose the case it was written for |
| `promotion.rs`'s `one_peer_per_store` | the record-only guard | `region 3 has peers 16 and 27 both on store 3`, on the run after the first half landed |
| `a_peer_learned_at_append_already_counts_as_placed_on_its_store` | — | coverage for the new method; the red for that half is the row above |

### What is left, and it is a clock

At **two** copies rather than four: **5 of 6 green**, and the one failure is not a stranded learner
but the test's own load generator giving up —

```
writing b"k000551" never succeeded; the last refusal was: None
```

`None` is the whole of it: no store refused the write, no store answered as leader for that key for
ninety seconds. At four copies the same failure takes three of four runs and the passing runs
stretch from 88 s to 168 s. That is `docs/plans/phase-11-engine.md`'s rule reading true — *"the way
to tell them apart is that the failure does not survive being run alone"* — and it is starvation of
a machine running twelve stores and four drivers, not an ordering bug.

So the deadline stays where it is. **What changed is what the test says when it expires**: it now
asserts the state behind the two known causes *before* it times anything —

* **no region has two peers on one store**, which is the bug above, named where it happens rather
  than thirty seconds later; and
* **a peer PD reported as a voter is never a learner again**, because nothing here demotes one, so
  that is PD's view going backwards and not a promotion that failed;

and when it does expire it prints PD's own peer list, roles, epoch and leader beside the learner's
store's view, so the next sighting is diagnosable from the panic instead of from a re-run.

| configuration | before | after |
|---|---:|---:|
| alone, `RUST_LOG=…=debug` | 8 of 8 red before `548dd62` | **3 of 3 green** |
| 4 copies at once | **2 of 4 red**, both the duplicate peer | no duplicate peer in any run since |
| 2 copies at once, 3 batches | — | **5 of 6**, the sixth the load generator's own deadline |
| `cargo nextest run -p esker-store` | — | **299 of 299**, twice |

---

## U3 — `pd_wiring::an_alter_reports_every_range…` and `cluster_start`

### 3.1 `pd_wiring`: one fixture constant serving two tests with opposite needs

Twelve copies of the binary at once, four batches: **4 of 48 red**, all identical —

```
called `Result::unwrap()` on an `Err` value: SchemaLeaseExpired { command: "ALTER TABLE" }
```

which is ADR 0028 fail-closed working exactly as designed, in a test about *report content*.

`standin_pd` hands out `LEASE_MS = 600`, *"a short lease, so a lapse is a second of test rather
than five"* — which is what `a_lapsed_lease_refuses_writes_and_still_serves_reads` needs and the
exact opposite of what this one does. A node renews on a thread, and `redrive_live` spawns one for
this reason (*"the test runs for several lease terms, so the refresher thread is load-bearing"*).
This test **cannot**: a refresher asserts the whole columnar set on every renewal, and those
assertions are what it counts. So it holds the one lease it fetched at startup across a body of DDL
over three real stores, and on a loaded machine that body outlives 600 ms.

The lease is now per driver: `serve_with_lease(NO_LAPSE_MS)` for the tests that are not about
leases, `serve()` and `LEASE_MS` kept for the one that is. Not a widened deadline — the lapse is
still exactly `LEASE_MS` where a lapse is the subject.

| lease | runs | failed |
|---|---:|---:|
| 600 ms (the shared default) | 48 | **4** |
| **1 ms** | 5 | **5** |
| 3,600,000 ms (`NO_LAPSE_MS`) | 48 | **0** |

The 1 ms arm is the deterministic red: the same `SchemaLeaseExpired`, arriving at the first `CREATE
TABLE` instead of the fifth statement. One variable, three points, monotone.

### 3.2 `cluster_start`: not reproduced in 21 runs, and a trap found beside it

| configuration | runs | failed |
|---|---:|---:|
| alone | 3 | 0 |
| under a `cargo build --workspace --tests` loop from a detached worktree, load average ≈ 20 | 8 | 0 |
| with `esker-cli` **relinked before every run**, under that load | 4 | 0 |
| after the band change below | 6 | 0 |
| inside a full `just check` under that load | 1 | 0 |

The third row is the recorded scenario exactly — *"every time on the run right after a build"* — and
`c3b64d9`'s `warm_the_binary()` holds it at under a second. But that is not the whole story, because
the two phase-10 sightings **postdate** it: `c3b64d9` is 2026-09-01 18:05 and
`.config/nextest.toml` first exists in `c887382` at 20:05, so a sighting "before the CLI's binaries
were serialised" is already after the warm-up. The warm-up fixed the three failures it was written
for; it did not fix those two.

**What was found instead is a trap, and it is stated as one.** `free_port_run` binds a run of
ports, releases it and returns the base — a race the file's own `PORTS` mutex closes only for the
other test *in this file*, since a mutex is process-local. The band is therefore the real
mitigation, and this file scanned **30,100–40,000**, which swallows `tier_acceptance`'s
**31,000–39,000** entirely:

| test | band, before | after |
|---|---|---|
| `cluster_chaos` | 21,000–30,000 | unchanged |
| `cluster_start` | **30,100–40,000** | **30,100–31,000** |
| `tier_acceptance` | 31,000–39,000 | unchanged |
| `columnar_cluster` | 41,000–50,000 | unchanged |

`tier_acceptance` is `#[ignore]`d — it needs a MinIO container — so it does not run in the gate and
**cannot be what the two sightings were**. That is why this is recorded as a trap rather than
claimed as the diagnosis: run its three tests beside a workspace run, which is exactly what checking
phase 6b means, and the failure lands in `cluster_start` sixty seconds later in another crate with
nothing pointing back. An exhausted band now panics by name, which is the right failure: it says the
ports ran out, where a collision says nothing at all.

**The sighting stays open**, with the reproduction recipe written down: relink `esker-cli`, run
under a build loop, and read `sample`/`ps` on a store that has not registered to tell *"the OS has
not started it"* from *"it started and did not register"*. Twenty-one runs did not produce one to
read.

---

## 2. Units

| unit | test | done when |
|---|---|---|
| U0 | — | this file, committed |
| U1 | `redrive::two_re_drivers_racing_take_each_step_exactly_once` | deterministic repro, mechanism, fix, regression red-first, 50 of 50 green |
| U2 | `promotion::a_learner_on_a_fresh_store_becomes_a_voter_under_load` | the same, or the evidence that the remainder is a clock and the test made to watch an event |
| U3 | `cluster_start` and `pd_wiring::an_alter_reports_every_range…` | the same, for each |
| U4 | the gate | `just check` three times in a row under load, recorded below |

## 3. What this lane will NOT do

* **Not widen a deadline.** `PROMOTION_DEADLINE`, `cluster_start`'s 60 s and `redrive`'s 200-pass
  ceiling stay where they are unless the change is from a clock to an event, which is not the same
  thing.
* **Not serialise more test binaries.** `.config/nextest.toml` is untouched.
* **Not change `esker-raft`.** It is a pure state machine and none of these four reach it except
  through `esker-store`.
* **Not touch `crates/esker-sql/tests/corpus/`, `crates/esker-keys/`, `crates/esker-columnar/`**,
  and to keep out of the type lane's way in `esker-sql`, changes stay in the re-drive module and its
  test.
