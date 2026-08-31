# Phase 6e plan — online schema change

Status: **complete**, units 0–7 — written before implementation; §9 records progress and §10
what changed.
Design: [ADR 0020](../adr/0020-online-schema-change.md), resting on
[ADR 0019](../adr/0019-a-row-says-how-many-columns-it-has.md) (the row format) and
[ADR 0021](../adr/0021-time-machine.md) (retention, which turns out to be the same number as the
stale-schema window). Milestone: `docs/plans/phase-6a.md` §12, M1. Constitution: `CLAUDE.md`.
The compatibility contract this inherits whole: `docs/plans/phase-6a.md` §1 (C1, C2, C3).

`ALTER TABLE ADD COLUMN` of a nullable column is instant because **both schemas read every row the
other writes, identically** — a property of the row format, not of a protocol. Nothing else has it.
This phase builds the protocol for the things that do not: four states per index, a step clock in
PD, a lease that stops a node acting on a schema the cluster has left behind, and a backfill that
is a resumable job rather than one enormous transaction.

Lanes: `crates/esker-sql/**`, `crates/esker-pd/**`, `crates/esker-proto/**`. `esker-client`,
`esker-store`, `esker-txn`, `esker-engine` and `esker-raft` grow **nothing** — ADR 0020 says so and
this plan holds itself to it; if one of them turns out to need a change, that is a report and not a
commit.

## 1. The safety argument, restated — and two corrections to ADR 0020

ADR 0020's dangerous interleaving is a **writer acting on a schema the cluster has moved past**:

```text
T1  begin at ts=100, reads the catalog: table t has one index
DDL commits at ts=110: t now has two
T1  inserts a row at ts=120, writing one index entry
```

Nothing conflicts, because `T1` only *read* the catalog. The four states make one step of that
staleness harmless; the step clock is what keeps it to one step.

**Correction 1: the bound on a writer's schema age is already there, and it is the lock TTL.**
`Catalog::view` reads the `catalog_version` key **inside the transaction, at the transaction's own
snapshot**, on every transaction (`crates/esker-sql/src/catalog/mod.rs`). So a writer's schema is
never older than its own `start_ts`, and its lifetime is bounded by the lock TTL: past it a
resolver rolls it back and its writes cannot land (`docs/txn-spec.md` §5.2). A step interval above
the lock TTL therefore already gives "at most one step behind", which is the two-version invariant.

That is a property nothing currently asserts, and this phase asserts it (§8, test 1) rather than
inheriting it. It also says what the lease is **for**, which is not what the ADR implies — see §3.

**Correction 2: the GC safepoint belongs in the step interval only for a *removal*.** ADR 0020 puts
`max(lock TTL, safepoint distance)` in the formula on the strength of read-only transaction
lifetimes. For something being **added** the safepoint term is inert, and the reason is worth
writing down: a reader that sees an index as `public` does so because the DDL that made it public
committed below the reader's snapshot, which means the backfill finished below it too — so the
index it reads is complete at any age. A reader behind that simply does not use the index.

For a **removal** the term is real: a reader at `public` reads entries a node at `absent` has
already deleted, and what keeps those entries readable is MVCC retention — the safepoint. So the
formula keeps the term, with the reasoning attached, and the first DDL staged here is an *add*,
where it costs nothing.

This matters practically: retention defaults to **one hour**
(`esker_sql::catalog::DEFAULT_RETENTION_MS`), so a formula that always included it would make every
schema change take over an hour. The acceptance numbers in §9 are produced with a test-sized
retention and lease, and the plan says which.

**Correction 3, smaller: PD does not publish the safepoint today.** ADR 0020 says "both bounds are
already published by PD". The lock TTL is `esker_client::LOCK_TTL_MS`; the safepoint is set
*store-side* by whoever sends `TxnKvReq::GcSafepoint`, and nothing computes it. PD is still the
right owner and this phase makes it one, but it is an addition rather than a fact.

## 2. What is staged first, and what stays refused

**`CREATE INDEX` on a populated table** — ADR 0020's own worked example, and the one that closes
`exec::ddl::backfill`'s `TODO(post-v1)`. `DROP COLUMN` stays `0A000`: it needs a row format that
carries column *identity* rather than a count (ADR 0019 Decision 3, a future version 3), and that
is explicitly out. `ALTER COLUMN TYPE` stays `0A000`.

`DROP INDEX` runs the states backwards and is the natural second. **Built, as unit 8** — the
removal direction is what makes the safepoint term live, which is why it got a unit rather than a
rider on the adding one.

## 3. The lease, designed here because it is the one new safety property

### What it is

PD publishes a **schema lease**: a duration, and the deadline it implies for a node. A SQL node may
serve a **write** only while it holds an unexpired lease; past it, it must reach PD again before
writing. Reads are never gated (§1 correction 2, and ADR 0020's own "a stale reader is safe").

### What it protects, honestly

Given §1's correction, the per-transaction snapshot read already bounds a writer's staleness. The
lease is not redundant, and the case it covers is the one the ADR calls the hard part: **a node that
has stopped hearing from PD and keeps serving writes.** Concretely, three things:

1. **Fail-closed under partition.** A node cut off from PD must stop writing, so that PD's step
   clock can advance on a timer rather than on a poll of every node it may not be able to reach.
   Without it PD could only ever advance by waiting out a number nothing enforces.
2. **A backstop under correction 1.** "The catalog version is read at the transaction's snapshot on
   every write path" is a property of code that a future edit can break — a cached `TableDef` held
   across transactions would break it silently and produce exactly the missing index entry this ADR
   exists to prevent. The lease bounds the damage of that class of bug to one lease term.
3. **A number PD can put in the step arithmetic.** The interval is a computed function of published
   numbers rather than a constant somebody tunes, which is the brief's own requirement.

### How it travels: on the timestamp, because every writer needs one

A SQL node needs a timestamp from the oracle to begin any transaction and another to commit one, so
**every writer talks to PD by construction**. The lease therefore rides on the TSO answer:

```rust
// esker-proto: PdResp::Tso gains one field.
PdResp::Tso { start_ts: u64, count: u32, schema_lease_ms: u64 }
```

That is the whole of the mechanism, and it is what makes fail-closed *free* rather than a second
thing to get right: a node that cannot reach PD cannot get a timestamp, cannot begin a transaction,
and therefore cannot write. There is no separate liveness path to go wrong.

**The wire change is one field on one message.** `esker-proto` is in this lane and `PdResp::Tso` has
a golden test, so this is a format change with an ADR (`docs/adr/0028-the-schema-lease.md`, written
with the unit that lands it) and a golden that updates alongside a new-version golden.

The node side is a `PdOracle` implementing `esker_client::TimestampOracle` — implementing another
crate's trait, not changing it — that stamps `Instant::now() + schema_lease_ms` on each answer and
hands the deadline to the executor. `esker-sql`'s `connect()` builds a `CountingOracle` today under
a `TODO(phase-6a)`; this replaces it when PD's address is given and keeps the counting one for the
in-process fake, which is what the existing tests run on.

### The step interval

```text
step_interval_ms = schema_lease_ms
                 + max(lock_ttl_ms, safepoint_distance_ms)
```

Computed in PD from three published numbers, never a constant. `safepoint_distance_ms` is the
retention window; for an *add* it is inert (§1) and PD is told which direction a job runs so it can
say so rather than sleeping through an hour it does not need.

## 4. Catalog v3 — pre-approved, and what it holds

`IndexDef` and `ColumnDef` gain two fields:

```rust
pub struct IndexDef { /* ... */ pub state: SchemaState, pub state_since: u64 }
pub struct ColumnDef { /* ... */ pub state: SchemaState, pub state_since: u64,
                       /// PostgreSQL 11's missing value; see §5 unit 1.
                       pub missing: Option<Datum> }

pub enum SchemaState { Absent, DeleteOnly, WriteOnly, Public }
```

`state_since` is the `TableDef::schema_version` the state was entered at — the field ADR 0019 added
for exactly this, and the number ADR 0020's two-version invariant is stated over.

`CATALOG_FORMAT_VERSION` goes to **3**, and unlike versions 1→2 there **is** version 2 data now
(this crate has a real backend), so v2 decodes: a v2 record reads as every index and column
`Public` with `state_since = schema_version`, which is what a table that has never staged a change
means. Goldens for both, and a test that a v2 byte string still decodes — the format change costs a
golden update *plus* a new golden, never a replacement.

## 5. Units, in order, each its own commit

### Unit 1 — `ADD COLUMN ... DEFAULT <constant>`, the PostgreSQL 11 way

No backfill, no protocol, no rewrite: the default is stored as a **missing value** on the column and
readers pad an absent column with it, which is ADR 0019's NULL pad rule generalised. Captured from
the server rather than recalled (§7):

| What | PostgreSQL 19beta1 |
|---|---|
| `ADD COLUMN c text DEFAULT 'old'` on two rows | both read `old`; `pg_attribute.atthasmissing = t`, `attmissingval = {old}` |
| then `ALTER COLUMN c SET DEFAULT 'new'` | old rows **still** `old`; `attmissingval` unchanged; a new row gets `new` |
| `ADD COLUMN n int8 NOT NULL DEFAULT 7` | missing value `{7}`, **no rewrite** — so `NOT NULL` with a constant default is instant |
| `ADD COLUMN e int8 DEFAULT (1+1)` | folded to `{2}` |
| `ADD COLUMN r float8 DEFAULT random()` | `atthasmissing = f` — PostgreSQL **rewrites the table** |

So: a constant default is stored and frozen at `ADD COLUMN` time; a later `SET DEFAULT` must **not**
touch it. A volatile default is `0A000` naming why — the rewrite it needs is the job unit 4 builds,
and until then answering it instantly would be a wrong answer rather than a missing feature.

This also **closes a divergence**: `ADD COLUMN ... NOT NULL` is currently `0A000` even on an empty
table (`docs/plans/phase-6a.md` §10a). With a missing value, `NOT NULL DEFAULT <constant>` is
instant and correct, and only bare `NOT NULL` with no default stays refused.

### Unit 2 — the states in the catalog (v3)

The four states on defs; the planner refuses to choose a non-public index; the DML maintains
delete-only (delete removes) and write-only (insert, update and delete all maintain) indexes. Two
in-process caches at different states, driven directly, with one test per ADR anomaly (§8).

### Unit 3 — the lease and the step clock

§3, built: the field on `PdResp::Tso`, the `PdOracle`, the executor's write gate, PD's job record
and its computed interval. PD's job state is durable — it already fsyncs `alloc` and the TSO mark —
so a crash of PD or of a node resumes the job and never skips a state.

### Unit 4 — the backfill as a resumable job

Batched `[cursor, cursor + n)` transactions through the ordinary executor path, cursor durable in
the catalog beside the job. Resume, not restart. A `UNIQUE` duplicate fails the whole change with
`23505` naming the row, and the states unwind.

### Unit 5 — `CREATE INDEX` becomes the job

`CONCURRENTLY` accepted as the same thing; plain `CREATE INDEX` may block-wait on the job, which is
PostgreSQL's own difference between the two. `esker_schema_jobs()` so a human at `psql` can watch
states advance — the same table-function shape phase 6d used for `esker_checkpoints()`.

### Unit 6 — parity and `.slt`

Capture what PostgreSQL does for the new surface, every divergence into `docs/plans/phase-6a.md`
§10a's table, and a `.slt` file that runs under both runners and against real stores. Oracle-
independent, for the reason phase 6d found the hard way.

### Unit 7 — stretch: `FLASHBACK TABLE`

ADR 0021's fourth verb, deferred in phase 6d for exactly the durable-cursor machinery unit 4 builds.
Only if units 1–6 are green and there is budget.

## 6. What this phase will NOT do

* **`DROP COLUMN`** — needs row format v3 (column identity), ADR 0019 Decision 3. Stays `0A000`.
* **`ALTER COLUMN TYPE`** — same answer, different reason: a type change is a rewrite, and a rewrite
  is unit 4's machinery pointed at every row rather than at an index.
* **Anything in `esker-client`, `esker-store`, `esker-txn`, `esker-engine`, `esker-raft`.** ADR 0020
  says they grow nothing. If one of them must, that is a report.
* **A rewrite for a volatile default.** Unit 1 refuses it by name.

## 7. Risks

* **A lease that protects nothing.** Guarding *reads* with it adds stalls and closes no hole (§1).
  The gate is on writes only, and the test that proves it is a read succeeding past an expired
  lease.
* **A lease that is weak where it matters.** A node that cannot reach PD must refuse writes, not log
  and continue. Riding the lease on the timestamp makes that structural rather than a check
  somebody can forget, and the test kills PD and asserts the refusal.
* **The step clock advancing while a node is two behind.** The interval is a computed number with
  its three inputs named, and a test drives a node deliberately two steps behind and asserts the
  clock refuses to advance.
* **A backfill in one transaction.** It would exceed the lock TTL the step arithmetic depends on and
  conflict with everything. Batched, with the batch size a named number and a test that a table
  larger than one batch is fully indexed.
* **A v2 record that stops decoding.** The format change adds a golden rather than replacing one.
* **Another lane owns `crates/esker-columnar`.** `cargo fmt --all` is forbidden here; this lane
  formats its own crates by name.

## 8. Test list

Written before the code. The three anomaly repros are mutation-checked: each must **fail** if the
state that guards it is skipped, which is what makes them tests of the rule rather than of the code.

1. **The property correction 1 rests on**: every write path reads the catalog version inside its own
   transaction, at its own snapshot. Asserted by a backend that counts version-key reads.
2. **Skip delete-only** (absent → write-only): node A at write-only inserts `r` and writes entry
   `e`; node B at absent deletes `r`. With delete-only, `e` is gone. Without it, `e` survives and a
   scan through the index returns a phantom row.
3. **Skip write-only** (delete-only → public): node B at delete-only inserts a row and writes no
   entry; node A at public reads through the index and does not find it.
4. **Skip the backfill** (write-only → public with old rows unindexed): a row written before
   write-only is invisible to an index scan.
5. `ADD COLUMN ... DEFAULT <constant>` reads back on rows that predate it, at any width.
6. `ALTER COLUMN SET DEFAULT` does not disturb the stored missing value — PostgreSQL parity.
7. `ADD COLUMN ... DEFAULT random()` is `0A000` naming volatility.
8. A v2 catalog record still decodes, as everything `Public`.
9. The step interval is the three inputs, computed; changing an input changes it.
10. PD refuses to advance a step while a node could be two states behind.
11. A node past its lease refuses a **write** and still serves a **read**.
12. A node that cannot reach PD refuses writes.
13. The backfill resumes from its cursor rather than restarting, after a kill mid-job.
14. Concurrent DML and backfill writing the same entry converge.
15. A `UNIQUE` backfill meeting a duplicate fails the change with `23505` naming the row, and the
    states unwind.
16. A table larger than one batch is completely indexed.
17. `esker_schema_jobs()` shows a job advancing through its states.
18. The corpus: every new statement parses, and every refusal is by name.

## 9. Progress

- [x] 6 — **parity and `.slt`.** `CREATE INDEX CONCURRENTLY` out of the unsupported list and into
  `tests/slt/index.slt` as a driven job; four new corpus statements verified against 19beta1; two
  new divergence rows in `docs/plans/phase-6a.md` §10a.
- [x] 5 — **`CREATE INDEX` becomes the job.** `CONCURRENTLY` declares the index at `absent` and
  records a job; `esker_schema_step('<index>')` takes one step and says which; `SELECT * FROM
  esker_schema_jobs()` is the `psql`-visible progress.
- [x] 4 — **the backfill as a resumable job.** Batched `[cursor, cursor + 256)` transactions with
  the cursor durable in the catalog, resume-not-restart across sessions, live DML converging with
  the backfill, and a `UNIQUE` duplicate failing the whole change and unwinding the states.
- [x] 3 — **the lease, and the step arithmetic.** A new PD method `SchemaLease` (0x0307) rather
  than a field on `Tso`; PD computing `lease + lock_ttl` and reporting the removal term separately;
  the executor refusing every **write** past the lease and serving every read. Two new golden lines
  and **no existing golden byte moved**. `docs/adr/0028-the-schema-lease.md` records the decision.
  PD *driving* the job is folded into unit 5, where there is a job to drive.
- [x] 2 — **the four states.** `SchemaState` on every `IndexDef` with the schema version it was
  entered at, in catalog v3 beside unit 1's fields and behind the same v2 fallback; the planner
  refusing a non-public index; the DML writing entries at write-only and public and removing them
  from delete-only on. `advance_index_state` refuses a two-step move, so no caller can break the
  two-version invariant. Six new tests including all three anomaly repros, each mutation-checked.
- [x] 1 — **`ADD COLUMN ... DEFAULT <constant>`, the PostgreSQL 11 way.** Catalog v3 with a
  `default` and a `missing` on every column, v2 still decoding and both goldens kept;
  `RowSchema` so a types list and a pad list cannot drift apart; the insert path filling omitted
  columns from the default. `NOT NULL DEFAULT <constant>` is instant, which narrows a divergence
  §10a of phase 6a records. 12 new tests, 5 new corpus statements, `.slt` updated.

## 10. What changed from this plan

**The wait belongs between state *transitions*, not between backfill batches** — and the number is
what showed it. A 20,000-row backfill is 82 steps, of which 79 are batches; waiting the eight-second
interval after every step is **656 seconds**, and waiting only after the three transitions is
**24 seconds**. Same safety: every batch runs at write-only, so no state moves and no node can fall
a step behind while they run. `esker_schema_step`'s answer says which kind of step it took, so a
driver can tell.

**`DROP INDEX CONCURRENTLY` does not parse, and PostgreSQL accepts it — so the source is rewritten.**
`sqlparser` 0.62.0's `Statement::Drop` has no `concurrently` field and the parser stops at the
keyword. Unlike `AS OF SYSTEM TIME` or `FLASHBACK TABLE`, this is a spelling PostgreSQL 19 *takes*,
which makes it a contract C1 gap rather than a syntax question — so it uses the mechanism this crate
already had for that, `rewrite_synonym`'s: strip the word from the source and carry the fact on
`Parsed`. Carried rather than inferred, because a rewrite that dropped it silently would give the
**blocking** drop to somebody who asked for the concurrent one. Gap register row G05 narrowed.

**The `25001` check has to run before `in_a_transaction`, not inside it.** That function *takes*
the open transaction out of `self` before running the statement, so a check for "am I in a block"
further down always reads `None`. Found by the test, which is the only way it would have been.

**`FLASHBACK` is `esker_diff` applied backwards, and reusing the merge is the point.** A flashback
that disagreed with the diff about what changed would be a flashback whose preview was a lie. Its
own module rather than a mode of the diff, because one writes and the other does not, but the walk
is the same walk.

**A flashback batch reads both sides to the *smaller* of the two pages' last keys.** A key past the
end of one page might exist on the other, and calling it an insert or a delete would be deciding a
comparison from half the evidence. Both sides resume from the same boundary next time.

**Its record stores the target, not just the cursor** — a resume that picked up a cursor without
checking what it was a cursor *for* would finish somebody else's flashback with its own target and
leave the table in a state it was never in. Calling `esker_flashback` with a different snapshot
while one is in flight is refused by name.

**Unlike the schema job, a flashback runs its own batches to completion** rather than needing a
driver. It is a data operation the user is waiting on rather than a schema change the cluster has to
agree about, so there is no interval to wait between batches — but the cursor is still durable, so a
node that dies mid-flashback leaves one and the next call resumes.

**An orphaned job stalls, and that is a liveness gap rather than a safety one.** If the node that
started a `CREATE INDEX CONCURRENTLY` dies, nothing re-drives the job by itself: it sits at whatever
state it reached until any node or an operator calls `esker_schema_step('<index>')`. Nothing is lost
— the job record and its cursor are durable, and a different session finishing a half-done job is
one of the tests. Nothing is unsafe either: the index is not readable until `public`, and every node
maintains it at whatever state it is stuck in, so a stalled job is a slow schema change and never a
wrong answer. ~~Automatic re-drive is future work.~~

> **Closed in the debt wave.** Every node runs a re-driver; a job idle for a whole pass is picked up
> by whoever sees it, and the pass period is the step interval so the wait is taken by construction.
> Two bugs that only appear with more than one driver came out of it: a step decided against one
> state could be applied to a later one, and a backfill batch that lost an ordinary write-write race
> unwound the entire change instead of retrying it. `docs/plans/debt-c2.md`, ADR 0020 as amended
> again.

**PD publishes the interval; a SQL node drives the steps.** ADR 0020 puts the step clock in PD, and
the arithmetic *is* PD's — a cluster-wide bound needs one writer. Driving is not: a step is a catalog
transaction, the catalog is `esker-sql`'s, and PD is byte-opaque by `CLAUDE.md` invariant 7 and
cannot read a table definition, let alone write one. So the job record lives in the catalog where
every node can see it, and the step is a verb rather than a timer — which is also what makes the
whole state machine testable without one.

**The lease is a new method, not a field on `Tso` — and the constitution is what bought the better
design.** The plan proposed piggybacking on the timestamp, which would have made fail-closed free.
`CLAUDE.md` says to stop and ask before changing a wire format with a golden test, and `PdResp::Tso`
has one; a widened lane is not a blanket permission. Looking for a shape that did not need the
change produced an *additive* one: no existing message's bytes move, every existing golden is
byte-identical, and the extra round trip is once per lease period rather than once per timestamp —
cheaper than the rejected shape rather than dearer. `docs/adr/0028-the-schema-lease.md`.

**PD is told the step interval's inputs rather than keeping copies of them.** The lock TTL is the
client's and the retention window is the SQL layer's; both crates sit above PD, which does not link
them. Copies drift, and a step interval short by exactly the drift is unsafe rather than merely
wrong. They are `PdOptions` fields whose defaults name their sources; the interval itself stays
computed.

**The lease source is a trait, not a `PdClient`**, because the test that proves fail-closed has to
be able to stop answering. Untested fail-closed is fail-open with good intentions. And a node with
*no* lease source writes freely — "nobody is coordinating" and "I have lost the thing that
coordinates" are different facts, and only the second is a reason to stop.

**ADR 0027 was taken by the columnar lane**, which committed first, so this is 0028
([[esker-adr-numbering-rule]]).

**Delete-only is unreachable through the executor, and that is correction 1 taken to its
conclusion.** The ADR's "skip delete-only" story needs a deleter at an *earlier* state than the
inserter of a row it can see. A transaction's schema is the schema at its own snapshot and states
only move forward, so a transaction that can see a row committed at `S_r` reads a state of at least
`S_r` — and `written` implies `maintained`, so if the insert wrote an entry the delete removes it.
The interleaving cannot be built with two sessions; the second one simply cannot see the row.

That does **not** make delete-only ceremony. It makes it the rule that saves a node whose schema is
stale *independently of its snapshot* — which is precisely the failure the lease bounds and the one
ADR 0020 calls the hard part. So there are two tests: one drives the rule directly by winding the
catalog back, and one asserts *why* the interleaving cannot be built, because that property is what
makes the step arithmetic sufficient and a future cached `TableDef` would break it silently.

**Only a *unique* index has a read path here**, so the "skip the backfill" repro goes through one.
Rules 1 and 3 of `crate::plan::query` choose a point read or a unique-index lookup; a non-unique
index is never read, so it has no way to answer wrongly. The anomaly is observable exactly where the
planner can reach it, which is also the only place it can hurt.

**`ColumnDef` did not get a state.** The plan said both defs; a column state is for the *removal*
direction (`DROP COLUMN`), which is explicitly out of this phase, and ADR 0019 warns in as many
words about a field that looks load-bearing and is not. It lands with the DDL that needs it.

**A column needs two fields, not one, and unit 1 is bigger than "the missing value".** The plan
named `missing`; the measurement said a *default* is a separate thing that PostgreSQL lets diverge
from it (`ALTER COLUMN SET DEFAULT` moves one and not the other). And column defaults did not exist
in this crate at all — `CREATE TABLE t (a int8 DEFAULT 1)` was `0A000` — so `ADD COLUMN c DEFAULT
'x'` giving old rows `'x'` and new rows NULL would have been incoherent. Both fields, on both
statements, is the smallest thing that is not wrong.

**`decode_row` grew a `RowSchema` rather than a second argument.** Four plan nodes carry a column
list, and a parallel pad list would have been four chances to build a node with the wrong pair —
whose symptom is a column reading NULL instead of its default, only for rows written before it was
added, which is the hardest case to notice. One value carries both.

**Version 3 reads version 2 for *every* record kind, not just the table's.** The first version made
`Reader::new` strict and only the table record tolerant, which would have made a phase-6d cluster's
retention and checkpoint records `DATA_CORRUPTED` on upgrade — a break caused by a layout change
that does not touch them. The version byte names the catalog format; only the table record's layout
moved.
