# 0104 — Where a conflict becomes `40001`, and where it becomes `40P01`

*Status: proposed. Nothing here is built. Written 2026-09-10 from run 112a's interim, which is the
first evidence this project has about the transaction family on a real topology.*

## Context — three failures the fake backend cannot produce

Run 112a runs Rails' ActiveRecord suite against **a placement driver, four stores and one SQL
node** (`esker-rails-harness/cluster-control.sh:2`). Run 111 ran the same suite against the
in-process `MemoryBackend`. Of the eight files compared so far, five are identical and three
transaction-family assertions moved — all three in the same direction of *concurrency control*,
which is the layer an in-process backend does not have:

| test | fake (run 111) | real (run 112a) | PostgreSQL |
|---|---|---|---|
| ① `transaction_test.rb` · `raises SerializationFailure when a serialization failure occurs` | passed | **nothing was raised** | `40001` |
| ② `transaction_nested_test.rb` · `unserializable transaction raises SerializationFailure inside nested SavepointTransaction` | passed | **nothing was raised** | `40001` |
| ③ `transaction_nested_test.rb` · `deadlock inside nested SavepointTransaction is recoverable` | passed | **`40001` — `a lock from the transaction at 6688 could not be cleared`** | `40P01`, then commit |

And `transactions_test.rb`, 105 runs green on the fake, hit the 1800 s per-file watchdog on the
real topology while the node went on answering new sessions.

The two neighbours of ③ that **passed** are what make it legible. `transaction_test.rb`'s
`raises Deadlocked when a deadlock is encountered` and `transaction_nested_test.rb`'s
`deadlock raises Deadlocked inside nested SavepointTransaction` both pass, so **the node does
detect this deadlock and does answer `40P01` to exactly one session.** ③ differs from its passing
neighbour in one thing only: after the `rescue`, it carries on and commits.

> The failure is not in detecting the deadlock. It is in what the transaction may do afterwards.

## The statement sequences, and what PostgreSQL raises at which step

`s1` and `s2` are rows of `samples` with `id` 1 and 2. `Bit.take` is
`SELECT "bits".* FROM "bits" LIMIT 1` — the tests' `make_parent_transaction_dirty`, there to force
Rails to open the real transaction without touching `samples`.

### ① `raises SerializationFailure when a serialization failure occurs`

Two sessions, no savepoint, both `SERIALIZABLE`
(`rails/activerecord/test/cases/adapters/postgresql/transaction_test.rb:38`):

```text
        session A                                   session B
  1     BEGIN ISOLATION LEVEL SERIALIZABLE          BEGIN ISOLATION LEVEL SERIALIZABLE
  2     -------------------- barrier `before` --------------------
  3     SELECT SUM(value) FROM samples              SELECT SUM(value) FROM samples
  4     INSERT INTO samples (value) VALUES ($1)     INSERT INTO samples (value) VALUES ($1)
  5     -------------------- barrier `after`  --------------------
  6     COMMIT                                      COMMIT   -- PG: 40001 on one of the two
```

This is write skew with a phantom: each session reads the whole table and then inserts into the
range it read. PostgreSQL's SSI records the two rw-antidependencies, finds the dangerous structure,
and raises **`40001` at the `COMMIT`** of whichever session commits second. The barriers guarantee
the overlap: neither session has committed when the other reads.

### ② the same, one savepoint deeper

`transaction_nested_test.rb:47`. Identical to ① except that each session runs `Bit.take` and then
opens `SAVEPOINT active_record_1`, and steps 3–4 happen inside it:

```text
  1     BEGIN ISOLATION LEVEL SERIALIZABLE
  2     SELECT "bits".* FROM "bits" LIMIT 1          -- make_parent_transaction_dirty
  3     SAVEPOINT active_record_1
  4     ---- barrier ---- SELECT SUM(value) FROM samples ; INSERT INTO samples … ; ---- barrier ----
  5     RELEASE SAVEPOINT active_record_1
  6     COMMIT                                       -- PG: 40001
```

PostgreSQL raises the same `40001` at the same step: a subtransaction's reads are the top
transaction's reads, and SSI does not care which savepoint they were made in.

### ③ `deadlock inside nested SavepointTransaction is recoverable`

`transaction_nested_test.rb:166`. No isolation clause — **READ COMMITTED** — and the crossed
sequence is inside a savepoint on both sides:

```text
        session A                                   session B
  1     BEGIN                                       BEGIN
  2     SELECT bits LIMIT 1                         SELECT bits LIMIT 1
  3     SAVEPOINT active_record_1                   SAVEPOINT active_record_1
  4     SELECT … id=1 … FOR UPDATE                  SELECT … id=2 … FOR UPDATE
  5     -------------------- barrier --------------------
  6     UPDATE samples SET value=4 WHERE id=2       UPDATE samples SET value=3 WHERE id=1
                    ↑ waits for B                              ↑ closes the cycle
  7                                                 PG: 40P01 to exactly one of the two
  8     (survivor proceeds)                         ROLLBACK TO SAVEPOINT active_record_1
  9     RELEASE SAVEPOINT active_record_1           UPDATE samples SET value=10 WHERE id=1
 10     UPDATE samples SET value=10 WHERE id=2        ↑ waits for the survivor
 11     COMMIT                                      COMMIT
        assert deadlocks == 1 ; assert samples.value == [10, 10]
```

Two things PostgreSQL does at step 8 that the test depends on:

* **`ROLLBACK TO SAVEPOINT` un-aborts the transaction.** `40P01` aborts the subtransaction, not the
  top transaction; after the rollback the block may write again and may commit.
* **A subtransaction's row locks are released when it aborts.** The victim's `FOR UPDATE` lock from
  step 4 goes away, which is what lets the survivor's step 6 finish. Locks taken *outside* the
  savepoint are kept to the end of the top transaction.

The test asserts both: `deadlocks == 1` (exactly one victim) and `[10, 10]` (both transactions
committed, in that order).

### The neighbours, for completeness

`transaction_test.rb` also holds `raises LockWaitTimeout when lock wait timeout exceeded`
(`SET lock_timeout = 1` behind a held row → `55P03`), `raises QueryCanceled when statement timeout
exceeded` (`SET statement_timeout = 1` → `57014`) and `raises QueryCanceled when canceling statement
due to user request` (`pg_cancel_backend` → `57014`). **All three pass on the real topology** — the
run's only failure in that file is ①. So the three *timeout* SQLSTATEs are already right, and the
missing ones are the two conflict codes.

## What the node does today

### Two lock spaces, and only one of them can be given back

A row lock in this node is two things, taken in this order
(`crates/esker-sql/src/backend/store.rs:314`):

1. **the node-local lock** — `RowLocks`, one table per `esker-sql` process, with the wait-for graph
   and the cycle walk (`crates/esker-sql/src/backend/locks.rs:244`);
2. **the Percolator lock** — a `Prewrite` carrying `TxnMutation::Check`, sent early, which is
   [ADR 0088](0088-a-row-lock-across-nodes.md)'s (a′). Taken **only for `Reach::Cluster`**, which is
   `SELECT … FOR UPDATE`; a plain write is `Reach::Node` and takes no store lock until it commits.

Giving one back is where they diverge:

| caller | node-local | Percolator |
|---|---|---|
| `ROLLBACK TO SAVEPOINT` → `Savepoints::give_locks_back_to` (`exec/savepoint.rs:191`) | released | **kept** |
| deadlock victim → `abandon_inner_locks` (`exec/savepoint.rs:181`) | released | **kept** |
| `Txn::abandon_locks` (`backend/store.rs:407`) | released | **kept** |
| `COMMIT` / `ROLLBACK` | released | released |

`StoreBackend::unlock` (`backend/store.rs:486`) calls `RowLocks::give_back` and nothing else, and
`esker_client::Transaction` has **no method that releases one eager lock** — `lock()` inserts into
`self.locked` (`crates/esker-client/src/txn.rs:698`) and only `commit` or `rollback` empties it.

### Where a conflict becomes an error, and which SQLSTATE it becomes

Every path below ends in `translate` (`crates/esker-sql/src/backend/store.rs:548`), and **the
mapping is correct — the codes are right and the sentences are PostgreSQL's.** What is wrong is the
*condition being reported*, not its number.

| condition | where it is decided | error | SQLSTATE | Rails class |
|---|---|---|---|---|
| a commit landed after our snapshot | `check_prewrite`, at prewrite | `TxnConflict` → `SerializationFailure` | `40001` | `SerializationFailure` |
| a commit landed inside a read **range** | `newest_write_in_range` (`esker-store/src/txnkv.rs:168`) | `TxnConflict` | `40001` | `SerializationFailure` |
| a lock we could not clear in 8 rounds | `Transaction::prewrite` (`esker-client/src/txn.rs:1354`) | `LockNotCleared` → `SerializationFailure` | `40001` | `SerializationFailure` |
| our own primary was rolled back under us | `check` (`esker-client/src/txn.rs:1623`) | `TxnSettled` → `Deadlock` | `40P01` | `Deadlocked` |
| a cycle in one node's wait-for graph | `RowLocks::deadlocks` (`backend/locks.rs:244`) | `Lock::Deadlock` → `SqlError::Deadlock` | `40P01` | `Deadlocked` |
| `lock_timeout` expired | `wait_for_the_lock` (`exec/mod.rs:359`) | `LockTimeout` | `55P03` | `LockWaitTimeout` |
| `statement_timeout` expired / cancelled | same | `StatementTimeout` / `QueryCanceled` | `57014` | `QueryCanceled` |

So there are **two** producers of `40P01` and **three** of `40001`, and the two families are told
apart by *who was killed*: a transaction whose primary carries somebody else's rollback marker was
killed on purpose and is a deadlock victim; a transaction that lost a race is a serialization
failure.

### Is the lock wait bounded, and is there deadlock detection?

Three different waits, three different answers — and the differences are the whole of this ADR.

| wait | bound | detector |
|---|---|---|
| statement waiting for a **node-local** lock (`wait_for_the_lock`, `exec/mod.rs:359`) | `lock_timeout`, else `statement_timeout`, else **none** | the node-local cycle walk, on every attempt |
| statement waiting for a **Percolator** lock (same loop, `Reach::Cluster`) | the same, so also **none** by default | **nothing** — no edge is drawn, see below |
| **commit** waiting for a Percolator lock (`Transaction::prewrite`, `esker-client/src/txn.rs:1307`) | `MAX_LOCK_RESOLUTIONS = 8` rounds, the last one the whole remaining lease | wound-wait on `start_ts`, only for the acquirer |

Two remarks that matter more than they look.

**An unbounded wait is not the divergence.** PostgreSQL's `lock_timeout` defaults to `0` too, and
Rails sets neither parameter in these tests: a real server waits for a row lock *indefinitely* and
relies on `deadlock_timeout` plus its detector. Copying the wait was right. What is missing is not a
timeout — it is a detector that can see this wait.

**A store-side wait draws no edge.** `StoreBackend::lock` takes the node-local lock *first*; when it
is granted, the session is recorded as **holding** the key and `RowLocks::take` clears any waiting
edge. The wait that then happens — inside `txn.lock(key)`, against another session's Percolator lock
— is invisible to the graph. Two sessions of one node can therefore hold each other:

```text
  A holds node-local k1, waits on B's Percolator lock on k2   -- no edge recorded
  B holds Percolator k2, waits on A's node-local lock on k1   -- edge B → A recorded
  the walk from B reaches A, finds A waiting for nobody, and reports no cycle
```

Wound-wait does not save this, because only *one* of the two waits goes through it: `A` is inside
`wound_or_wait` and waits if `B` is older (`esker-client/src/txn.rs:808`), and `B` is asleep in a
poll loop that has no opinion about age at all. Both wait; neither is bounded.

## The three failures, each traced

### ③ — a savepoint gives back half a lock

Take session B as the victim, and it is the older of the two, which the test does not control:

1. B's `SELECT … id=2 FOR UPDATE` (step 4) leaves **both** locks on `s2`.
2. B is told `40P01` at step 6 by the node-local walk — correct, and the reason the neighbouring
   test passes.
3. `ROLLBACK TO SAVEPOINT` gives back B's **node-local** lock on `s2`. Its **Percolator** lock on
   `s2` stays in the store — and because `s2` was B's first eager lock over an empty write buffer,
   that lock is B's **primary** (`pin_primary`, `esker-client/src/txn.rs:730`), so B's renewal
   thread (`esker-client/src/renew.rs`) keeps its lease alive for as long as B lives. It never
   expires and nobody may settle it.
4. A finishes its `UPDATE … id=2`, releases the savepoint, writes `s2 = 10`, and **commits**. Its
   commit prewrites `s2` and meets B's orphaned lock.
5. B is alive and *older*, so A may not wound it (`wound_or_wait`: `lock.start_ts <= self.start_ts`
   → wait). A spends its eight rounds, the last of them a whole lease, and is refused:

```text
ERROR:  could not serialize access due to concurrent update:
        a lock from the transaction at 6688 could not be cleared
```

which is `crates/esker-sql/src/backend/store.rs:585`, verbatim, and is the error run 112a recorded.
It escapes the test's `rescue ActiveRecord::Deadlocked` because it is a `SerializationFailure`, and
the file records it as an **error** rather than a failure — which is exactly how the interim reports
it.

The mirror case (B younger) is not green either: A wounds B, B's own commit is refused with
`TxnSettled` → `40P01`, and an `ActiveRecord::Deadlocked` escapes at step 11 where the test has no
`rescue`. **Both orderings fail, in two different ways, for one cause.**

### ① and ② — a range check that cannot see a lock

`SERIALIZABLE` here is snapshot isolation plus a validated read set
([ADR 0062](0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md)), and the
machinery is all present — ADR 0062 left ranges undecided (*"deciding ranges against a measurement
rather than against an argument"*) and
[ADR 0067](0067-the-check-mutation-and-the-latest-commit-question.md) §1 added `CheckRange` as tag 6:
`record_range` (`backend/store.rs:247`) records the scan behind
`SELECT SUM(value) FROM samples`, `commit` hands the read set over as `CheckRange` mutations
(`backend/store.rs:508`), and the store answers them at prewrite (`esker-store/src/txnkv.rs:406`).

The gap is one line of reach. A `CheckRange` asks `newest_write_in_range`, which iterates
**`cf::WRITE` only** (`esker-store/src/txnkv.rs:168`). A concurrent transaction that has prewritten
into the range but not yet committed is in `cf::LOCK`, and the check walks straight past it:

```text
  A  read range R, INSERT, prewrite(row_a, CheckRange R)   -- R holds no commit after A.start_ts → Ok
  B  read range R, INSERT, prewrite(row_b, CheckRange R)   -- A's lock is in cf::LOCK, not cf::WRITE → Ok
  A  COMMIT                                                -- succeeds
  B  COMMIT                                                -- succeeds; nothing was raised
```

The barriers in ① and ② are built to hold exactly this window open. The `CheckRange` arm declares
the window in one direction — *"what it cannot do is stop an insert that lands after this check"*
(`esker-store/src/txnkv.rs:404`) — and this is the other direction, an insert that landed **before**
the check and had not yet committed. A key-level `Check` does see a lock (`check_prewrite` answers
`KeyIsLocked`); a range check does not. That asymmetry is the defect.

The savepoint in ② adds nothing: the read set is the transaction's, `Recording` forwards
`validate_reads`, `read_set` and `restore_read_set` (`exec/savepoint.rs:333`), and ② fails for the
same reason ① does. It is listed separately only because it fails separately.

## The design

Four units. Each says which layer it is in and what it needs before it can be built.

### §1 — a range check must see a lock, not only a commit  *(store/txn — h1)*  — **built**

`TxnSnapshot` gains `foreign_lock_in_range(start, end, mine)`, the same walk
`newest_write_in_range` already does, over `cf::LOCK` — the iterator and the key split both exist
in the same file (`esker-store/src/txnkv.rs`, `key::split_lock`). The `CheckRange` arm of
`prewrite` asks it **after** the write scan and, when a lock of another transaction is inside the
range, answers `TxnStatus::Locked(lock)` instead of `Ok`.

The order is the point: a commit inside the range is a verdict — this transaction has lost whatever
anyone is holding — and a lock is a *question*, because its owner may still roll back, in which
case nothing was ever there and refusing over it would be a `40001` for a phantom that never
existed. `Locked` and not `Conflict` for exactly that reason.

**`mine` is excluded, and the build is what proved it must be.** `Transaction::commit` prewrites
this transaction's own keys at step 3 and sends the range checks at step 4, so by the time the
check runs, the row this transaction inserted into the range it scanned is locked *inside that
range*. A scan without the exclusion refuses every transaction that writes where it read — which is
all of them. `our_own_lock_inside_a_checked_range_is_not_a_phantom` is that guard.

No wire change: `Prewrite` already answers one status per mutation and `TxnStatus::Locked` is
already one of them ([ADR 0016](0016-txnkv-on-the-wire.md) decision 1).

**The client must not wound for a range check, and this is the part to get right.** A wound exists
for an *acquirer* — a transaction that holds locks and wants one more, which is half of a cycle. A
range check acquires nothing: it asserts that a range it read has not moved. Killing the holder to
make that assertion true would abort a transaction that did nothing wrong, and would answer `40P01`
to a session whose test expects `40001`. So the range check resolves with `may_wound = false` —
wait for the holder to settle, then ask again, which is what a reader already does
(`esker-client/src/txn.rs` and the note there: *"a reader holds no locks: it can wait and cannot be
waited for"*).

**Where that loop goes is not where this ADR first said.** A range check never travels in
`prewrite`'s batch: `prewrite_range_checks` sends **one `Prewrite` per range, carrying a single
`CheckRange`**, so no positional trick is needed to tell a range's lock from a key's. The gap was
one line further on — the answer fell through to `check`, whose `Locked` arm reports
`LockNotCleared` on sight with the comment *"reaching here means the caller skipped the
resolution"*. It had, because until now the store could not answer `Locked` to a range. So the loop
is `prewrite_range_checks`'s own, bounded by `MAX_LOCK_RESOLUTIONS` like every other.

The outcome for ① and ②: the second session waits for the first, asks again, now sees its commit in
`cf::WRITE`, and is refused `TxnStatus::Conflict` → `40001`. If both sessions wait for each other,
both spend their budget and both are refused `40001` — which still satisfies `assert_raises`, and is
a conservative answer rather than a wrong one.

**What it still cannot see is a lock past a region boundary.** A range check is answered by the
region its lower bound falls in and no further, which is the bound ADR 0067 §3 already declares for
commits; the lock scan inherits it exactly. `a_range_check_waits_for_a_lock_it_may_not_wound`
places its holder on the near side of the boundary deliberately, because a test that straddled it
would be measuring the limitation instead of the fix.

**This does not make the node serializable.** It closes the concurrent-prewrite window and nothing
more; the window ADR 0062 declares — an insert that lands after the check — stays open, and
`Isolation::Serializable` stays the declared divergence it is
(`crates/esker-sql/src/parameter.rs:594`).

### §2 — a savepoint must be able to give back an eager lock  *(store/txn — h1; needs the human)*

The one thing this needs does not exist: **a way to remove this transaction's own lock record from a
key without ending the transaction.**

`Rollback` cannot be it. `esker_txn::rollback` writes a rollback marker at `commit_ts == start_ts`
(`crates/esker-txn/src/percolator.rs:592`), which makes the transaction dead on that key for ever —
a later `Prewrite` of it answers `PrewriteDecision::RolledBack`. A savepoint's victim very often
writes the row it locked (③'s victim writes `s1` after its `rescue`), so a marker would trade this
bug for a worse one. `ResolveLock` with `commit_ts = 0` calls the same function
(`esker-store/src/txnkv.rs:581`) and is out for the same reason.

So the proposal is a new request:

```text
TxnKvReq::ReleaseLock { start_ts, keys }
  -> for each key: if cf::LOCK holds a lock with this start_ts, delete it. No write record.
     Answers how many were released. Idempotent; a key we no longer hold is a success.
```

with `esker_client::Transaction::release(&mut self, keys)` removing them from `self.locked`, and
`StoreBackend::unlock` calling it beside `RowLocks::give_back` so that one code path gives back both
halves. `Savepoints::give_locks_back_to` (`exec/savepoint.rs:191`) already knows exactly which keys
belong to the savepoint and already calls `Txn::unlock` for each, so nothing above changes.

**This is a wire change and it is not mine to make.** `CLAUDE.md` — *"Ask before doing … change an
on-disk or wire format that already has a golden test"* — and ADR 0067 §2's rule that a
cluster-scope lock change is *"asked for as one rather than smuggled in"*. It adds a method tag to
`TxnKvReq`; it adds **no** `TxnWrite` variant and **no** record kind, so no on-disk golden moves,
and it is not replicated as a new command shape — the deletion of a lock record is a mutation the
`Mutations` type already produces. That is the smallest form I can find; the question is the human's.

**Only our own lock.** A key whose lock belongs to another `start_ts` is untouched — the same rule
`rollback` states (`crates/esker-txn/src/percolator.rs:610`).

#### The primary is the hard half, and in ③ it is *the* half

The obvious guard — *never release the primary* — would leave ③ exactly as red as it is today, and
working out why is what makes this unit worth its ADR.

`Transaction::lock` pins a primary on the first eager lock: **the smallest key already buffered, or
the key being locked when nothing is** (`pin_primary`, `esker-client/src/txn.rs:730`). In ③ the
victim's transaction has read `bits` and written nothing when it reaches `s2.lock!`, so the buffer
is empty and **`s2` becomes the primary** — the very key the savepoint has to give back. The
renewal thread then keeps that lock's lease alive for the life of the transaction, which is why
step 5's `classify` never finds it expired and why the survivor spends its whole budget on it.

So the guard cannot be "never". Two ways out, and I would build (a):

**(a) Release the primary when nothing else names it, and unpin.** A lock record is only consulted
through its primary, so a primary with no secondaries pointing at it is safe to delete: set
`pinned = None`, stop the renewal, and let the next eager lock or the commit pin a new one by the
existing rule. The condition is exact and cheap — the transaction holds no *other* key in
`self.locked` and has prewritten no buffered write. That is precisely ③'s victim, and it is the
common Rails shape: a savepoint whose first act is a `lock!`.

Refuse the release when the condition fails, and the residue is **one row of one transaction**
rather than every row of every savepoint — worth stating as the declared remainder rather than
pretending it is closed.

**(b) Pin a primary no row shares.** Give every transaction a private primary key in the `'x'`
namespace derived from its `start_ts`, prewritten once at the first eager lock. Then no eager lock
is ever a primary, every one of them is releasable with no condition at all, and (a)'s remainder
disappears. It costs one round trip and one extra key per transaction that takes an eager lock, and
it wants its own measurement before anyone believes the round trip is affordable — which is the
same question ADR 0088 asked of the eager lock itself and answered with `tests/lock_cost.rs`.

(b) is the better end state and (a) is what closes ③. They compose: (a) first, (b) if the remainder
is ever measured to matter.

*If the human refuses the new method*, the fallback must be written down rather than discovered:
`SELECT … FOR UPDATE` inside a savepoint keeps its cluster lock to the end of the top transaction,
③ stays red, and the sentence goes in `DESIGN.md` §8 and the divergence table — *"a subtransaction's
row locks are released node-locally on `ROLLBACK TO SAVEPOINT`; the cluster-scope half is held to
the end of the transaction"*.

### §3 — a store-side wait must draw an edge  *(SQL layer — either lane; I would take it)*

`StoreBackend::lock` returning `Lock::Held { by, .. }` from the Percolator half must record the
wait in the node-local graph before returning, and clear it on every exit — which
`wait_for_row`'s single tidy-up point already does (`exec/mod.rs:321`).

`by` is the holder's `start_ts`, and `RowLocks` already carries a `start_ts` beside every holder's
id, so a holder **on this node** is resolvable to an id and the existing edge and the existing walk
apply unchanged. A holder on *another* node has no id here; that edge cannot be drawn and must not
be faked, so the answer stays wound-wait, which terminates.

That is the whole of it: one lookup, the edge, and the same `deadlocks` call the node-local path
already makes. It turns the invisible same-node hold-and-wait above into a `40P01` for exactly one
session.

**It does not fix ③.** ③'s fatal wait happens inside `COMMIT`, in the client's prewrite loop, where
the SQL layer's graph is not consulted at all. §3 is for the *statement* path, and it is what stands
between `transactions_test.rb` and a wait that nothing ends — see below.

### §4 — the commit path's budget reports the wrong condition  *(store/txn — h1)*

`Transaction::prewrite` gives up after `MAX_LOCK_RESOLUTIONS = 8` rounds and reports
`LockNotCleared` → `40001`. Against a *dead* holder that is right and the budget is what stops an
infinite loop. Against a **live, heartbeating** holder it is a spurious failure: PostgreSQL would go
on waiting, and the transaction being refused has lost no race — it has met a lock that is still
somebody's.

Two changes, neither of them a new bound:

* **Ask why the budget ran out.** A refusal after eight rounds against a holder that was *alive on
  every one of them* is not a serialization failure; it is this transaction failing to acquire. The
  honest code for it is `55P03` (`SqlError::LockNotAvailable` / `LockTimeout`, both already mapped),
  and the honest sentence names the holder. `40001` tells a client to retry an identical
  transaction, which will meet the same live lock and fail the same way.
* **Check our own fate before reporting.** A transaction that has been wounded is holding a primary
  with somebody else's rollback marker on it, and `primary_fate` (`esker-client/src/txn.rs:1841`)
  already asks that question atomically. Asking it *before* returning `LockNotCleared` turns a
  wound the victim has not noticed yet into the `40P01` it is, at the step where PostgreSQL raises
  it, rather than one statement later.

§4 is not required by any of the three tests once §2 lands. It is here because ③ is the second time
this project has read a `LockNotCleared` as evidence of something it is not, and because the first
of the two changes is what makes a *long* transaction survive a *slow* one.

### §5 — savepoint interaction, stated once

| event | node-local locks | Percolator locks | the block |
|---|---|---|---|
| `SAVEPOINT` | mark taken (`locks_at`) | mark taken (same list) | open |
| `ROLLBACK TO SAVEPOINT` | keys since the mark released | **§2**: keys since the mark released, the primary only if nothing else names it | usable again |
| `40P01` inside a savepoint | keys since the mark released (`abandon_inner_locks`) | **§2**: the same | usable after `ROLLBACK TO` |
| `RELEASE SAVEPOINT` | nothing released | nothing released | open |
| `COMMIT` / `ROLLBACK` | all released | all released | over |

The rule in one sentence: **a lock's lifetime is the innermost open savepoint that took it**, and
after §2 that is true of both halves rather than one. Locks taken before the mark are the outer
block's and are kept — which `Savepoints::locks` already gets right by recording a key only the
*first* time it is locked (`exec/savepoint.rs:85`).

## What each of the three tests needs

| test | needs | layer |
|---|---|---|
| ① `raises SerializationFailure when a serialization failure occurs` | §1 | store/txn (h1) |
| ② the same inside a nested `SavepointTransaction` | §1 | store/txn (h1) |
| ③ `deadlock inside nested SavepointTransaction is recoverable` | §2 | store/txn (h1) + one wire method **the human must approve** |

**Nothing on this list is a SQL-layer mapping change.** That is the finding I did not expect: the
codes, the sentences and the Rails classes they land in are all already right, and all three
failures are the store and client deciding the wrong *condition*. The only SQL-layer unit in this
ADR is §3, which no listed test needs and which the hung file may.

Two consequence tests, red before either change:

* `crates/esker-sql/tests/` — two sessions, `SERIALIZABLE`, each scanning a range and inserting into
  it, both prewriting before either commits: **one must be refused `40001`.** Fails today (§1).
* `crates/esker-sql/tests/` — two sessions, crossed `FOR UPDATE` inside savepoints, victim rolls
  back to its savepoint and both commit: **both commits succeed and exactly one `40P01` was
  raised.** Fails today with `40001` from the survivor's commit (§2).

## `transactions_test.rb`'s 1800 s — candidates, and what would settle it

Not diagnosed. The node log for run 112a carries no statement log, so which test stopped is not in
the evidence, and this section names what the code makes possible rather than what happened.

The file's only concurrency is `ConcurrentTransactionTest`, two tests, both **three or four sessions
writing one row**:

* `test_transaction_per_thread` (`transactions_test.rb:1707`) — 3 threads, each
  `BEGIN; SELECT topics WHERE id=1; UPDATE; UPDATE; COMMIT`.
* `test_transaction_isolation__read_committed` (`transactions_test.rb:1725`) — 3 threads doing
  `find/save/find/save/find` on `developers` id 1, plus a fourth doing ten read-only transactions.

Both are `Reach::Node` writes only — no `FOR UPDATE` — so §3's invisible wait needs a `FOR UPDATE`
that these tests do not have, and neither should be able to reach the hold-and-wait shape. What they
*can* reach is the restart loop: `changed_since_statement` → `restart_statement` →
`StatementMustRestart`, capped at `MAX_STATEMENT_RESTARTS = 32` (`exec/mod.rs:572`), with each
attempt's wait unbounded because neither `lock_timeout` nor `statement_timeout` is set. Thirty-two
bounded restarts do not hang; thirty-two **unbounded waits** can, if a holder never releases.

A holder that never releases is the third candidate and the one I would look at first.
`test_rollback_when_thread_killed` (`transactions_test.rb:1100`) kills a thread inside an open
transaction, mid-`UPDATE topics … WHERE id = 1`, and never rolls it back. That lock is released by
`StoreTxn`'s `Drop` (`backend/store.rs:208`), so it lives exactly as long as the session's open
transaction object does — and a connection sitting idle in a transaction in the pool keeps it
indefinitely. `RowLocks` is the one lock space in this system with **no TTL at all**
(`backend/locks.rs:15`: *"a holder is alive exactly while its transaction is"*), and the node's
`idle_in_transaction_session_timeout` — which would end such a session — defaults to `0` here as it
does on a real server (`pgwire/server.rs:242`).

**The test that leaves the lock is not the test that hangs.** Its own assertions are plain `SELECT`s
and never wait; what waits is the *next* test in the file that writes `topics` id 1, and minitest
does not run them in source order. That is the shape to look for in (2) below, and it is one this
project has met before — a `psql` that answered instantly while a pass sat still, and the cause was
a lock left by a killed connection.

What would settle it, in the order I would spend the time:

1. **`RUST_LOG` at a level that logs the statement**, or the harness's `log_statement` tap, so the
   watchdog's last statement is in the record. One re-run of that one file.
2. **`SELECT * FROM pg_locks` against the node while it is stuck.** The node answers new sessions,
   which is what makes this cheap, and `LockView` reports both holders and waiters with their
   backend pids (`backend/locks.rs:75`). A holder with a pid no session owns is the killed-connection
   answer; a pair of waiters is a cycle the graph could not see.
3. Only then a constructed reproduction.

Until (1) or (2), *"a lock wait with no timeout and no deadlock detection"* is a well-formed
hypothesis with three candidate mechanisms and no evidence separating them, and this ADR does not
choose between them.

## Consequences

* **`SERIALIZABLE` gets stricter but does not become serializable.** §1 refuses a class of history
  the node admits today; the `40001` rate under a range-scanning workload goes up, and some of those
  refusals are conservative. ADR 0062's declared divergence narrows by one window and survives.
* **§2 adds a method to the TxnKv wire.** Every store must understand it before a client sends it;
  a mixed-version cluster answers `UnexpectedResponse` to a release. It needs the ordinary
  gate-reverse-dependents care that a new proto variant always needs.
* **§3 makes a `40P01` reachable where the node used to hang.** A test that passed by waiting will
  now be told, which is the direction we want and is still a behaviour change.
* **§4 moves one condition from `40001` to `55P03`.** A client retrying on `40001` alone stops
  retrying that case, which is correct — the retry could not have worked — and is visible.
* **Nothing here changes a SQLSTATE mapping, a message, or an on-disk record.** The three tests need
  the store to decide differently, not the SQL layer to say it differently.

## References

* [ADR 0057](0057-read-committed-waits-for-the-writer-in-front-of-it.md) — the wait, the statement
  restart, and the per-key `read_ts`.
* [ADR 0062](0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md) — `SERIALIZABLE`
  is SI plus a validated read set, and the window it declares.
* [ADR 0067](0067-the-check-mutation-and-the-latest-commit-question.md) — `TxnMutation::Check`, the
  range check's region bound, and §2's rule about asking for a cluster-scope lock as its own change.
* [ADR 0088](0088-a-row-lock-across-nodes.md) — the eager Percolator lock, wound-wait, the renewal
  sender, and the one-node/two-node table this ADR extends with the savepoint case.
* `esker-rails-harness/results/run-112a-interim.md` — the three failures and their text.
