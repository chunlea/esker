# ADR 0062 — SERIALIZABLE is snapshot isolation plus a validated read set

Status: proposed · Date: 2026-09-04 · Phase 9 (Rails compatibility), the locking family ·
Builds on [ADR 0057](0057-read-committed-waits-for-the-writer-in-front-of-it.md)

## Context

`SET TRANSACTION ISOLATION LEVEL SERIALIZABLE` is accepted today and gives **snapshot isolation**.
That is [ADR 0031](0031-rails-compatibility-is-measured.md)'s permanent caveat, and it is honest
rather than silent: the level is answered, its guarantee is not. The human accepted it for v1 and
asked for the better implementation to be designed.

What SI is missing is one anomaly with a name. Snapshot isolation refuses dirty reads, refuses
non-repeatable reads, and refuses lost updates by first-committer-wins — and permits **write skew**:

```sql
-- the classic, and the one the suite writes: two doctors, one must stay on call
-- T1                                        -- T2
BEGIN ISOLATION LEVEL SERIALIZABLE;          BEGIN ISOLATION LEVEL SERIALIZABLE;
SELECT count(*) FROM on_call WHERE duty;     SELECT count(*) FROM on_call WHERE duty;
-- 2, so it is safe to go off                -- 2, so it is safe to go off
UPDATE on_call SET duty = false              UPDATE on_call SET duty = false
  WHERE name = 'alice';                        WHERE name = 'bob';
COMMIT;                                      COMMIT;
-- both commit. Nobody is on call.
```

Neither transaction wrote a key the other wrote, so nothing about first-committer-wins has an
opinion. What each of them *read* is what the other invalidated, and a rule about writes cannot see
that.

The measure this ADR is judged by is already on the board and already red **by design**:
`transaction_nested_test.rb`'s two `test_*Serialization*` cases, which expect
`ActiveRecord::SerializationFailure` and get a clean commit. ADR 0057's unit declared them as
staying red while SERIALIZABLE is served as SI; this is the ADR that makes them green.

## Decision

**Record what a SERIALIZABLE transaction reads, and validate that read set at commit.**

A transaction at SERIALIZABLE keeps everything snapshot isolation already gives it — one snapshot
for its whole life, readers that do not block writers, first-committer-wins on the keys it writes —
and adds one rule:

> at commit, for every key this transaction **read**, there must be no commit on that key newer
> than the snapshot it read at. A key that has one is `40001`.

That is the validation condition of backward-oriented optimistic concurrency control (Kung and
Robinson, 1981), and it is what makes the result serializable rather than merely stricter: a
transaction that passes validation can be placed in a serial order **after** every transaction that
committed during its lifetime, because it read nothing any of them changed.

Write skew dies on it in the obvious way. T1 read the rows T2 wrote, so whichever of the two commits
second finds a commit on a key it read and takes the `40001`. Exactly one survives, which is what a
real server does.

### 1. The read set, and where it is recorded

In **`esker-client`'s `Transaction`**, beside the write buffer, populated by `get` and `scan` — not
in the SQL layer. The client is the one place every read passes through, it already holds the
per-key machinery ADR 0057 §4 added, and a read set assembled a layer up would miss the reads the
executor makes on a caller's behalf.

Three kinds of read are **excluded**, and each exclusion is a correctness statement rather than an
optimisation:

* **A read served from the transaction's own write buffer.** It read its own value; no other
  transaction can invalidate it.
* **Catalog reads** (the `'m'` namespace). Every statement reads the catalog, so validating it would
  make every concurrent `CREATE TABLE` a serialization failure for every transaction in flight.
  PostgreSQL's own predicate locking ignores system catalogs for the same reason. The cost is
  declared below.
* **Non-transactional reads**: `nextval`, the sequence relation's counter, `pg_stat_activity`, and
  anything else already outside the snapshot. They were never serializable and this does not change
  that — it records it.

### 2. Validation is atomic because the check takes a lock

This is the half a sketch gets wrong, and getting it wrong yields a design that is *nearly*
serializable, which is worse than SI declared honestly.

Validation and commit must be atomic with respect to other transactions' commits. A check performed
at prewrite that left nothing behind would leave a window: between our check and our commit,
another transaction can commit a write to a key we read, and both of us pass.

So a checked key takes a **lock record**, in the lock CF, at prewrite — the same place and the same
shape as a write's lock, carrying no value:

* a concurrent writer's prewrite of that key meets a live lock and does what it already does with
  one — waits, or resolves it if the lease has expired;
* our own check sees *their* lock or their newer commit if they got there first, and we take the
  `40001` instead.

The interval `[start_ts, our commit]` is then covered end to end: a commit before our check is
**detected**, and a commit after it is **blocked**. That is the OCC validation condition made atomic
by the mechanism Percolator already has.

The cost is stated plainly: under SERIALIZABLE, **a reader blocks a writer for the length of the
reader's commit window**. Not for the length of the transaction — the locks go on at prewrite — but
it is no longer true at this level that readers never block writers. That is the price of the
guarantee, and it is why it is this level's price and not everyone's.

### 3. The wire shape: an additive tag, as in ADR 0057

`TxnMutation::Check { key }` takes **tag 5** beside the existing 1 and 2 (put, delete) and 3 and 4
(the same two carrying a per-key read timestamp), and `TxnWrite::Check` takes log kind 5 —
`TxnCommand::Prewrite` is a replicated command, so this is a wire change and a Raft log change
together and needs the human's ruling exactly as ADR 0057 §4's did.

The argument is the one that was accepted there, and it holds for the same reasons: every existing
golden stays **byte-identical**, because a mutation that is not a check is still tag 1, 2, 3 or 4 and
still encodes what it encoded before; and an older peer meets an unknown tag and **refuses**, rather
than reading a longer message under a known tag as a short one with trailing bytes.

**No `read_ts` on the check.** A SERIALIZABLE transaction reads at its own `start_ts` for its whole
life — statement re-snapshotting is READ COMMITTED's, and this level does not have it — so the
request's `start_ts` is the timestamp every check is validated against and repeating it per key would
be a number that can only disagree with itself.

### 4. What happens to a check at commit

A check lock is released like any other lock the transaction holds: it is **committed**, writing a
`write` record of a kind that carries no value and is invisible to readers. Not rolled back
separately, and the reason is crash safety rather than tidiness — a commit that wrote the data keys
and then cleaned the check locks in a second step would leave, on a crash between them, locks whose
transaction is committed, which is the one state the resolver has to reason hardest about. Uniform
commit means the existing resolve path already handles them.

The cost is one write record per checked key. GC may drop a check record as soon as it is below the
safepoint — it is not a version of anything.

### 5. Read-only transactions

A read-only SERIALIZABLE transaction has nothing to prewrite, so nothing validates it, so it can
still observe the **read-only anomaly**: a state no serial order of the read-write transactions could
have produced. PostgreSQL's SSI handles this case; SI plus write-set-owner validation does not.

Two options, and this ADR takes the second:

1. **Validate read-only transactions too** — a prewrite carrying only checks, then a rollback of
   those locks. Correct, and it makes every read-only SERIALIZABLE transaction pay a round trip and
   take locks that block writers.
2. **Declare it.** `SERIALIZABLE READ ONLY` is snapshot isolation with a validated read set only when
   the transaction writes something; a transaction that writes nothing is not validated and the
   read-only anomaly stands.

Option 2, because the anomaly needs three transactions in a specific interleaving to appear, no test
in the Rails suite constructs one, and the cost of option 1 falls on the workload that is most
common and most performance-sensitive. `SERIALIZABLE READ ONLY DEFERRABLE`, PostgreSQL's own answer
to this, is a `0A000` naming itself until this is revisited.

## What this does **not** catch

* **Phantoms.** A key-level read set records keys that *existed*. `SELECT … WHERE duty` that matched
  two rows records those two; a third row **inserted** by a concurrent transaction is a key nobody
  read, so no check names it and both transactions commit. Ranges are the fix and they are their own
  decision — see §Ranges below. Until then this is SERIALIZABLE **without phantom protection**,
  which is materially stronger than SI and materially weaker than PostgreSQL, and must be documented
  as exactly that rather than as "serializable".
* **Predicates the store cannot see.** A read filtered in the SQL layer still read the keys it
  filtered, so the read set is a superset — that direction is safe (spurious `40001`s, never a missed
  conflict). A read that never happened because a plan skipped it is invisible, and a plan that reads
  *fewer* keys makes the guarantee weaker without saying so. **A plan change is a correctness change
  at this level**, which is a rule this codebase does not have anywhere else and belongs in the
  planner's doc comments.
* **The catalog**, by construction (§1). Two transactions that serialize only through a DDL are not
  serialized.
* **Anything outside the transaction**: sequences, advisory locks, `now()`, the application's own
  cache.
* **Cross-node deadlock**, unchanged from ADR 0057: a check lock can be waited on, and the wait-for
  graph is one node's.

### Ranges, and why they are not decided here

A phantom is a commit in a *range* the transaction read, and there are two shapes:

1. **A range check**: the scan records `[lo, hi)` and the check asks the store for any commit in that
   range after `start_ts`. Precise, and it needs a range scan of the write CF per checked range at
   prewrite — the cost of the check is then the cost of a scan rather than of a point read.
2. **A table version**: every write to a table bumps one counter, and a scan records the counter it
   saw. Cheap and coarse — every concurrent write to the table is a conflict for every scanner of it,
   which on a hot table is a serialization failure per commit.

Neither is free and neither is obviously right; this ADR proposes shipping key-level validation
first, with the phantom gap declared, and deciding ranges against a measurement rather than against
an argument. The measurement to make: how many of the suite's SERIALIZABLE tests need phantom
protection at all.

## Cost

| | |
|---|---|
| **Memory** | one entry per distinct key read, per transaction in flight. A scan of a million rows is a million entries, so the set needs a cap and an escalation — the natural one is "past N keys, record the range instead", which is §Ranges arriving early for a different reason. |
| **Network** | prewrite grows by one entry per checked key. A key that is both read and written needs **no** check: its own write lock already covers the interval. |
| **Store** | a check is the conflict test prewrite already runs, without the write. |
| **Blocking** | a writer blocks behind a checked key for the reader's commit window (§2). |
| **Read-only** | unchanged, and unvalidated (§5). |

## The measure

* **`transaction_nested_test.rb`'s two `test_*Serialization*` cases**, red by design since ADR 0057
  and green when this lands. They are the whole suite-visible payoff, and the honest headline is two
  tests rather than a level.
* **A write-skew probe** in `two-session-replay.rb`'s shape: two sessions, the on-call table above,
  both committing today and exactly one committing after. It belongs beside the three-writers probe
  as a contract row, and it is the row that says the level means something.
* **No regression in the locking family**: 261 runs / 5 failures / 4 errors is ADR 0057's baseline,
  and a SERIALIZABLE that turns concurrent writers into serialization failures wholesale would move
  it the wrong way while making the two tests above pass.

## Consequences

* SERIALIZABLE stops being a synonym for SI and starts having a cost that shows in a workload's
  numbers. `default_transaction_isolation` stays READ COMMITTED, which is PostgreSQL's default and
  what every Rails application uses unless it asks otherwise.
* ADR 0031's caveat narrows rather than disappearing: the sentence becomes "SERIALIZABLE validates
  the keys a transaction read, and does not protect against phantoms."
* One more mutation tag, and the same ruling it needed last time.
* A plan change can weaken an isolation guarantee (§What this does not catch). That is the sharpest
  edge in this design and the one to write down where planners are written, not only here.
