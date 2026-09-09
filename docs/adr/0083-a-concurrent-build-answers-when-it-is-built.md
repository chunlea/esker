# 0083 — A concurrent index build answers when it is built

*Status: accepted (2026-09-08, by the coordinator on the user's behalf). Amends
[ADR 0020](0020-online-schema-change.md)'s client contract; the states, the interval and the
re-driver are untouched.*

## Context

`CREATE INDEX CONCURRENTLY` here wrote a job record and returned. That was a deliberate reading of
what `CONCURRENTLY` means — the change is many transactions, so the statement cannot be one of them
— and it is not what a client sees on a real server.

PostgreSQL's concurrent build is synchronous *to its client*: it waits for concurrent transactions
twice, builds the index, and only then answers. When it fails it answers with the failure and
leaves the half-built index in the catalog, marked invalid. Measured on 19beta1:

```text
BEGIN; CREATE UNIQUE INDEX CONCURRENTLY … ;  25001 CREATE INDEX CONCURRENTLY cannot run inside
                                                   a transaction block
CREATE UNIQUE INDEX CONCURRENTLY invalid_index ON ex (number);   -- two rows share a value
                                             23505 could not create unique index "invalid_index"
                                             DETAIL:  Key (number)=(1) is duplicated.
SELECT indisvalid, indisready FROM pg_index …;       f | f
CREATE UNIQUE INDEX [CONCURRENTLY] invalid_index …;  42P07 relation "invalid_index" already exists
REINDEX INDEX invalid_index;                         23505, the build tried again
DROP INDEX invalid_index;                            DROP INDEX, and the name is free
```

`postgresql_adapter_test#test_invalid_index` asserts three of those in one statement: the raise,
the index existing afterwards, and its `indisvalid` being false. This node answered none of them —
it reported success, built nothing, and left the index at `absent` for ever, because the fake
backend publishes no step interval and so runs no re-driver.

Two of the six lines were already right, and one of those was right by accident: `indisvalid` was
`f` because the index had never been built, not because a build had failed.

## Decision

**The statement drives the change it starts, and answers when the change is over.**

* `create_index`'s concurrent branch declares the index at `absent` and writes the job exactly as
  before, and records the index id for the far side of the statement's commit. The drive itself is
  `exec::ddl::finish_concurrent_build`, called from the implicit transaction's commit path — the
  earliest point at which a job step, which is a transaction of its own, can see the declaration.
* It steps until the job record is gone, which is what a finished change is: the last step forgets
  it. A duplicate is the step's error and therefore the statement's, raised **after** the states
  have unwound — so the invalid index is in the catalog, `indisvalid = f`, with its name still
  taken, which is what the three Rails assertions read.
* The error is the build's sentence, not a writer's: `job::backfill_batch` now raises
  `CouldNotCreateUniqueIndex`, the same variant the blocking backfill has always raised, with the
  duplicated key rendered into the `DETAIL`. `duplicate key value violates unique constraint` is
  what an `INSERT` meeting the finished index is told, and it was the wrong sentence here.
* **The wait between transitions is kept.** A driven build takes PD's published step interval after
  every transition, in interruptible slices, exactly as an interactive driver must — driving from
  the statement does not make a node one state behind any less able to become two. A node the
  interval was never published to takes no wait: it has no placement driver, and so no second node
  that could be a state behind it, which is the same reading `bin/esker-sql.rs` already applies to
  the re-driver.
* The schema lease is checked **before every step**, not once: a build is many transactions over
  minutes and a node that loses PD part-way must stop writing (ADR 0028). Stopping leaves the job
  where it is, for another node's re-driver.

**And `esker.concurrent_index_build` is the seam.** `wait` is the boot value and the decision
above. `stage` is what this node did before: write the job and return, leaving it for
`esker_schema_step` or a re-driver.

`stage` exists for two reasons and neither is a test:

* this node's concurrent build costs `3 × (lease + lock TTL)` — twenty-four seconds on the
  published numbers — because the interval is a safety bound rather than a wait for readers. That
  is a real cost PostgreSQL does not have, and an operator running a migration may reasonably
  refuse to hold a connection for it;
* a staged change is the only thing a *cluster* can drive, and a node that wants a change started
  rather than made is asking for exactly that.

It is also, and only then, what keeps ADR 0020's own tests able to watch a state machine: a
statement that has already run the states to the end cannot be observed part-way through them.
`tests/schema_change.rs` and `tests/redrive.rs` set it once per node; `tests/slt/index.slt` and
`tests/pg_catalog_index.rs` set it where a half-built index is the subject.

## Consequences

* One Rails test goes from failing to passing, and the whole `algorithm: :concurrently` surface
  becomes honest rather than optimistic: a migration that would fail now fails, where before it
  reported success and left nothing behind.
* A concurrent build holds its client for three step intervals on a cluster with a placement
  driver, and for the length of the backfill on one without. `statement_timeout` and `pg_cancel`
  both reach it — the waits are short sleeps with a cancel check between them — so a client is
  never stuck with no way out.
* `CREATE INDEX CONCURRENTLY` can now fail *after* its declaration has committed. That is
  PostgreSQL's behaviour and it is why `25001` matters more than it did: the statement must be
  alone, because there is no block whose rollback could take the declaration back once the build
  has begun.
* **`DROP INDEX CONCURRENTLY` is deliberately left staged.** Its last step waits the *retention*
  window rather than the step interval — an hour on the published numbers, because what it must
  outlast is a reader and not a writer — and a statement that held a client for that would be
  worse than one that returns. Nothing in the suite executes the concurrent drop (the two Rails
  tests that name it assert generated SQL), so the divergence is recorded rather than paid for.
  Revisit it if the removal wait ever stops being retention-shaped.
* `indisready` is still a column this node's `pg_index` does not have, and `REINDEX` is still
  `0A000` (register G05). Neither is reached by the Rails test; both are measured in
  `tests/invalid_index.rs`'s module docs so the next reader does not have to measure them again.
