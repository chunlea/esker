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
