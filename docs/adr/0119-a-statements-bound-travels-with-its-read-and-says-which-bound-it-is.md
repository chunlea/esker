# 0119 — A statement's bound travels with its read, and says which bound it is

Status: **Proposed**, 2026-09-17 — debt [#115](../plans/debts-v1.1.md), the number issued by the
coordinator. **This page stops at the design.** Nothing here is built; what follows is where the
bound has to travel, what it costs at each seam, and the tests that would hold it.

## ① Context — two waits, and the bound is on the other one

[#115](../plans/debts-v1.1.md) began as "a reader waiting on a live lock does not honour
`statement_timeout`". The observation is right and the attribution was wrong: **the bound is not
missing, it is installed on a path a `SELECT` never takes.**

| Wait | Where | What ends it |
|---|---|---|
| **Row lock, write path** | `esker-sql/src/exec/mod.rs:413` `wait_for_the_lock`, reached through `lock_or_restart` → `Txn::lock` (`:394-409`) | The lock comes free, a cycle is found, `lock_timeout`, or `statement_timeout` — the deadline is checked at `:504-508` |
| **A read that meets a lock** | `esker-client/src/txn.rs:2157` `Transaction::resolve`, the `Classified::Alive` arm at `:2175-2188` | **The lease, and nothing else.** `Deadline` does not exist in this crate |

The client's own comment says what it is doing — *"Its owner is inside its lease. Waiting is the
whole answer"* (`txn.rs:2167-2169`) — and on the last resolution attempt the sleep is
`if last { lease_ms } else { backoff_ms(attempt).min(lease_ms) }` (`:2180-2185`). **On the last
look that is the whole remaining lease**, so a lock with a 24-hour lease is a 24-hour sleep.

`lock_deadline()` (`exec/mod.rs:1478`) and `deadline_from` (`:572`) are both correct:
`SET statement_timeout='10s'` yields `Some((10_000, Deadline::Statement))`. It simply never reaches
the read.

**Five measurements, one mechanism** (`esker-coord/s1-fill-window-2026-09-16/q115-design.md` §3): a
600 s lease took ~565 s; that round totalled 610.9 s; a 10 s lease returns in ~10 s; a 24-hour lease
had not returned after 300 s; an expired lock returns in 113 ms because it is `Settled` and never
enters the wait. The client-level characterisation test committed with #115 prints the number
directly: **86,400,001 ms slept against a lease of 86,400,000**, for one read.

## ② Decision

**The deadline a statement is given travels with that statement into the client, carrying which
deadline it is; the read-side wait is truncated by it; and expiry comes back as an error the SQL
side maps to the right SQLSTATE.**

Three parts, and the third is the one that is easy to drop:

1. **The bound travels.** `(millis, which)` — not a bare duration.
2. **The wait is truncated, not replaced — and every round of it, not only the last.**
   `resolve`'s `Alive` arm sleeps `min(backoff_ms(attempt), lease_ms, remaining_budget)`, where the
   last attempt's `backoff` term is the whole remaining lease (`txn.rs:2180-2185`). **Bounding only
   the last round would leave the budget beatable by arithmetic**: `call_resolving` runs
   `max_lock_resolutions` rounds (`txn.rs:2099`), and their backoffs sum, so a budget checked once at
   the end can be overshot before it is ever consulted. The budget is spent down across the rounds,
   and when it is what ran out, `resolve` returns instead of looping.
3. **Provenance survives the crate boundary.** `statement_timeout` → `57014`, `lock_timeout` →
   `55P03`, a cancellation → `57014` with the cancel sentence. These are **different answers to the
   client application**, and `esker-sql/src/error.rs:499` records the distinction as measured:
   *"`57014`, not `55P03`, even when what it was doing was waiting for a row."*

**This repository has already paid once for losing that provenance.** `exec/mod.rs:540-543` records
that `lock_deadline` used to return a bare `Option<u64>`, and the wait loop then reported `55P03`
"whichever parameter produced it". **A bound handed across a crate boundary without its provenance
repeats that bug one layer down.**

**The default does not change.** Both parameters boot at `"0"` (`parameter.rs:201`, `:248`), which
means no deadline, and a read that waits out a lease under the default is **correct Percolator
behaviour** — the debt is that a user who asks for a bound does not get one, not that waiting is
wrong. This is why #115 explicitly rejected giving the read wait a constant budget of its own: that
would turn waits which succeed today into failures.

## ③ The change surface

### The seam already exists, one notch narrower than it needs to be

`esker_client::Transaction::begin_statement(&mut self, read_ts: u64)` (`esker-client/src/txn.rs:1068`)
is called from `StoreTxn::begin_statement` (`esker-sql/src/backend/store.rs:392`). **A statement
boundary already runs end to end and already carries a per-statement value.** The work is to widen
it, not to invent a channel.

### Three ways to get the bound to the read, with their real sizes

`Txn` (`esker-sql/src/backend/mod.rs:202`) has **26 methods and no defaults** (ADR 0105), and **five
implementations**: `MemoryTxn` (`mod.rs:878`), `StoreTxn` (`store.rs:271`), `Recording`
(`exec/savepoint.rs:276`), and two test doubles — `GatedTxn` (`tests/redrive.rs:354`) and
`ProfiledTxn` (`tests/cluster/profile.rs:264`).

| | Shape | Call sites to touch | Notes |
|---|---|---|---|
| **(A)** A parameter on the read methods | `get`, `scan`, `get_without_waiting` | **88** in `esker-sql/src` alone (56 + 30 + 2) | Every reader in the crate learns about deadlines to pass one through |
| **(B)** A context object on those methods | as above, but future fields are free | same 88 now | Pays (A)'s cost once and then stops; still teaches 88 sites about a concern that is not theirs |
| **(C, recommended)** The statement boundary carries it | `begin_statement` gains the bound, or a `statement_deadline` setter sits beside it | **18** call sites, **7** implementations | The bound is **per statement**, which is exactly what this seam already is; readers stay unchanged |

**(C) is recommended**, and not only for the 5× difference: a deadline is a property of the statement
and not of each read, so hanging it on the read methods would put it in the wrong place even if it
were free. The trait already has the statement-scoped family to join — `begin_statement`,
`restart_statement`, `changed_since_statement`, `validate_reads`.

**One caution for whoever builds it**: `restart_statement` exists because a statement can be
re-run, and a re-run must **not** restart the budget — otherwise a statement that keeps restarting
never expires. The budget's origin instant belongs to the statement, not to the attempt.

### The clock has to express a truncated sleep

`Clock` (`esker-client/src/clock.rs:17`) is `now()` + `sleep(&self, Duration)`. Truncation needs the
sleeper to learn **whether it slept the whole duration**, so the caller can tell "the lease ended"
from "the budget ran out". Two shapes, both small:

- `fn sleep_bounded(&self, want: Duration, limit: Duration) -> Slept` — the clock does the `min` and
  says which one it used;
- keep `sleep` and have `resolve` compute `min` itself, with the clock unchanged.

The second is smaller but leaves the *cancellation* half (a `pg_cancel_backend` while asleep) with
nowhere to live. **Since this page merges (1) and (3), the first shape is the one that serves both**:
a bounded sleep is one that can also be cut short by a token.

`FakeClock` (`:54`) records sleeps (`sleeps_ms()`, `:85`) — **it must be able to express "truncated
at X"**, or the tests of ⑤ can assert only what was asked for and not what was honoured.

**The cancellation token's type belongs to `esker-client`.** `esker-sql`'s `cancel`
(`exec/mod.rs:442` calls `cancel::check()` at the top of the row-lock loop) is where the *signal*
comes from, but the dependency runs one way: `esker-client` does not and must not know about
`esker-sql`. So the token is defined here — beside `Clock`, which is already this crate's seam for
"something outside decides when this stops" — and the SQL side **injects** it at the statement
boundary along with the bound. A token typed in `esker-sql` would invert the dependency and make the
client unbuildable without the SQL crate.

### The error variant and its blast radius

A new client error — `Error::DeadlineExpired { which }` or similar — joins `Error::LockNotCleared`,
whose name appears **26 times across 6 files** today (`esker-client` tests, `esker-sql`'s
`backend/store.rs` and `catalog/record.rs`, `routing_differential.rs`). That is the order of
magnitude to expect, and `store.rs` is where the mapping to `SqlError::StatementTimeout` /
`SqlError::LockTimeout` belongs, because that is where client errors already become SQL ones.

## ④ What this does not do

- **No default budget for the read wait.** Rejected with #115: `statement_timeout = 0` means
  unbounded, and a reader waiting for a lock is Percolator working, not failing.
- **No wire change**, no `TxnKvResp` field, nothing in `esker-store`. The bound is local to the node
  and its client.
- **No change to the write path's wait.** `wait_for_the_lock` already honours the deadline; this page
  gives the read path the same property, it does not redesign the one that works.

## ⑤ Tests

**(a) The characterisation test flips, and that is the acceptance criterion.**
`crates/esker-client/tests/a_read_waits_out_a_live_lease.rs` asserts today's behaviour — the client
sleeps the remaining lease — and its module doc says it goes red when #115 is fixed. **Rewriting it
is part of this unit**: the assertion becomes *the sleep equals the budget, not the lease*, with the
same `FakeClock` and the same printed line. A fix that leaves that test untouched has not been
integrated.

**(b) Three SQL-level tests, each bounded at the thread.** The statement runs on its own thread,
joined with a 30 s deadline; **after the assertion another thread releases the lock** so the blocked
statement exits and the cluster can be stopped. Without that recovery the red test hangs the gate,
which is the failure this whole debt is about.

| Test | Sets | Expects |
|---|---|---|
| 1 | `statement_timeout = '2s'` | `57014` within a few seconds, not the lease |
| 2 | `lock_timeout = '2s'` | `55P03` — the **different** SQLSTATE is the point |
| 3 | neither; `pg_cancel_backend` from another session | `57014` with the cancel sentence |

Test 2 is the one that would catch a fix that passes a bare duration: with only test 1, a mapping
that always answers `57014` looks correct.

## ⑥ Relation to #86, #88 and ADR 0117

**Adjacent, not the same.** #86 / #88 / [ADR 0117](0117-a-fragments-refusal-carries-the-keys-that-stopped-it.md)
are about a fragment **refusing** when it meets a lock, and the planner falling back to the row path.
This page is about what the row path then does with that lock.

- **0117's options do not fix #115.** Carrying the keys back still leaves the client resolving them,
  and resolving a live lock still sleeps the lease.
- **#115 does not change #88's 7.29 s either — as long as the default stays unbounded**, which ②
  keeps. #88's row carries the condition: if a default bound is ever put on that wait, the number has
  to be measured again.
- The counter of [ADR 0118](0118-a-counter-is-placed-at-a-door-and-counts-events.md) is unaffected:
  it counts at the store's refusal point, before any of this.

## ⑦ Size

| | |
|---|---|
| Crates | 2 — `esker-sql` (the boundary and the mapping), `esker-client` (the truncation) |
| Signature changes | **1** trait method widened or added, **7** implementations, **18** call sites |
| New surface | one `Clock` method, one error variant, one mapping arm |
| Tests | 1 rewritten (a) + 3 new (b) |
| Windows | **two**: one to build the seam and flip (a); one for the three SQL-level tests, which need a cluster and a recovery path each |
| Risk | Low on the wire and the store, which are untouched. The real risk is the budget's origin instant surviving `restart_statement` — named in ③ so it is not discovered in a window |

**This page stops at the design.**
