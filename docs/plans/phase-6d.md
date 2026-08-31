# Phase 6d plan — the time machine, as a SQL surface

Status: **in progress** — written before implementation; §8 records progress and §9 what changed.
Design: [ADR 0021](../adr/0021-time-machine.md). Milestone: `docs/plans/phase-6a.md` §12, M2.
Constitution: `CLAUDE.md`. The compatibility contract this inherits whole: `docs/plans/phase-6a.md`
§1 (C1, C2, C3).

The storage layer has been a time machine since phase 5 and nothing above it exposed that. Every
version of every key is filed under `txn_key(user_key, commit_ts)` and a read at `T` is "the newest
write with `commit_ts ≤ T`", which is what `TxnKv` already does for whatever `start_ts` a
transaction was opened with. This phase builds **the surface, the bound, and two verbs** — and
builds no storage at all.

Lane: `crates/esker-sql/**`. `TxnClient::begin_at` is another lane's and does not exist yet; §3
says what this crate does about that.

## 1. The syntax was measured, and it decided the whole shape

ADR 0021 surveyed `sqlparser` 0.62.0's PostgreSQL dialect and this plan re-ran the survey rather
than trusting it. The result is unchanged and it is the reason this phase has the surface it has:

| Spelling | Whose | `sqlparser` 0.62.0 | PostgreSQL 19beta1 |
|---|---|---|---|
| `SELECT ... AS OF SYSTEM TIME '...'` | CockroachDB | **no** — `Expected: end of statement, found: SYSTEM` | `42601 syntax error at or near "SYSTEM"` |
| `SELECT ... FOR SYSTEM_TIME AS OF '...'` | SQL:2011 | **no** | — |
| `BEGIN AS OF SYSTEM TIME '-1h'` | CockroachDB | **no** | — |
| `CHECKPOINT <name>` | invented | **no** — `Expected: an SQL statement` | `42601 syntax error at or near "nightly"` |
| `SELECT ... AS OF CHECKPOINT '<name>'` | invented | **no** | — |
| `SET TRANSACTION SNAPSHOT '<id>'` | **PostgreSQL's own** | **yes** | executes |
| `SET esker.read_as_of = '...'` | PostgreSQL's custom-GUC namespace | **yes** | accepts and stores it |
| `SHOW` / `RESET esker.read_as_of` | PostgreSQL's own | **yes** | executes |
| `SELECT esker_checkpoint('nightly')` | a function call | **yes** | `42883 function does not exist` |
| `SELECT * FROM esker_diff('t','a','b')` | a table function | **yes** | `42883` |
| `ALTER TABLE t SET (retention = '7d')` | a storage parameter | **yes** | `42601 unrecognized parameter` |

**The decision follows from the table and not from taste.** The three invented spellings are
exactly the three PostgreSQL answers with `42601`. Adding them to the parser would put this node
outside contract C1's own boundary in the one direction the contract does not police — accepting
what the oracle rejects — and it would do it inside ADR 0014's containment, which is the thing
`sqlparser` was taken *for*. Everything this phase needs already parses, as PostgreSQL, and needs
**not one line inside `src/parse/`'s grammar**.

So: `AS OF SYSTEM TIME` stays refused, and it is refused *by name* with a `HINT` naming the
spelling that works. That is the honest reading of the brief's "never a bare syntax error": the
statement is not valid PostgreSQL either, so `42601` is parity — and a `HINT` pointing at
`SET esker.read_as_of` turns a dead end into a redirect. CockroachDB's syntax is the **reference
for the semantics**, cited in the docs, and not the surface.

## 2. The error surface is PostgreSQL's, captured rather than recalled

Every row below came off the `esker-pg19` container on 2026-08-31, not out of memory. It is the
whole specification of unit 1's behaviour, and the reason the ADR chose `SET TRANSACTION SNAPSHOT`:
the preconditions of a feature PostgreSQL already has are the preconditions this feature wants.

| Condition | PostgreSQL 19beta1 |
|---|---|
| `SET esker.read_as_of = '-1h'` | accepted, silent |
| `SHOW esker.read_as_of`, after a `SET` | one row, column named `esker.read_as_of` |
| `SHOW esker.read_as_of`, after `RESET` | one row, **the empty string** |
| `SHOW esker.never_set` | `42704 unrecognized configuration parameter "esker.never_set"` |
| `SET nonamespace_thing = '1'` | `42704 unrecognized configuration parameter` |
| `SET TRANSACTION SNAPSHOT` outside a block | `WARNING 25P01 SET TRANSACTION can only be used in transaction blocks`, then `ERROR 0A000 a snapshot-importing transaction must have isolation level SERIALIZABLE or REPEATABLE READ` |
| ... after a query in the block | `25001 SET TRANSACTION SNAPSHOT must be called before any query` |
| ... a malformed id | `22023 invalid snapshot identifier: "nope"` |
| ... a well-formed absent id | `42704 snapshot "00000003-0000001B-1" does not exist` |
| `pg_export_snapshot()` | `0000002F-000000BA-1`, i.e. `%08X-%08X-%d` |
| a write in a read-only transaction | `25006 cannot execute INSERT in a read-only transaction` |

**One correction to ADR 0021**, which named `42704` for a bad id: PostgreSQL has *two* answers and
the distinction is real. A string that cannot be a snapshot identifier at all is `22023`; one that
is well formed and simply is not there is `42704`. The ADR is amended in the same commit as the
unit that implements it, because a plan and an ADR that disagree are the drift `CLAUDE.md` forbids.

## 3. The seam: `begin_at`, and building against one that does not exist

ADR 0021 Decision 1: *a historical read is a read timestamp, and nothing else.* The whole of it is
one constructor, and this crate's `backend::Backend` grows the same shape the client will:

```rust
pub trait Backend {
    fn begin(&self) -> Result<Box<dyn Txn>>;
    /// A **read-only** transaction at a snapshot the caller chose.
    fn begin_at(&self, start_ts: u64) -> Result<Box<dyn Txn>>;
    /// The oracle's current timestamp. `CLAUDE.md` invariant 6: never a wall clock.
    fn now(&self) -> Result<u64>;
}
```

`MemoryBackend` implements all three today — it already keys every version by `commit_ts` and
already has a monotone clock standing in for the oracle, so a historical read there is the same
`visible(key, ts)` call with a different `ts`. That is what the unit tests and the whole `.slt`
corpus run against, so the feature is fully exercised before the client half lands.

`StoreBackend` implements `now` today (`TxnClient` holds the oracle; it needs one accessor, which
is this crate asking for a `pub fn` and not a design). `begin_at` is the **stub**: it answers
`0A000 reading as of a past timestamp is not supported by this store yet` until
`TxnClient::begin_at` exists, at which point the body is three lines and nothing above it changes.
Refusing by name is the honest stub — a `TODO` that silently read the present would be the defect
class this crate's lowering exists to prevent.

**Read-only is enforced here, not hoped for.** `begin_at` returns a `Txn` whose `put` and `delete`
are refusals, and the executor checks before it plans: a statement that writes, under a past
snapshot, is `25006` with PostgreSQL's own message naming the command. ADR 0021's argument is that
a commit at `commit_ts > start_ts` against an old snapshot is a lost update Percolator's conflict
check cannot catch, so this is a correctness rule and not a policy.

## 4. Units, in order, each its own commit

### Unit 1 — the historical read, its bound, and the retention DDL

* `SET esker.read_as_of = '<timestamp>' | '<interval>' | DEFAULT`, `SET LOCAL`, `SHOW`, `RESET`.
  An interval is signed and relative to now (`-1h`, `-30m`, `-2d`, `-500ms`); a timestamp is the
  one `crate::value` already parses, so there is one datetime grammar in this crate and not two.
* **Resolved once, at `SET`**, to an absolute timestamp, and `SHOW` hands back the text the user
  wrote (PostgreSQL's own behaviour). Resolving per statement would make `'-1h'` mean a different
  instant in every statement of a session, which is not a snapshot.
* `ts = physical_ms << TSO_LOGICAL_BITS` with the logical bits zero — the first timestamp of that
  millisecond, so a read "as of 14:00" sees everything committed strictly before 14:00.
* `SET TRANSACTION SNAPSHOT '<id>'`, with all five preconditions from §2 in PostgreSQL's own order.
* Three refusals, never clamps: **not in the future** (`22023`, naming the oracle's high-water
  mark), **not below the window** (`22023`, naming how far back the user *can* ask), **no writes**
  (`25006`).
* The window is retention (ADR 0021 Decision 2), so the DDL that sets it lands here rather than
  ahead of it: `ALTER TABLE t SET (retention = '7d' | 'forever' | DEFAULT)`, over the records that
  already exist in `catalog` and are already golden-tested.

### Unit 2 — checkpoints

* `SELECT pg_export_snapshot()` — PostgreSQL's own verb, and free: it writes nothing and returns a
  token carrying this transaction's `start_ts`.
* `SELECT esker_checkpoint('<name>')` — the named variant, one small record, no copy and no flush.
* `SELECT esker_drop_checkpoint('<name>')`, `SELECT * FROM esker_checkpoints()` — a name you cannot
  list is a name you cannot use.
* `SET TRANSACTION SNAPSHOT '<name>'` reads at a checkpoint; the id namespace holds both an opaque
  `esker-<16 hex>` token and a checkpoint name, and §2's `22023`/`42704` split tells a malformed
  one from an absent one.
* A checkpoint record is a **claim, not a guarantee** (ADR 0021): older than the window, it names
  data that is gone, and reading at it is unit 1's window refusal with the checkpoint's name in it.
* `CHECKPOINT <name>` and `AS OF SYSTEM TIME` become recognizer rows: PostgreSQL's own `42601`,
  plus a `HINT` naming the spelling that works.

### Unit 3 — `DIFF`

`SELECT * FROM esker_diff('<table>', '<from>', '<to>')`, four text columns —
`change`, `key`, `before`, `after`. Two read-only transactions at the two snapshots, one
`SeqScan` each over the same row range, and a merge that buffers one row per side: a key on the
right only is an `insert`, on the left only a `delete`, on both with different bytes an `update`,
and identical bytes are not a row. `O(rows)`, no set-operation machinery, and no `EXCEPT`.

**It is not a changelog, and the docs say so where a user reads them.** It compares two states: a
key written and written back is invisible to it, and five updates look like one. The changelog is
the Raft log and reading it is a different feature.

`EXCEPT` stays `0A000`: implementing general set operations to reach a two-table diff would be a
larger feature refused in a smaller disguise.

### Unit 4 — the tests, and where PostgreSQL parity ends

* `tests/slt/time_machine.slt` — the surface, end to end, over `MemoryBackend`.
* `tests/time_machine.rs` — the unit tests, including every refusal by SQLSTATE.
* `tests/corpus/pg19_time_machine.txt` — §2's capture, replayed. The half of this feature that
  **is** PostgreSQL (the GUC, `SET TRANSACTION SNAPSHOT`, `25006`, the `42601`s) is checked against
  the real server like everything else in this crate.
* The other half has no oracle: **PostgreSQL 19 has no time travel**, so `esker_checkpoint`,
  `esker_diff` and reading at a past snapshot are checked against *us* and documented as the point
  where parity ends. CockroachDB's `AS OF SYSTEM TIME` is cited as the semantic reference — the
  rounding rule, the read-only rule, the retention bound — and its *syntax* is deliberately not
  copied (§1).
* Every new divergence goes in `docs/plans/phase-6a.md` §10a's table, which is where this crate
  keeps the diff against PostgreSQL and where a reader will look for it.

## 5. What this phase will NOT do

* **`FLASHBACK`.** ADR 0021 Decision 3's fourth verb, and last for a reason: it is `O(rows changed)`
  writes in one transaction and needs the batching-with-a-durable-cursor that ADR 0020's backfill
  needs. It is not in this phase's brief and it is not in this phase.
* **`TxnClient::begin_at`, or anything else outside `crates/esker-sql/`.** Another lane's. §3 is
  the stub and the swap.
* **The collector's per-table safepoint arithmetic**, `esker-pd`'s pin for a named checkpoint, and
  the "smallest retention in the cluster" protocol field. All three are named in ADR 0021 as other
  crates' work; this phase writes the records they read and nothing else.
* **A cluster-default retention DDL.** The record and its accessors exist; the SQL for it does not,
  because a GUC that writes cluster-wide persistent state is a shape worth deciding once rather
  than in passing.
* **General set operations.** See unit 3.

## 6. Risks

* **A stub that reads the present.** The one failure that would be silent and wrong. `begin_at` on
  `StoreBackend` refuses by name rather than falling back, and a test asserts the refusal.
* **A snapshot that outlives its window.** Retention moves the floor forward under a session that
  set `esker.read_as_of` an hour ago. The check is therefore at *read* time as well as at `SET`
  time, and the error names the window rather than the timestamp.
* **`SET LOCAL` leaking past its block.** Scoped in the executor and undone by both `COMMIT` and
  `ROLLBACK`; a test drives each.
* **Two snapshots, two schemas.** `esker_diff` spans a `ALTER TABLE ADD COLUMN`, so each side is
  rendered with its own snapshot's catalog view. Falls out of giving each side its own transaction,
  and is asserted rather than assumed.

## 7. Test list

Written before the code, and each is one of the kinds `DESIGN.md` §11 requires of this layer.

1. `esker.read_as_of` accepted, `SHOW` echoes the text, `RESET` empties it — against the pg19 capture.
2. An interval and an absolute timestamp resolve to the same read when they name the same instant.
3. A read at a past snapshot sees the old row; the same query with no GUC sees the new one.
4. `INSERT`/`UPDATE`/`DELETE`/DDL under a past snapshot is `25006`, naming the command.
5. A timestamp above the oracle's high-water mark is `22023`, naming it.
6. A timestamp below the window is `22023`, naming the window.
7. `SET TRANSACTION SNAPSHOT`'s five preconditions, in PostgreSQL's precedence order.
8. `SET LOCAL` is undone by `COMMIT` and by `ROLLBACK`.
9. `ALTER TABLE ... SET (retention = ...)` writes the record; `DEFAULT` clears it; `DROP TABLE`
   clears it (already true, asserted here because this is the unit that makes it reachable).
10. `pg_export_snapshot()` round-trips through `SET TRANSACTION SNAPSHOT`.
11. A checkpoint outlives its session; a dropped one is `42704`; a listed one is in
    `esker_checkpoints()`.
12. `esker_diff` reports one insert, one update, one delete and nothing for an unchanged key.
13. `esker_diff` across a `ADD COLUMN` renders each side with its own schema.
14. `begin_at` on `StoreBackend` refuses by name.
15. The corpus: every new statement parses, and the three refused spellings come back `42601` with
    their `HINT` — never a bare syntax error, never `0A000` about valid PostgreSQL.

## 8. Progress

- [x] 1 — **the historical read, its bound and the retention DDL.** `SET esker.read_as_of`
  (`SET LOCAL`, `SHOW`, `RESET`, `= DEFAULT`), `SET TRANSACTION SNAPSHOT` with all five
  preconditions in PostgreSQL's measured precedence, the three refusals, and
  `ALTER TABLE ... SET (retention = ...)`. 21 new tests plus 11 corpus statements; `begin_at` on
  `StoreBackend` refuses by name, asserted against a real three-store cluster.
- [ ] 2 — checkpoints
- [ ] 3 — `DIFF`
- [ ] 4 — the tests, and where PostgreSQL parity ends

## 9. What changed from this plan

**A `CountingOracle` has no wall clock, so an instant means nothing against one.** The largest thing
the build found, and it is a property of the system rather than a defect. `esker_pd::tso` composes
`ts = physical_ms << 18 | logical` from a real clock, so `SET esker.read_as_of = '2026-08-30
14:00:00+00'` means what it says against PD. `CountingOracle` — which `src/bin/esker-sql.rs`'s
`connect()` still builds under a `TODO(phase-6a)`, and which every test cluster uses — is a pure
counter, so the physical half of every timestamp it hands out is zero: every version sits inside the
first millisecond of 1970 and no instant distinguishes two of them.

This is *why the snapshot token exists*, and it changed the test plan rather than the design. A
token carries a `start_ts` directly and needs no clock, so `SET TRANSACTION SNAPSHOT` works against
any oracle today; the GUC's instant form starts meaning something the moment this node takes its
timestamps from PD. `tests/real_backend.rs` uses the token form for exactly this reason and says so.

**The fake's clock was wrong in a way that would have made the tests prove nothing.**
`MemoryBackend`'s clock was a counter starting at zero, which is the same defect: every version in
the first millisecond of 1970. A test written against it would have passed on data no cluster
produces. It now starts at a plausible instant and advances the *logical* half per commit, with
`MemoryBackend::advance_ms` for a test that needs its versions in different milliseconds — which is
what a read *as of an instant* requires, because that timestamp has its logical bits zeroed by
design.

**ADR 0021's `42704` for a bad snapshot id was half the answer.** A second capture found two
conditions where the ADR named one: `22023 invalid snapshot identifier` for a string that cannot be
an identifier, `42704 snapshot "..." does not exist` for one that is well formed and absent. The
ADR is corrected.

**A failed `SET` must not leave its setting applied.** Found while wiring the stub: the first
version assigned `read_as_of` and *then* reopened the transaction, so a `SET` refused by the backend
left the session holding a snapshot the node had already refused — poisoning every later statement
after the user had been told the `SET` did not work. `Executor::move_to` now applies the setting
only when the move succeeds.

**Two test harnesses sent `BEGIN` through `execute`**, where it is `0A000 BEGIN is not supported`:
transaction control belongs to `pgwire::session` and reaches the executor as a call. Neither
harness had needed a block before. Both now route it the way the session does, which is what makes
`SET TRANSACTION SNAPSHOT`'s in-a-block preconditions testable at all.
