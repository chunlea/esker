# 0020 — Distributed online schema change

Status: **being built**, and **amended by what building it found**. The states, the rule and the
crate list below stand; three claims about *why* the rule works did not survive reading the code,
and are corrected in place below — each marked **Amended (phase 6e)**, with the full argument in
`docs/plans/phase-6e.md` §1. The milestone is in `docs/plans/phase-6a.md` §12. See
[ADR 0019](0019-a-row-says-how-many-columns-it-has.md) (the row format this rests on),
`crates/esker-sql/src/catalog/`, `docs/txn-spec.md` §5 and §7.

## Context

`ALTER TABLE ADD COLUMN` of a nullable column landed as one catalog write with no row rewritten
and no staging at all. That is not a general result and must not be read as one, so this ADR starts
by saying exactly why that one change was allowed to be instantaneous — because the same argument
is what shows that nothing else is.

Every node caches table definitions and reads the catalog through a transaction, so a definition
belongs to a snapshot. But **snapshot isolation does not order a schema change against a
concurrent writer.** The dangerous shape is this, and it is the shape every rule below exists to
close:

```text
T1  begin at ts=100, reads the catalog: table t has one index
DDL commits at ts=110: t now has two
T1  inserts a row at ts=120, writing one index entry, because that is what its schema says
```

`T1` never *wrote* the catalog, it only read it, and Percolator detects write-write conflicts
(`docs/txn-spec.md` §5) — so nothing conflicts and both transactions commit. The second index is
now missing an entry for a row that exists. Making writers write the catalog version key would
close it and would also serialise the entire cluster behind one key, which is not a trade anybody
should take.

### Why nullable `ADD COLUMN` escapes it

Run the same interleaving with a column instead of an index. `T1` writes a two-column row after
the table became three columns. A later reader with the three-column schema decodes it and pads the
third to NULL (ADR 0019), which is the same answer it would have given if `T1` had known about the
column and left it NULL. And the reverse cannot happen: a reader whose catalog view predates the
`ALTER` has a snapshot below the DDL's commit ts, so it cannot see any row written by a transaction
that started above it.

So a nullable `ADD COLUMN` is safe in one step for a reason that is a property of the *format*, not
of the protocol: **both schemas read every row the other writes, identically.** That is the
condition, and it is worth stating as one because it is the exception. `ADD INDEX`, `DROP COLUMN`,
`ADD COLUMN` with a `DEFAULT` that must be materialised, a type change and `DROP INDEX` all fail
it, each in its own direction, and each therefore needs the protocol below.

## Decision: F1's four states, with our catalog as the schema store

Every index and every column carries a **state** alongside its definition, and a schema change
moves it one state at a time. The states, for something being *added* (a removal runs them
backwards):

| State | Reads | Inserts / updates | Deletes |
|---|---|---|---|
| **absent** | no | no | no |
| **delete-only** | no | no | **yes** — removes the entry if one is there |
| **write-only** | no | **yes** | yes |
| **public** | **yes** | yes | yes |

Between write-only and public there is a **backfill**: entries for the rows that predate
write-only.

### What each intermediate state prevents, concretely

The states are not ceremony. Each one exists because the pair of states on either side of it is
safe together and the pair you would get by skipping it is not. Take `CREATE INDEX i ON t (a)` with
two nodes, A ahead and B behind.

**Skip delete-only** (absent → write-only). A is write-only and inserts row `r`, writing index
entry `e`. B is still absent and deletes `r`; knowing nothing of `i`, it leaves `e`. Now `e` points
at a row that is not there, and the moment the index goes public a scan through it returns a row
the table does not contain — a phantom, produced by a query the user would call correct.
Delete-only exists so that *every* node removes entries before *any* node creates them.

**Skip write-only** (delete-only → public). B, in delete-only, inserts a row and writes no entry.
A, public, answers `SELECT ... WHERE a = ...` from the index and does not find it. The row exists
and the query says it does not. Write-only exists so that *every* node maintains the index before
*any* node trusts it.

**Skip the backfill** (write-only → public with the old rows unindexed). Every row written before
write-only is invisible to an index scan, which is the same wrong answer as above with a wider
blast radius. The backfill exists so that the index is *complete* before it is *trusted*.

These are the same three anomalies for a column being dropped, read backwards: a column that goes
public → absent in one step is read by a node that still thinks it is there, out of rows a node
that thinks it is gone has already rewritten without it.

> **Amended (phase 6e): what the removal direction costs that the adding one does not.** `DROP
> INDEX CONCURRENTLY` runs `public → write-only → delete-only → absent` and only *then* takes the
> entries and the definition away. The three state moves are bounded by exactly what an adding
> change's are — a **writer** one step behind, whose lifetime is the lock TTL — so they wait the
> ordinary interval.
>
> The **final removal** is bounded by something else, and this is where the safepoint term in the
> step interval goes live. A transaction that began while the index was `public` reads through it,
> and its catalog *and* its entries are both at its own snapshot — so it keeps answering correctly
> after the entries are deleted, because MVCC keeps the versions it can see
> (`crates/esker-sql/tests/schema_change.rs::an_old_reader_at_public_still_reads_entries_a_removal_deleted`
> is that fact). What bounds *that* reader is retention: a read below the GC safepoint is refused
> (`docs/txn-spec.md` §7).
>
> So waiting the retention window before the removal means no live reader can still be at `public`
> when the entries go, and correctness stops resting on retained versions that a shorter retention
> would silently take away. That is `removal_extra_ms` in [ADR 0028](0028-the-schema-lease.md), and
> it is **separate** from the ordinary interval rather than folded in: an adding change never needs
> it, and retention is an hour by default.

## The rule that makes one state at a time enough

**At most two adjacent states may be in use in the cluster at any instant.** That is F1's
two-version invariant, and everything below is how we get it out of machinery that already exists.

**A cached schema has a lease.** A node may answer from a cached definition only while its lease is
unexpired; past that it must re-read the catalog version before it serves anything.

> **Amended (phase 6e).** This paragraph originally said the lease was the deadline by which a node
> is guaranteed to have noticed, and implied that nothing else bounded a writer's staleness.
> Reading `crate::catalog` to build the thing showed otherwise: `Catalog::view` reads the
> `catalog_version` key **inside the transaction, at that transaction's own snapshot**, on every
> transaction. So a writer's schema is never older than its own `start_ts`, and its lifetime is
> already bounded by the lock TTL — a step interval above that gives the two-version invariant on
> its own.
>
> The lease is therefore not the *primary* bound; it is three other things, and they are worth
> having: it makes a node cut off from PD **stop writing**, so the step clock can advance on a
> timer rather than on a poll of nodes it may not be able to reach; it bounds the damage if a
> future edit ever caches a definition *across* transactions, which would break the property above
> silently; and it is a published number PD can put in the step arithmetic instead of a constant
> somebody tunes. The property it backs up is now asserted by a test rather than inherited
> (`docs/plans/phase-6e.md` §8, test 1).

**A schema-change step waits longer than a lease plus the longest transaction.** Then no
transaction can still be running under a state two steps behind. Both bounds are already published
by PD and already enforced:

* a **writing** transaction's lifetime is bounded by its lock TTL — past it a resolver rolls the
  transaction back (`docs/txn-spec.md` §5.2), so its writes cannot land;
* a **read-only** transaction's lifetime is bounded by the **GC safepoint**: a read at a ts below
  the safepoint is refused (§7). The safepoint distance is the retention window of
  [ADR 0021](0021-time-machine.md), which makes the travel window and the maximum stale-schema
  window the same number — worth knowing before either is tuned.

So a step interval of `lease + max(lock TTL, safepoint distance)` is sufficient, and PD is where
that arithmetic belongs.

> **Amended (phase 6e), twice.**
>
> **The safepoint term is inert for something being *added*.** A reader that sees an index as
> `public` does so from a snapshot above the DDL that made it public, which is above the backfill
> too — so the index it reads is complete at any age, and a reader further behind simply does not
> use it. The term goes live for a **removal**, where a reader at `public` reads entries a node at
> `absent` has already deleted and MVCC retention is what keeps them readable. Keeping the term
> unconditionally is not free: retention defaults to an hour, so it would make every schema change
> take one. PD is told which direction a job runs and drops the term when it cannot bite.
>
> **PD does not own both inputs today.** The lock TTL is `esker_client::LOCK_TTL_MS`; the GC
> safepoint is set *store-side* by whoever sends `TxnKvReq::GcSafepoint`, and nothing computes it.
> PD is still the right owner — it is a cluster-wide number with one writer, the same argument as
> the safepoint's own — but publishing it is an addition this phase makes rather than a fact to
> build on.

**A schema-change step is itself an ordinary transaction**, so two concurrent schema changes on one
table conflict on the catalog version key and one retries — which is the serialisation the catalog
already has (`crate::catalog::bump_version`) and the reason it is not a bottleneck worth removing.

**A stale reader is safe without a lease, and a stale writer is not.** This is worth separating,
because it is what tells us the lease is about *writes*. A transaction's catalog read is at its own
snapshot, so a reader always sees a schema consistent with the rows it can see. It is the writer
that can act on a schema older than the one the cluster has moved to, and the states are what make
that harmless for one step's worth of staleness.

## The backfill is ordinary transactions

Nothing new. The backfill scans the table's row range and writes index entries with the same
`scan`/`put`/`commit` the executor already uses (`crate::backend`), in **many small transactions
rather than one**: a single transaction over a large table would hold locks for its whole duration,
conflict with everything, and exceed the lock TTL that the step interval above depends on.

So it is a resumable job: a batch is `[cursor, cursor + n)` of the row range, one transaction per
batch, and the cursor is durable so a node that dies resumes rather than restarts. Two properties
make it safe to run concurrently with live traffic, and both are already true:

* it runs while every node is at **write-only**, so a row written or deleted during the backfill is
  maintained by its own writer. The backfill and the writer may both write the same entry;
  identical values, and a lost race is an ordinary conflict-and-retry.
* a `UNIQUE` index whose backfill meets a duplicate fails the whole change with `23505`, exactly as
  `CREATE INDEX` does today over rows that already violate it — which is the one place a schema
  change can fail on *data* rather than on a conflict, and the user has to be told which row.

`CREATE INDEX` today does this in one transaction inside the statement (`exec::ddl::backfill`) and
is honest about it: it is `TODO(post-v1)` for a table large enough to matter. The staged protocol
is how that TODO closes.

## What each crate has to grow

* **`esker-sql`** — the bulk of it. A `state` on every `IndexDef` and `ColumnDef` plus the schema
  version it entered (the `TableDef::schema_version` field ADR 0019 added is what these hang from);
  the planner refusing to choose a non-public index; the DML maintaining write-only and delete-only
  indexes on insert, update and delete; the job that drives the states and the backfill; and
  `CREATE INDEX` becoming a job rather than a statement that finishes.
* **`esker-pd`** — the step clock. It already publishes the GC safepoint and already schedules
  operators (`crates/esker-pd/src/schedule.rs`), and a schema-change job is the same shape: durable
  state, a step it may take when a precondition holds, and a report. The schema **lease** is also
  PD's to publish, for the same reason the safepoint is: it is a cluster-wide number with one
  writer.

  > **Amended (phase 6e): PD publishes the interval and does not drive the steps.** The arithmetic
  > is PD's — a cluster-wide bound needs one writer — but driving is not. A step is a *catalog
  > transaction*; the catalog is `esker-sql`'s; and PD is byte-opaque by `CLAUDE.md` invariant 7,
  > so it cannot read a table definition, let alone write one. Giving PD the drive would mean
  > giving PD key semantics, which is the one thing invariant 7 exists to prevent.
  >
  > So the job record lives in the catalog where every node can see it, and a node takes the steps
  > using PD's published interval as the wait. `esker_schema_step('<index>')` is the step with the
  > wait taken out, which is also what makes the state machine testable without a timer.
  >
  > **A consequence, stated rather than discovered: an orphaned job stalls.** If the node that
  > started a `CREATE INDEX CONCURRENTLY` dies, nothing re-drives the job on its own — it sits at
  > whatever state it reached until *any* node or an operator calls `esker_schema_step`. **Nothing
  > is lost and nothing is unsafe**: the job record and its cursor are durable, the index is not
  > readable until it is `public`, and every node maintains it at whatever state it is stuck in, so
  > a stalled job is a slow schema change and never a wrong answer. `docs/plans/phase-6e.md` §10.
  >
  > **Amended again (debt wave): it no longer stalls, and "a human notices" was never a liveness
  > mechanism.** Every SQL node runs a re-driver (`crates/esker-sql/src/exec/redrive.rs`): a
  > background pass that scans the jobs and steps any whose fingerprint — state, cursor, done —
  > has not changed for a whole pass. The pass period *is* the step interval, so a job that
  > survives one has been still for at least an interval and the wait an interactive driver takes
  > has already been taken. Idleness is counted in passes rather than measured on a clock, which
  > keeps invariant 7's wall clock out of it and keeps a "stepped at" timestamp out of the catalog
  > record.
  >
  > **Nothing coordinates the re-drivers, because a step is already a catalog transaction.** Two
  > that overlap write the same table record and one is rolled back with `40001`; one that merely
  > *follows* another is refused by the step itself, which takes the state its caller expected and
  > answers `overtaken` if it has moved. Both halves are needed: without the second, two drivers
  > that never overlap take consecutive transitions moments apart, each legal alone and together
  > exactly the acceleration this interval forbids. A lock would be a second mechanism to keep in
  > step with the first, and it would have a holder that can die — which is the failure being
  > survived.
  >
  > A node that cannot be told the interval does not re-drive at all, and that is the opposite
  > default to the lease's on purpose: a node with no lease source still writes, because "nobody is
  > coordinating" is not a reason to stop, but stepping without an interval means inventing one and
  > an invented interval that is short is the unsafety the number exists to prevent.
  > `docs/plans/debt-c2.md`.
* **`esker-proto`** — the messages PD needs to hand a schema-change job out and collect its
  progress, and the lease. **Amended (phase 6e):** the lease is a method of its own,
  `Pd::SchemaLease` (0x0307), rather than a field on a message PD already sends — PD sends a SQL
  node nothing, and adding a field to `PdResp::Tso` would have changed a wire format with a golden
  test. [ADR 0028](0028-the-schema-lease.md) has the argument.
* **`esker-client`** — nothing. The backfill is `TxnClient` used the way everything else uses it.
* **`esker-store`, `esker-txn`, `esker-engine`** — nothing. Every layer below the catalog is
  byte-opaque (`CLAUDE.md` invariant 7) and a schema change is keys and values like any other.

The honest size: this is a phase of its own, not a unit. The part that is genuinely hard is not the
state machine — it is four states and a table — but the lease, because a lease is a liveness
mechanism with a safety consequence, and the failure it has to survive is a node that stops hearing
from PD and keeps serving writes. That node has to *stop*, and stopping a node that believes it is
healthy is the thing distributed systems are worst at.

## What is deliberately not decided here

The **DDL surface**. `DROP COLUMN`, `ALTER COLUMN TYPE` and `CREATE INDEX CONCURRENTLY` stay
`0A000` naming themselves until this exists, and which of them lands first is a scheduling
question, not an architectural one. `DROP COLUMN` additionally needs a row format that carries
column *identity* rather than a count, which ADR 0019 names as a future version 3.
