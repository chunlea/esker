# 0114 — A unique key being written waits at READ COMMITTED

Status: **Proposed**, 2026-09-13 — debt #90, the number reserved for the s1-sql lane. **§1 is built**
on `sql-gaps`. **§2 is a wire and Raft-log format change** and waits for the user (`CLAUDE.md`,
"ask before doing"). **§3 is a question**, because the premise it was issued with did not survive the
measurement. Builds on [ADR 0057](0057-read-committed-waits-for-the-writer-in-front-of-it.md) (the
wait, the statement re-run, the per-key read timestamp) and
[ADR 0088](0088-a-row-lock-across-nodes.md) (the eager row lock), beside
[ADR 0062](0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md) and
[ADR 0104](0104-where-a-conflict-becomes-40001-and-where-40p01.md).

## Context

Run 127 attempt 6 finished the Rails suite on the real topology, and its one divergence from the
fake-backend baseline was `relations_test.rb`: both of `CreateOrFindByWithinTransactions`' tests,

```text
test_multiple_find_or_create_by_within_transactions
test_multiple_find_or_create_by_bang_within_transactions
ActiveRecord::SerializationFailure: PG::TRSerializationFailure: ERROR:  could not serialize
  access due to concurrent update: a commit at 469047227545812992 beat this transaction at …
```

The tests race two threads on purpose. Each runs `Subscriber.transaction { find_or_create_by(nick:
"bob") }`, which in Rails 8.1 is `find_by(attributes) || create_or_find_by(attributes)`, and
`create_or_find_by` is a `SAVEPOINT`, an `INSERT`, and — on `RecordNotUnique` and nothing else — a
`ROLLBACK TO SAVEPOINT` and `where(attributes).lock.find_by!`, which is a `SELECT … FOR UPDATE`. The
table is Rails' `subscribers`: `id: false`, **no primary key**, and a unique index on `nick`.
PostgreSQL 19's own log of the file shows both tests sending the same statements, and the file
passes there: `2 runs, 4 assertions, 0 failures, 0 errors`.

So two things have to be true, in this order: the second `INSERT` of `bob` fails **as a duplicate,
at the `INSERT`**, where the rescue can see it; and the rescue's `FOR UPDATE` of the row the first
thread committed **takes its lock**.

### What PostgreSQL does with the second insert, measured

Two `psql` sessions on 19beta1, interleaved by `pg_sleep`, one table shaped like `subscribers`
(`esker-coord/s1-oracle-2026-09-13/d/`). A inserts `bob` and holds it for 2.0 s; B has read that
there is no `bob` and then inserts it. *Waited* is from B's `INSERT` being sent to its answer, and
every answer that waited arrived within 10 ms of A's `COMMIT` or `ROLLBACK`.

| case | level | the holder | B's `INSERT` | waited |
|---|---|---|---|---|
| 01 | READ COMMITTED, Rails' own sequence | commits | `23505`, then `ROLLBACK TO SAVEPOINT`, a `FOR UPDATE` that returns `bob`, `COMMIT` | 1.32 s |
| 02 | READ COMMITTED | rolls back | `INSERT 0 1`, `COMMIT` | 1.31 s |
| 03 | READ COMMITTED | committed before B's `INSERT` | `23505` | — |
| 04 | REPEATABLE READ | commits | `23505` | 1.33 s |
| 05 | REPEATABLE READ | rolls back | `INSERT 0 1` | 1.30 s |
| 10 | REPEATABLE READ | committed before | `23505` | — |
| 06 | SERIALIZABLE | commits | `40001 could not serialize access due to read/write dependencies among transactions` (`Canceled on identification as a pivot, during write`) | 1.32 s |
| 07 | SERIALIZABLE, **B never read `bob`** | commits | `23505` | 1.31 s |
| 08 | SERIALIZABLE | rolls back | `INSERT 0 1` | 1.31 s |
| 09 | SERIALIZABLE | committed before | `40001 … read/write dependencies …` | — |
| 11 | READ COMMITTED, `ON CONFLICT (nick) DO NOTHING` | commits | `INSERT 0 0` | 1.31 s |
| 12 | READ COMMITTED, `ON CONFLICT (nick) DO UPDATE` | commits | `INSERT 0 1`, the update lands on A's row | 1.32 s |
| 13 | SERIALIZABLE, `ON CONFLICT (nick) DO NOTHING` | commits | `40001 could not serialize access due to concurrent update` | 1.33 s |

* **The wait is every level's.** A uniqueness check is not a snapshot read on PostgreSQL: it sees an
  in-progress insert of the same key and waits for that transaction to end, whatever the level.
* **What follows the wait is the level's.** A holder that rolled back leaves nothing and the insert
  goes through at all three. A holder that committed is `23505` at READ COMMITTED and REPEATABLE
  READ; at SERIALIZABLE it is `40001` when B had read the key and `23505` when it had not (06
  against 07).

### What PostgreSQL does with the `FOR UPDATE`, measured

B begins and runs one statement; A then changes the table in its own transaction and commits; then B
locks the row (`esker-coord/s1-oracle-2026-09-13/e/`):

| case | level | what A committed | B's `SELECT n … FOR UPDATE` |
|---|---|---|---|
| e1 | READ COMMITTED | an `UPDATE` of row 1, `n` 10 → 11 | `11` |
| e2 | READ COMMITTED | an `INSERT` of row 2 | `20` |
| e3 | REPEATABLE READ | the same `UPDATE` | `40001 could not serialize access due to concurrent update` |
| e4 | REPEATABLE READ | the same `INSERT` | no row |

Under READ COMMITTED a commit that landed before the locking statement began is that statement's
**input**: the lock is taken on the version the statement reads.

## What the node did, measured

`crates/esker-sql/tests/concurrent_unique_insert.rs` against three real stores, on the tree before
§1 (`sql-gaps` at `bc90eb5e`):

* **READ COMMITTED, the holder live and then committing**: B's `INSERT` answered at once, `INSERT 0
  1`; B's `COMMIT` was refused `23505 duplicate key value violates unique constraint
  "index_subscribers_on_nick"` — the prewrite's conflict on the entry, renamed by
  `Executor::explain_conflict`. The right code, in the one place `create_or_find_by` cannot rescue.
* **READ COMMITTED, the holder rolling back**: B's `INSERT` answered at once and committed. PostgreSQL's
  outcome without PostgreSQL's wait.
* **`relations_test.rb`'s duel, statement for statement**: A committed; B was refused `23505` at its
  `COMMIT`.
* **SERIALIZABLE, B having read the key, A committed before B's `INSERT`**: `23505` at `COMMIT`.
* In process, `tests/insert.rs`: a second insert of a held unique value went straight through.

The cause of the first three is in `exec::dml::write_row`: it takes ADR 0057's row lock on the **row
key** and then, for each by-value unique entry, reads the entry's key at the statement's snapshot,
requires it absent and puts it — **with no lock**. A table whose row key is its declared primary key
already waits, because the row key *is* that key
(`tests/insert.rs::a_concurrent_duplicate_primary_key_is_also_a_duplicate`). A table whose row key
is an internal row id collides on nothing but its unique indexes.

**And after §1, the duel still failed — with attempt 6's text.** The second insert waited and was
`23505` at the `INSERT`; the rescue's `SELECT … FOR UPDATE` was refused

```text
40001 could not serialize access due to concurrent update: a commit at 1015 beat this transaction at 1004
```

which is the message the field recorded. The node before §1 can reach that message only through the
rescue — only when B's `INSERT` already sees A's commit, which is case 03's interleaving — so that is
what happened on the real topology: A's commit landed before B's `INSERT` ran, and the lock refused
B. The in-process cluster answers fast enough for A's commit to land after B's `INSERT`, which is why
the test met the other half first. **Both halves are #90**, and §1 alone does not make the tests pass.

## Decision

### §1 — at READ COMMITTED, a unique entry is locked before it is read (built)

**A write that adds a by-value entry to a unique index takes the row lock on that entry's key before
it reads it** — the same `exec::wait_for_row` ADR 0057 takes on a row key, with the same reach,
`Reach::Node`. Everything after the lock is machinery that already exists:

* **another transaction of this node holds the key** → the statement waits, bounded as any row wait
  is (below);
* **the lock is taken after a wait** → `Txn::restart_statement` and a re-run at a fresh snapshot,
  whose read of the entry finds what the holder left: its committed entry, which is `23505` from the
  statement (case 01); or nothing, and the insert goes through (case 02);
* **the lock is free at once but the key has a commit newer than the statement's snapshot**
  (`Txn::changed_since_statement`) → the same re-run, and the same `23505` (case 03).

**REPEATABLE READ and SERIALIZABLE take no lock on the entry.** They keep one snapshot and have no
re-run to wait for, so ADR 0057's loop would answer `40001` the moment it met a live holder — and a
holder that then rolls back would cost a refusal PostgreSQL never gives (cases 05 and 08). Their
conflict stays where it was decided before, at prewrite.

The paths it reaches are every caller of `write_row`: `INSERT`, the insert arm of `ON CONFLICT` and
its `DO UPDATE` rewrite (so cases 11 and 12 now wait, re-run, and find the row to skip or update), an
`UPDATE` through `rewrite_row`, a cascade, and a backfill.

### §2 — the eager row lock is taken at the statement's snapshot (proposed — a format change)

`SELECT … FOR UPDATE` takes ADR 0088's eager lock: `esker_client::Transaction::lock` prewrites the
key at once, and a key that is not in the write buffer goes out as `TxnMutation::Check { key }`
(`mutations_for`). **A `Check` carries no read timestamp.** The store gives it the request's
`start_ts` (`crates/esker-store/src/txnkv.rs:623`), and `esker_txn`'s prewrite refuses any key with a
commit newer than that. So under READ COMMITTED every `FOR UPDATE` of a row another transaction
committed after this one **began** is refused `40001`, where PostgreSQL locks the version the
statement reads (e1, e2).

That is not #90's alone. It is `lock!` or `with_lock` inside any block that started before somebody
else's update of the row. `MemoryBackend` takes no Percolator lock, so no in-process test can see it;
ADR 0057 §5's *"this transaction's prewrite validates the key at that statement's `read_ts`"* was true
of the lock before ADR 0088 moved it into the store, and a `Check` has had no timestamp to carry it
since.

**The proposal is a `Check` that carries the statement's read timestamp, as an additive tag on the
wire (`TxnMutation`) and an additive kind in the Raft log (`TxnWrite`)** — exactly the shape ADR 0057
§4 took for `Put` and `Delete` with tags 3 and 4:

* every existing golden stays byte-identical, and an older peer refuses the new tag rather than
  misreading it; the wire golden (`esker-proto/tests/golden/messages.hex`) and the log golden gain
  their lines;
* the store validates it as it validates a `Put` with a read timestamp: no commit on the key after
  **that** timestamp;
* the client sends it for an eager lock when a statement timestamp is set — which READ COMMITTED sets
  and the other two levels do not, so e3 stays `40001` — and the read-set `Check` a SERIALIZABLE
  commit sends keeps tag 5 at `start_ts` (ADR 0062 §3).

**Not built, and not this lane's to build without a yes**: it changes a wire format and a replicated
log format that both have goldens. What exists is the red test.

### §3 — SERIALIZABLE (a question)

The unit was issued as *"SERIALIZABLE keeps its `40001`"*. The node never gave one: a unique conflict
at SERIALIZABLE is `23505`, at `COMMIT`. PostgreSQL gives `40001` at the `INSERT` when the
transaction had read the key and `23505` when it had not (06, 09 against 07). Three answers are
available and none is built:

1. **Declare it** as it stands: the right code for case 07, the wrong one for 06 and 09, and at
   `COMMIT` rather than the `INSERT` for all three.
2. **`40001` when the lost key was read by an earlier statement**, `23505` otherwise — PostgreSQL's
   rule, in the SQL layer alone. It needs the read set to tell a statement's earlier read from the
   `INSERT`'s own uniqueness probe, which reads the same key.
3. **`40001` for every unique conflict found at `COMMIT` under SERIALIZABLE** — simplest, and wrong
   for case 07.

(2) is the recommendation; the red test asserts case 09.

## Options

### Rejected: lock the entry at every level, as the row key is

Uniform, and wrong in both directions at the two levels that keep a snapshot: `wait_for_the_lock`'s
`Held` arm is an immediate `40001` there, so a holder that rolls back becomes a refusal (cases 05 and
08), and a holder that commits turns REPEATABLE READ's `23505` into `40001` (case 04).

### Rejected: only rename the commit-time conflict

`Executor::explain_conflict` already turns a lost race on a unique entry into `23505`, and that was
the node's answer before §1. It changes the **code** and not the **place**: the rescue is around the
`INSERT`.

### Rejected for now: a store-side lock on the entry

`Reach::Cluster` would make §1's wait visible to sessions of other nodes, at a prewrite round trip per
written entry. One node is the suite's shape; a second node's writer still meets the key at prewrite,
first committer wins, as it did. It is the follow-on if two-node uniqueness under contention is ever
measured to matter.

### Rejected: take §2's lock as a `Put` of the row's current value

Tags 3 and 4 already carry a read timestamp, so writing the row back unchanged would be validated at
the statement's snapshot with no format change. But a `FOR UPDATE` would then commit a **new version
of the row** instead of a lock record — history, the columnar copy and collection would all see a
write the statement never made.

## The wait, and its upper bound

§1's wait is the row wait, unchanged (`exec::wait_for_the_lock`): a poll every `WAIT_STEP_MS` (2 ms),
ended by the lock coming free, by `lock_timeout` (`55P03`), by `statement_timeout` or a cancel
(`57014`), or by a cycle in this node's wait-for graph (`40P01`, the youngest transaction the
victim). With neither timeout set it is **unbounded, which is PostgreSQL's default too**
(`lock_timeout = 0`, measured in ADR 0057). A statement that restarts more than
`MAX_STATEMENT_RESTARTS` (32) times is behind a queue that keeps refilling and is refused `55P03`. A
holder that disappears gives its locks back with its session
(`store_locking.rs::a_dropped_session_gives_its_row_locks_back_against_real_stores`).

## How §1 sits with the rest of the transaction machinery

* **Percolator's lock resolution is not involved.** A write takes no store lock until its commit's
  prewrite (`Reach::Node`), so this wait is between two sessions of one node's `RowLocks` table and
  never reaches `wound_or_wait` or `MAX_LOCK_RESOLUTIONS`. Nothing in `esker-client`, `esker-store`
  or `esker-txn` changes.
* **The per-key read timestamp is what lets the waiter commit** (ADR 0057 §4). The re-run writes the
  entry under the re-run's snapshot, so a waiter whose holder rolled back prewrites a key it does not
  conflict on.
* **Savepoints give the lock back.** `savepoint::Recording::lock` records a key the savepoint took, so
  `ROLLBACK TO SAVEPOINT` releases the entry's lock (ADR 0104 §5) — which lets the rescue path, and any
  third session queued on the same value, go on.
* **ADR 0062's read set is untouched**: the lock is not taken at SERIALIZABLE.
* **No format, no wire, no log.** `RowLocks` is one process's memory and `Txn::lock` exists.

## Consequences

* **Cost, at READ COMMITTED only**: one more entry in this node's lock table per by-value unique entry
  per written row, held to the end of the transaction, and — when the lock is free at once — one
  `LatestCommit` round trip for `changed_since_statement`. It is what the row key already pays per
  row, now paid per unique index as well.
* **A deadlock the node used to miss is now found.** Two transactions inserting two unique values in
  opposite orders wait for each other and one is told `40P01`, as on PostgreSQL; both used to reach
  `COMMIT`, where one of them lost.
* **Two tests pinned the old contract and are rewritten, not deleted.**
  `tests/insert.rs::a_concurrent_duplicate_is_reported_as_a_duplicate` and
  `tests/real_backend.rs::a_lost_race_on_a_unique_index_is_a_duplicate_key` each insert one unique
  value from two sessions **on one thread** at READ COMMITTED; with §1 the second insert would wait
  for a first its own thread must commit, and hang. Their lesson — a lost race on a unique entry is
  the `23505` it is — still holds wherever the race still happens, so they run at REPEATABLE READ, and
  `tests/insert.rs::a_concurrent_duplicate_of_a_unique_value_waits_at_read_committed` is the bounded
  wait beside them, the way ADR 0057 rewrote the primary-key test.
* **`crate::backend`'s module doc** and `docs/DESIGN.md` §8 say which levels a concurrent duplicate
  still meets at prewrite.
* **§2 and §3 have red tests, and they are kept off `main`** until the two are ruled on, because a
  red test cannot land: they are written, run and handed over with s1-sql's handover of 2026-09-13.

## What stays declared

* **REPEATABLE READ and SERIALIZABLE do not wait** for a live holder of a unique value. PostgreSQL
  waits at every level (cases 04–10).
* **A concurrent `DELETE` of the value does not make an `INSERT` wait.** `remove_row` locks the row
  and not its entries, so an inserter meets the deleter's still-visible entry and answers `23505` at
  once, where PostgreSQL waits for the deleter and inserts if it commits.
* **A deferrable unique constraint is checked by a scan**, not by one key, and takes no lock here.
* **Two nodes** meet at prewrite, as every write-write conflict between nodes does (ADR 0057 unit 7).

## Tests

**§1, green** — `crates/esker-sql/tests/concurrent_unique_insert.rs`, against three real stores:
the second insert waits and is `23505` when the first commits; it waits and goes through when the
first rolls back. "B waited" is B's ungranted row in `pg_locks`, seen by a third session before the
holder ends — never a sleep, because a slow B that arrives after the commit answers `23505` without
any wait and would pass a clock. Beside them, the two rewritten tests and the in-process bounded wait.
The counterfactual — §1 taken out by the same asserted replace that put it in — turns the three waits
red again.

**§2 and §3, red until ruled on, and off `main` until then** — for the same file: a `FOR UPDATE` of a
row committed after the transaction began (e1, e2); `relations_test.rb`'s duel, statement for
statement, twice, as the acceptance; and SERIALIZABLE's `40001` (case 09).
