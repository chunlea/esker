# ADR 0100 — A region between leaders waits on the caller's deadline, not on a count

Status: **proposed — the decision is refuted by its own measurement** (2026-09-09) · Numbered 0100
by the coordinator; 0099 is the highest on `main`. **Nothing in the client changes.** The shape this
ADR describes is real and is not what stands between the load that provoked it and an answer; the
measurement it deferred to found something larger, which is `docs/plans/debts-v1.1.md` #34.

## Context — a count standing where a deadline belongs

`esker_client::router::Router::call` bounds a call two ways at once. There is a **deadline** —
`ClientOptions::call_timeout`, ten seconds by default, and the caller's to set — and there is a
**count**, `RetryPolicy::max_retries`, eight fruitless retries. The count fires first, and for one
refusal it is the only thing that decides.

The count is not naive: it counts attempts *in a row that taught this client nothing*, and a
refusal carrying a newer epoch resets it (`Router::learned_a_newer_epoch`). That is the right rule
for the two answers it was designed around — a newer epoch is progress, and a leader hint is a
place to try next.

**A region between leaders says neither.** It answers `NotLeader` with no hint at an epoch that is
not moving, which is exactly what the budget calls fruitless, and the thing the caller is waiting
for is an election already under way. Measured on the fake clock, deterministically:

```text
a leaderless region: 9 calls, 2266ms   —  of a 10,000 ms deadline the caller set
```

**Twenty-three per cent of the time the caller allowed**, and the answer is
`08006 gave up after 9 attempts: peer is not the leader`.

That is not a hypothetical. `where_a_bulk_load_into_a_splitting_table_breaks`, re-measured on
2026-09-09 (`docs/plans/phase-16-mpp.md`), broke three rounds out of three, and two of them broke
exactly here:

| round | last batch that landed | how it broke |
|---|---|---|
| 1 | 6,000 rows @ 162 regions | `08006 … gave up after 10 attempts: peer is not the leader of region 655` |
| 2 | 750 rows @ 16 regions | `40003`, then a lock that cleared inside the retry after 2.7 s |
| 3 | 4,500 rows @ 120 regions | `08006 … gave up after 9 attempts: peer is not the leader` |

**And this is a shape this codebase has met before.** `Router`'s wait for PD was a fixed 310 ms
until it became the caller's deadline. The same sentence applies here: a count is a statement about
attempts, and what the caller has is time.

## Options

**(a) Leave the count.** A leaderless region fails at ~2.3 s with a typed error, and a client that
wants longer sets a bigger `call_timeout` — which today does nothing for this case, because the
count fires first regardless.

**(b) A fixed retry duration, separate from the caller's deadline.** "Retry a leaderless region for
up to N seconds." One number to tune, and it is the wrong number for at least one of the three
paths below.

**(c) The caller's deadline, with backoff, and no count for this refusal.** The loop already has a
deadline and already backs off; this removes the count as the thing that ends a call the deadline
has not ended. The count stays for refusals that *can* loop without progress and without an
election — a leader hint that keeps pointing at a peer that keeps refusing.

### What each does to the three paths that retry

| path | what a longer retry buys | what it costs |
|---|---|---|
| **RawKV** (`RawClient::get`/`put`) | a single key's call rides out an election instead of surfacing `08006` | a genuinely dead region makes every call take the full deadline before it says so |
| **TxnKV** (`Transaction::prewrite`/`commit`) | the case that matters most: **one refused prewrite fails the whole transaction**, and a bulk load's batch is one prewrite over many regions — the failure the table above is made of | a stalled region holds a transaction's locks for the length of the deadline, and a wound-wait victim waits behind it |
| **fragment** (`FragmentClient::evaluate`) | little: the SQL node's answer to a refusal is to read the rows at the same `ts`, which is a correct answer that is merely slower | **a longer retry delays a fallback that would already have answered** — this path wants the *shortest* budget of the three, which is the argument against (b) |

The asymmetry in that last row is the whole case for (c) over (b): the three paths do not want the
same number, and the caller is the one that knows which path it is.

## The measurement this was deferred to, and what it found

The criterion was written down **before the numbers**, so that reading them could not choose it: if
the distribution sat well inside the default deadline, (c) turns a hard failure into a slower
success; if it had a long tail, the answer was a bounded (b) instead.

`how_long_a_writer_waits_for_a_region_between_leaders` asks it the way a caller experiences it: when
a statement is refused with `not the leader`, ask again until it lands, and record how long the
deadline would have had to be. Four runs on a quiet box, loading a table that splits under itself:

| run | regions | sightings | how long each stayed refused |
|---|---|---|---|
| 1 | 325 | 3 | 30.8 s · 31.5 s · 31.8 s |
| 2 | 192 | 6 | 30.2 s · 30.4 s · 30.7 s · 31.4 s · 31.6 s · 31.7 s |
| 3 | 252 | 4 | 30.0 s · 30.0 s · 30.9 s · 33.4 s |
| 4 | 276 | 1 | 30.8 s |

**Fourteen sightings, and not one recovered.** Thirty seconds is where the instrument gives up, so
every number in that table is a floor and not a measurement of the window — the window is longer
than the experiment. Seven of the fourteen name **region 1**, the original whole-key-space region.

## Decision — **refuted, and the ADR stays for the reason it was refuted**

**Neither (b) nor (c).** The client is unchanged.

The pre-registered criterion answers itself: at these region counts a region that loses its leader
does not get one back inside thirty seconds, so spending the caller's whole ten seconds would turn
a 2.3-second failure into a ten-second failure and not into a success. **The count is not what
stands between that load and an answer.** Adopting (c) here would have looked like a fix, shipped a
slower failure, and left the thing that actually breaks the load untouched — which is the argument
this file exists to record.

The shape stays true and stays written down: a count *is* standing where a deadline belongs, and
`what_a_leaderless_region_costs_a_caller` (`crates/esker-client/tests/sim_retry.rs`) prints what it
costs — **9 calls, 2,266 ms of a 10,000 ms deadline** — and pins today's behaviour so that a future
change to it is deliberate. If the leaderless windows below ever come down to something a deadline
could cover, this ADR is where the option list already is.

**What the measurement opened instead** is `docs/plans/debts-v1.1.md` #34: at 192–325 regions a
region can be without a leader for more than thirty seconds, repeatedly region 1, and whether that
is the system or a four-store in-process harness driving three hundred Raft groups is **not
separated**. Two reproducers are kept for it:
`a_splitting_bulk_load_never_fails_for_want_of_attempts` and the measurement above, both `#[ignore]`d
in `crates/esker-sql/tests/routing_differential.rs`.

## Consequences, if it is ever adopted

* **`RetriesExhausted` stops being reachable for `NotLeader` without a hint**, so the error a
  caller sees for a leaderless region becomes `DeadlineExceeded` — a different variant, and any
  matcher on the old one has to be found. `git grep RetriesExhausted` is the list.
* **A bulk load into a splitting table becomes slower rather than broken**, which is what the
  re-measure wants; it does not become correct, because `40003` is still reachable and is a
  different mechanism with its own record.
* **The deadline becomes the only bound on this path**, so a caller that sets no deadline at all
  would wait for ever. `ClientOptions::call_timeout` has a default and this ADR does not remove it.
