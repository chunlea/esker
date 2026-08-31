# Debt wave, lane wy-c2: the chaos budget under load, and the orphaned schema job

Two pieces of debt, both of them liveness with a safety edge. Neither was fixed by documenting it.

## 1. `chaos_linearizability` exhausted its search under load

### What was actually wrong

The previous fix read the cost as superlinear in a history's **length** and raised `KEYS` from
three to five to make each history shorter. That diagnosis was wrong, and the wrongness was hidden
by the report: the exhaustion message printed `history.pending()`, which counts
`Completion::Pending`, and every operation in this test ends in `Completion::Unknown`. It printed
`(0 pending)` on every run, so the number the cost actually depends on was never on screen.

The search is exponential in how many operations are **unbounded** — the ones whose outcome nobody
learned. Their response time is infinite, so each overlaps everything after it and roughly doubles
the space the checker has to walk. And the test was recording *every* failed call as unbounded,
including the ones the store had refused outright. A three-second run with four leader kills
recorded **eleven acknowledged writes and two hundred and forty refusals**; fifty unbounded
operations in one key's history does not decide at any budget worth waiting for.

Under nextest's full-workspace parallelism the cluster spends longer with no leader, so it refuses
more, so the histories got worse — which is why it looked like a load problem. It was a modelling
problem that load made visible.

### The fix

`Error::changed_nothing()` already carries `esker-proto`'s verdict outwards, and the transaction
layer branches on the same call. The recorder now asks it:

* the store refused ⇒ the operation did not happen ⇒ **it is not in the history at all**;
* nobody learned ⇒ recorded, unbounded, exactly as before.

Operations are appended when their call *ends* rather than when it starts, because until then it is
not known whether there is anything to append. `KEYS` went back to three: five bought shorter
histories, which was never the constraint, and cost the collisions that make a history worth
checking.

Measured after: per-key histories of 39–54 operations deciding instantly, **1–6 unbounded per key**.
That bound is structural rather than lucky — a kill can strand at most one in-flight call per
client, so unbounded operations are bounded by clients × kills whatever the timing does.

### What it uncovered

The search then decided, and what it decided was that the histories were **not linearizable**.
Dropping a refused operation is sound exactly when a refusal is true, and it was not:
`esker-store` failed proposals already appended to the Raft log with `not_sent` — *"provably never
left this process"*, `RequestOutcome::NotApplied`, "safe to send again" — while a surviving majority
went on to commit them. Nine of thirty-two refused writes were in the database.

`crates/esker-client/tests/refusals.rs` is that property on its own, away from linearizability:
every write goes to a key nothing else will ever touch, so a landed write cannot be hidden by a
later one and the question becomes "does this key exist". Fixed by the `esker-store` lane in
`e06acbf`; the test is what says when it is fixed again.

### The client's half

The store fix turns those into `Closed`, which is `Unknown`, and a client has to tell a lost
*answer* from a refusal. It could not: both surfaced at once, so a read whose peer stopped
mid-call was handed back to the caller as a failure it would only have retried itself.
`retry::may_ask_again` is the rule `retry.rs`'s own module doc had named and left — a read may
always be re-sent, because asking again cannot change what the first attempt did; a write may not,
and is still `AmbiguousResult` on the first attempt and still never re-sent.

## 2. An orphaned schema job stalled until a human noticed

### The mechanism

Every node runs a `ReDriver` (`crates/esker-sql/src/exec/redrive.rs`). A pass scans the jobs in the
catalog and steps the ones that have gone idle. There is no lock and no election, because a step is
already a catalog transaction: two nodes that overlap write the same table record, first-committer
wins, the loser gets `40001` and looks again.

### "Idle" is counted in passes, not measured on a clock

A job is idle when its fingerprint — state, cursor, done — has not changed for a whole pass. The
pass period *is* the step interval, so a fingerprint that survives a pass has been still for at
least one interval, and the step that produced it happened at or before the previous pass.

Counted rather than timed, for two reasons. `CLAUDE.md` invariant 6 keeps wall clocks out of
ordering. And the job record carries no "stepped at" field: adding one would be a catalog format
change with a golden, to store a number only ever compared against a local duration. Reaching the
past by *token* needs neither, which is the lesson `esker-counting-oracle-has-no-wall-clock`
records.

The rule is one-sided in the safe direction. It can only be **late** — a job stepped a moment before
a pass sees it waits until the pass after next — and late is a slower schema change while early
breaks the two-version invariant the interval exists for.

### The interval, and the two defaults that go opposite ways

`Backend::schema_step_interval()` comes from the same PD answer the lease does, because PD computes
the interval *from* the lease and a node holding one without the other holds half an arithmetic. A
node with no lease source **writes** — "nobody is coordinating" is not a reason to stop. A node with
no interval source does not **step**, because stepping without an interval means inventing one, and
an invented interval that is short is the unsafety the number exists to prevent.

The `removal_extra_ms` term is respected as its own wait, and only before a removing change's last
step: `passes_to_wait` is `ceil((step_ms + extra) / step_ms)`, which is one pass for every ordinary
step and more only for that one.

### Two bugs the racing test found

Both were in code that predates this lane, and both are the kind that only appear with two drivers.

**A step decided earlier was taken later.** Eligibility is decided on the state a pass observed, but
`step_job` re-read the state and acted on whatever it found. Two racers that did not overlap in
time would therefore take *consecutive* transitions moments apart: each one legal on its own, and
together exactly the acceleration the interval forbids. Closed at both ends — `step_job` takes the
state the caller expected and answers `Overtaken` if it has moved, and `job::advance` re-checks
inside the transaction that writes, so an overlap conflicts and a follow does not proceed.

**A lost race unwound the whole change.** `adding_step` treated any backfill error as a data
failure and called `job::unwind`, walking every state backwards and forgetting the job. A `UNIQUE`
duplicate is a data failure; a `40001` is not. The backfill runs beside live traffic on purpose —
that is what running it at write-only buys — and `crates/esker-sql/src/exec/job.rs` says in as many
words that "a lost race is an ordinary conflict-and-retry". It was a conflict-and-destroy. On a
table busy enough to need `CONCURRENTLY`, that is a change that can never finish.

Two smaller ones came with them: a `forget` that lost a race was reported as the *step* having
failed, at the moment the index became public; and clearing a finished job's leftover record was
reported as a state transition, so an index could look like it had been stepped to `public` twice.
`Stepped` now distinguishes `Transition`, `Batch`, `Overtaken` and `Cleared`, which is what a
driver needs to know and what a human reading `esker_schema_step` was previously expected to infer
from a word.

### Wiring

`esker-sql`'s binary starts one per node on a thread of its own — a pass sleeps a step interval and
would hold a runtime worker for the whole of one. It is inert until this node holds a lease, and
nothing attaches one yet (`connect`'s `TODO(phase-6a)`), so today it starts, finds no interval and
waits. Wired and inert rather than absent and forgotten; the node logs which of the two it is.

Tests never spawn it. `pass()` is separate from `run()` precisely so the whole thing is testable
without a timer, which is the same reason `esker_schema_step` is a verb rather than a clock.

## Evidence

Taken at `666d32a` (at or past `a87e5ad`, so the `DriverPool::shutdown` fix is present) on a box
verified clean first — a twenty-one-hour-old orphaned `snapshot` test binary was eating a core
until it was killed, which means every timing number taken on this machine earlier in the day was
taken about a sixteenth short.

**Three consecutive full `--workspace` nextest runs, all green.**

| run | tests | result | wall |
|---|---|---|---|
| 1 | 2176 | 2176 passed, 0 failed, 30 skipped | 78.6 s |
| 2 | 2176 | 2176 passed, 0 failed, 30 skipped | 79.4 s |
| 3 | 2176 | 2176 passed, 0 failed, 30 skipped | 78.8 s |

No failures at all, mine or any other lane's. `chaos_linearizability` took 5.36 s, 5.77 s and
5.94 s under 2176-test saturation — the condition it used to exhaust in. All three in-gate
controls and all seven re-driver tests passed in every run.

**Forty fresh processes, standalone: 40 passed, 0 failed.** Per-key histories of 34–57
operations, and the number the search cost is exponential in, across all 120 key-histories:

```
 0 unbounded:   3      4 unbounded:  23
 1 unbounded:  16      5 unbounded:  14
 2 unbounded:  32      6 unbounded:   2
 3 unbounded:  30
```

**Worst case six.** Before the fix, a single key's history carried about fifty, which is the
difference between a search of 2^6 and one of 2^50 — and it is why the old failure was a cliff
rather than a slope. The distribution is also the structural claim holding up in practice: a kill
strands at most one in-flight call per client, so six clients over four kills bounds this whatever
the timing does, and nothing in 120 samples came near the bound.

**The controls were shown able to fail**, each by the mutation aimed at it:

| mutation | control that caught it |
|---|---|
| exhaustion returned as `Linearizable` (going blind) | `exhaustion_is_never_reported_as_a_violation` |
| a decided violation returned as `Exhausted` | `the_checker_still_catches_a_lost_write`, `a_grown_budget_still_reaches_the_decision` |
| `BUDGET_ATTEMPTS` 2 → 1 (no growth) | `a_grown_budget_still_reaches_the_decision` |

## Status

* `crates/esker-client/tests/chaos_linearizability.rs` — green, deciding, controls untouched.
* `crates/esker-client/tests/refusals.rs` — the store's promise, testable.
* `crates/esker-client/src/retry.rs`, `router.rs` — a lost read answer is asked again.
* `crates/esker-sql/src/exec/redrive.rs` — the re-driver, with seven tests in
  `crates/esker-sql/tests/redrive.rs`.
* ADR 0020's "automatic re-drive is future work" and `docs/plans/phase-6e.md` §10's matching
  paragraph are amended to describe what exists.

## What this lane did NOT do

* Touch `esker-store`, `esker-engine`, `esker-cli` or `esker-columnar`. The `not_sent` bug was
  reported to the lane that owns it with the analysis and the repro, and fixed there.
* Add a "stepped at" timestamp to the job record. It would be a catalog format change with a
  golden, and counting passes needs no such thing.
* Attach a PD lease to the `esker-sql` binary. That is `TODO(phase-6a)`'s, and inventing an
  interval to make the re-driver do something on this binary today would be exactly the guess the
  design refuses.
