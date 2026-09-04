# ADR 0056 — READ COMMITTED waits for the writer in front of it

Status: proposed · Date: 2026-09-04 · Phase 9 (Rails compatibility), the locking family

## Context

Run 54 measured **74 of 120 tests** in the transaction and locking family stopping on
`40001 could not serialize access due to concurrent update` where PostgreSQL simply waits and
proceeds: `locking_test.rb`, `transactions_test.rb`, `transaction_isolation_test.rb`, pessimistic
locking, counter caches, optimistic locking. It is the largest Rails-facing item left, and it is
not a missing feature — it is the wrong *isolation level*.

This node is Percolator: optimistic, snapshot-isolated, first-committer-wins. A writer that meets
another writer's lock backs off while the lock is alive, and when its budget runs out it answers
`40001`. That is a correct implementation of **REPEATABLE READ**, and PostgreSQL's default is
**READ COMMITTED**, under which a conflicting writer blocks and then re-evaluates.

Everything below was measured against PostgreSQL 19 (`esker-pg19`) with two `psql` sessions
interleaved by a sleep, not paraphrased from the manual.

### The contract, measured

| Situation | PostgreSQL 19 | Evidence |
|---|---|---|
| B updates a row A has locked and not committed | **blocks** until A ends | B's `UPDATE` returned 1.74 s after it was sent |
| …then A **commits** (`n` 10 → 11) | B re-evaluates on **A's** version | `n + 100` gave **111**, not 110 |
| …then A **rolls back** | B works from the original | `n + 100` gave **110** |
| A changes the column B's `WHERE` tests (`tag` `a` → `moved`), B is `WHERE tag = 'a'` | B **skips the row** | `n` stayed 10; B updated nothing |
| The same conflict under `REPEATABLE READ` | `40001 could not serialize access due to concurrent update` | raised at B's `UPDATE`, no wait |
| `SET LOCAL lock_timeout = '300ms'` | `55P03 canceling statement due to lock timeout` | with `CONTEXT: waiting for ShareLock on transaction …` |
| `SET LOCAL statement_timeout = '300ms'` | `57014 canceling statement due to statement timeout` | same `CONTEXT` |
| `SELECT … FOR UPDATE` against the held row | waits, then sees the **new** version | `11` |
| `SELECT … FOR UPDATE NOWAIT` | `55P03 could not obtain lock on row in relation "rc"` | a **different sentence** under the same code as the lock timeout |
| `SELECT … FOR UPDATE SKIP LOCKED` | skips to the next row | returned row 2 only |
| A plain `SELECT` | **never waits**, reads its own snapshot | `10` while the lock was held |
| Two transactions locking two rows in opposite orders | `40P01 deadlock detected`, one victim, the other proceeds | `DETAIL` names both processes and both transactions; the survivor's *both* updates landed |
| Under `READ COMMITTED`, two `SELECT`s in one transaction across another's commit | **each statement re-snapshots** | `10` then `99` |
| The same under `REPEATABLE READ` | the transaction's snapshot is fixed | `10` then `10` |

Two of those rows are the ones an implementation gets wrong by reasoning:

* **The arithmetic is on the new version.** `n + 100` over a row another transaction just moved
  from 10 to 11 is **111**. An implementation that waited and then applied its own already-computed
  value would answer 110 — a lost update wearing a successful commit.
* **A row that stops matching is skipped, not failed.** PostgreSQL re-evaluates the `WHERE` against
  the *new* version, and a row that no longer qualifies is simply not updated. That is
  EvalPlanQual, and it is why "wait, then apply" is not enough.

## Decision

**Under READ COMMITTED a write that meets a live lock waits, and then re-runs its statement at a
fresh read timestamp.** Three parts, and the third is what makes the first two small.

### 1. The wait is the client's existing one, given a deadline instead of a budget

`esker-client` already distinguishes the two cases this needs: `Classified::Alive { lease_ms }`
sleeps `min(backoff, lease)` and retries, and a settled lock is resolved. What ends the wait today
is a **retry budget**, and what should end it is a **deadline** — `lock_timeout`, then
`statement_timeout`, then never.

That crate is another lane's. So the deadline is applied where the settings live: the SQL write
path catches the conflict and re-drives, and the client's budget becomes the inner loop. The one
thing `esker-client` must eventually gain is written down here rather than reached for: a
`scan`/`prewrite` that takes a deadline instead of an attempt count, so a waiter does not have to
count round trips to know how long it has waited. Until then the SQL layer counts, and the ADR's
measure will say whether that costs anything visible.

### 2. The re-read is a statement re-execution, not a row-level EvalPlanQual

PostgreSQL re-evaluates **the conflicting row**. This node will re-run **the statement**, at a
fresh read timestamp, after rolling back to an implicit savepoint taken at the statement's start.

The machinery is already here: `exec::savepoint::Savepoints` records the before-image of every key
a statement writes, which is exactly the undo a re-run needs, and a statement-level implicit
savepoint is what PostgreSQL itself uses to make a failed statement not poison a transaction.

**Where it differs, stated rather than discovered.** For a statement whose conflict is its only
one — every case in the Rails family, and every row of the table above — re-execution is
observationally identical: re-running `n + 100` at a snapshot that includes A's commit gives 111,
and re-running `WHERE tag = 'a'` finds nothing. It differs for a statement that has already
updated *other* rows before it blocks: PostgreSQL keeps that work and re-evaluates only the
blocked row, where a re-run redoes all of it at a newer snapshot and can therefore see a third
transaction's commit on a row it had already passed. Both are legal READ COMMITTED — the level
promises only that each row is evaluated against *some* committed state — but they are not the
same execution, and a test that counts how many rows an `UPDATE` touched under three-way
contention can tell them apart. Named as a divergence with its own corpus row rather than left to
be found.

### 3. Isolation level becomes a session setting, and today's behaviour is what the other two get

This node has no isolation level at all. It is going to have three names for two behaviours:

* **READ COMMITTED** (the default, as on a real server): statement-level snapshots, and the wait.
* **REPEATABLE READ**: the transaction's snapshot, `40001` on a write-write conflict — which is
  **exactly what this node does today**, so the level that already works keeps working and the
  change cannot regress it.
* **SERIALIZABLE**: accepted and served as REPEATABLE READ, which is what snapshot isolation is.
  That is a real divergence — SI admits write skew and SSI does not — and it is declared rather
  than claimed, with the anomaly written into the corpus so nobody is surprised by it later.

A statement-level snapshot is a **fresh `start_ts` per statement** for reads, while the
transaction's writes stay buffered under one `start_ts` for the prewrite. That is the one piece of
Percolator this changes, and it is the piece to be careful about: a transaction's own writes must
be visible to its later statements whatever timestamp they read at, which is what the client's
buffer already guarantees by merging over the snapshot.

### Timeouts, and where the clock comes from

Invariant 6 says timestamps come only from PD's TSO. A deadline needs elapsed time, not an
instant, and the honest source is the same one the lock's own lease is measured against: the
**physical half of a TSO timestamp**. A waiter takes a fresh timestamp per attempt anyway, so
elapsed is `physical(now) − physical(start)` and a `lock_timeout` is directly comparable to a
lease. No wall clock enters `esker-txn`.

`statement_timeout` is currently refused for any non-zero value, because nothing here could cancel
a running statement. This unit is the first thing that can: a *waiter* is cancellable, being a
loop the SQL layer drives. So the refusal narrows rather than disappearing — a non-zero
`statement_timeout` becomes honoured **for a lock wait** and still refused for anything else,
which is a smaller lie than either accepting it wholesale or refusing it after this exists.

### Deadlock

**Node-local cycle detection, and no backstop that lies.** Each waiter records a wait-for edge
(`waiter start_ts → holder start_ts`) in a table on the `esker-sql` node; a cycle among the edges
is `40P01` and the youngest transaction in it is the victim. That catches every deadlock between
sessions on one node, which is every deadlock the Rails suite can make, and it costs one lock and
a walk of a graph whose size is the number of waiting sessions.

A cycle *across* nodes is not detected: those waiters block until a timeout. A timeout-based
backstop reporting `40P01` was considered and rejected — a slow but live transaction is not a
deadlock, and telling a client it was one is a wrong diagnosis that sends them looking for a cycle
that never existed. Cross-node detection needs a wait-for graph somewhere both nodes can see,
which is PD's job and is out of this lane; it is a named follow-on with the measure "a two-node
deadlock is reported as `40P01` rather than a timeout".

PostgreSQL waits `deadlock_timeout` (1 s, measured) *before* looking for a cycle, because looking
is expensive and most waits are short. The same applies here.

### What must not happen

* **A waiter must never resolve a lock whose lease is alive.** That steals a live transaction's
  row and looks like a successful commit — the one failure in this design that loses data rather
  than answering wrongly. `Classified::Alive` already refuses to, and the wait must go through it
  rather than around it.
* **The re-read must not move the transaction's snapshot for rows it is not about.** Under RC that
  is per *statement*, so a fresh timestamp for the re-run is right; under RR and SERIALIZABLE
  there is no re-run at all.
* **The barrier rule.** Every earlier racy test in this family gated on "thread started"; a
  two-session test here gates on the *transaction's edges* — prewrite done, commit done — because
  the thing being tested is what happens between them.

## Consequences

* `esker-sql` gains a per-statement implicit savepoint on the write path. It is cheap for a
  statement that writes nothing and proportional to the writes otherwise, which is what
  `Savepoints` already costs inside an explicit `SAVEPOINT`.
* A blocked writer holds a connection and a thread. This node runs a statement to completion on a
  blocking thread, so a wait is a thread parked — the same shape a real server has, at a much
  smaller connection budget. Worth a number in the plan.
* **SERIALIZABLE is snapshot isolation**, declared. Write skew is admitted where a real server
  refuses it.
* Statement re-execution can do more work than PostgreSQL's row-level re-evaluation under
  three-way contention, and can observe a third transaction's commit on a row it had already
  passed. Declared, with a corpus row.
* Today's `40001` becomes rare rather than absent: it is still what a **prewrite conflict** is at
  commit time under RC when the winner committed *after* this transaction's start and before its
  prewrite. PostgreSQL has no equivalent because it locks eagerly. This is the residue of
  optimism and it is what the measure will size.

## The file list

`crates/esker-txn/`: `percolator.rs` (the decision function — given a `Locked` and an isolation
level, `Wait` or `Conflict`; pure, no I/O), `error.rs`.
`crates/esker-sql/src/`: `exec/mod.rs` (the statement loop, the implicit savepoint, the deadline),
`exec/savepoint.rs`, `exec/dml.rs` and `exec/cursor.rs` (where a conflict surfaces),
`plan/session.rs` and `parse/lower.rs`'s `SET TRANSACTION` arm (the isolation level — **the one
file outside the lane's list**, and only its existing `SetTransaction` arm; if g1 or b4 is in it,
the level is carried on `SessionStatement` instead and the arm is left alone),
`parameter.rs` (the narrowed `statement_timeout` refusal), `error.rs`.
`crates/esker-sql/tests/`: new files only.
Docs: this ADR, `docs/DESIGN.md` §8, `docs/plans/phase-9-rails.md`.

## The test list

1. **A two-session interleaving per row of the table above**, gated on the transaction's edges —
   thirteen of them, including the two that reasoning gets wrong (the arithmetic on the new
   version, and the skipped row).
2. **Isolation levels**: the same conflict answers a wait under RC and `40001` under RR and
   SERIALIZABLE; `SET TRANSACTION ISOLATION LEVEL` and the session default are honoured; the
   statement-snapshot pair (`10` then `99` under RC, `10` then `10` under RR).
3. **The timeouts**: `lock_timeout` → `55P03` with PostgreSQL's sentence, `statement_timeout` →
   `57014`, and neither fires when the lock resolves first.
4. **Deadlock**: two sessions in opposite order → `40P01` to one victim, the other's work lands.
5. **Crash tests**: `kill -9` the waiter, the locker, and the node between the wait and the
   re-read. The invariant is that no lock outlives its lease and no row is resolved by a waiter
   while its owner is alive.
6. **A property test** over random interleavings of two writers, checking the outcome against the
   table: for every pair of statements and every commit/abort of the locker, the final row is the
   one PostgreSQL's rules give.
7. **The residue**: a corpus row for each declared divergence — SERIALIZABLE as SI, statement
   re-execution under three-way contention, and the prewrite-time `40001` that optimism leaves.

## The measure

Run 54: **74 of 120** tests in the locking family stopped on `40001`. r1 re-runs the family after
unit 4 and the number goes in `docs/plans/phase-9-rails.md` beside the before. A unit that moves it
by less than it costs in blocked threads is a unit to reconsider, and the plan row will say which
it was.
