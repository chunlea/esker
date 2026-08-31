# 0021 — The time machine

Status: **design, with one piece built.** The catalog records for retention exist and are
golden-tested (`crates/esker-sql/src/catalog/`, §4 below) because the garbage collector consumes
them and a format cannot wait for the feature on top of it. Everything else here is design: no
historical reads, no `DIFF`, no `FLASHBACK`. The milestone is `docs/plans/phase-6a.md` §12. See
`docs/txn-spec.md` §7, `docs/DESIGN.md` §8, [ADR 0020](0020-online-schema-change.md).

## Context

The storage layer already **is** a time machine and nothing on top of it exposes that.

Every version of every key is filed under `txn_key(user_key, commit_ts)`, ordered newest-first
(`esker_keys::prefix`), and a read at `T` is defined as "the newest write with `commit_ts ≤ T`" —
which is exactly what `TxnKv` `Get` and `Scan` already do for whatever `start_ts` the transaction
was opened with. A timestamp's high 46 bits are milliseconds since the Unix epoch
(`ts = physical_ms << 18 | logical`, `esker_pd::tso`), so turning a wall-clock instant into a read
timestamp is a shift and not a lookup.

So the machinery for "read the database as it was" is finished and load-bearing already. What is
missing is a surface, a bound, and a way to *use* the past rather than only look at it. This ADR
designs those three and builds none of them, except the record format the third depends on.

The motivation is a database for **agents**. An agent's failure mode is not a slow query, it is an
action it cannot inspect or undo: it wrote something twenty minutes ago, it is now unsure what it
changed, and the only artifacts are logs it also wrote. A store that can answer "what did this look
like before" and "put it back, without erasing the fact that it happened" turns that from an
incident into a query.

## Decision 1: a historical read is a read timestamp, and nothing else

A transaction's `start_ts` is the whole of its snapshot (`crates/esker-client/src/txn.rs`), and
`TxnClient::begin()` takes it from the oracle. Reading the past is *only* a matter of choosing a
different one.

**The seam is one constructor.** `TxnClient::begin()` becomes the special case of
`TxnClient::begin_at(start_ts)`, which does everything `begin` does with the timestamp handed in
rather than allocated. Nothing else in that crate changes: locks, resolution, read-your-writes and
commit are indifferent to where the number came from. This is the "one small constructor" the
backend-wiring unit expected, and it is worth writing down that it is genuinely that small — the
client was built around `start_ts` as a value, not as a call.

Three rules make a historical read safe, and each is a refusal rather than a clamp:

* **Read-only.** A transaction at a past `start_ts` may not write. Committing at `commit_ts >
  start_ts` against a snapshot that old is a lost update with extra steps, and Percolator's
  conflict check would not catch it — the conflicting writer committed *after* the snapshot and
  before the write, which is the one window snapshot isolation does not close. `25006
  read_only_sql_transaction` is PostgreSQL's own code for it.
* **Not below the safepoint.** A read at a ts the collector has already passed cannot be answered
  correctly, and answering it approximately is worse than refusing (`docs/txn-spec.md` §7 already
  says a read below the safepoint is not guaranteed). The answer is an error naming the window —
  the user asked for a time and the useful reply says how far back they *can* ask.
* **Not in the future.** A ts above the oracle's high-water mark names an instant that has not
  happened; a read there would see a prefix of it and call it complete.

### The syntax — measured, not chosen from memory

The first draft of this ADR proposed CockroachDB's `AS OF SYSTEM TIME` and deferred the question of
whether `sqlparser` reads it. Measuring took two minutes and changed the answer completely. Against
0.62.0's PostgreSQL dialect:

| Spelling | Whose | `sqlparser` 0.62.0 |
|---|---|---|
| `SELECT ... FROM t AS OF SYSTEM TIME '...'` | CockroachDB | **no** — `Expected: end of statement, found: SYSTEM` |
| `BEGIN AS OF SYSTEM TIME '-1h'` | CockroachDB | **no** |
| `SELECT ... FROM t FOR SYSTEM_TIME AS OF '...'` | SQL:2011, SQL Server | **no** — `Expected: one of UPDATE or SHARE` |
| `SET TRANSACTION SNAPSHOT '<id>'` | **PostgreSQL's own** | **yes** |
| `SET esker.read_as_of = '...'` | PostgreSQL's own custom-GUC namespace | **yes** |

Every invented spelling would have to be added to the parser, inside the boundary ADR 0014 draws,
and would make this node accept syntax the oracle rejects. The two that parse are PostgreSQL's, and
one of them already *means this*.

**Decision: `SET TRANSACTION SNAPSHOT` is the surface, and a namespaced GUC is the shorthand.**

`SET TRANSACTION SNAPSHOT '<id>'` is how PostgreSQL has always said "this transaction reads what
that snapshot saw" — the second half of `pg_export_snapshot()`. Its preconditions were captured
from the server rather than recalled, and every one of them is a rule we want anyway:

* outside a transaction block: `WARNING 25P01` and then `ERROR 0A000 a snapshot-importing
  transaction must have isolation level SERIALIZABLE or REPEATABLE READ`;
* after any query in the block: `ERROR 25001 SET TRANSACTION SNAPSHOT must be called before any
  query` — which is exactly right, because a `start_ts` cannot change under a transaction that has
  already read at it;
* under `READ COMMITTED`: the same `0A000`. Percolator gives snapshot isolation, which is
  PostgreSQL's `REPEATABLE READ`, so the precondition is one this node satisfies by construction;
* an id that is not there: `ERROR 42704 snapshot "..." does not exist`.

So the *whole* error surface of the feature is PostgreSQL's, already, including the one about
calling it too late — which is a rule an invented syntax would have had to discover the hard way.

The snapshot **id** is ours to define: an opaque token carrying a `start_ts`, produced by an export
function. One divergence, stated rather than discovered: PostgreSQL's exported snapshot is valid
only while the exporting transaction is open, and ours stays valid until retention passes it. That
is a superset — every PostgreSQL program keeps working — and it is what makes a checkpoint a
checkpoint rather than a handle.

`SET esker.read_as_of = '2026-08-30 14:00:00+00'` is the shorthand for the common case, where the
user has a wall-clock instant rather than a token. It is a **namespaced custom GUC**, which is
PostgreSQL's own extension mechanism: a real server accepts it, stores it and hands it back to
`SHOW` — measured — and refuses an *un*-namespaced `SET nonamespace_thing` with `42704`. So the
statement is valid PostgreSQL that a real server simply does not act on, which is the least
divergent shape a feature PostgreSQL does not have can possibly take.

Neither spelling needs one line inside `src/parse/`.

### Wall clock in, timestamp out

`ts = physical_ms << TSO_LOGICAL_BITS`, with the logical bits zero, is the first timestamp of that
millisecond — so a read at it sees every transaction that committed strictly before that
millisecond and none that committed within it. That is the correct rounding for "as of 14:00": a
transaction committing *at* 14:00:00.000 is not yet visible at 14:00:00.000, exactly as it is not
visible at the instant before it commits.

A snapshot id carries its `start_ts` directly, so that path does no arithmetic at all.

## Decision 2: the travel window is the GC safepoint distance, and it is per table

There is no separate retention mechanism to build. **How far back you can read is how far back the
collector has not yet swept**, and those must be the same number or the feature is a promise the
storage layer does not keep.

The knob is therefore retention, and one number for a cluster is the wrong shape for the workload
this is for. An agent's scratch table is rewritten constantly and nobody will ever read it as of an
hour ago; a ledger table is written once and read as of anything. Giving both the same window makes
one of them pay for the other.

So: **a cluster default, overridable per table.** Both live in the `'m'` space (§4). The cost is
stated plainly rather than buried:

> Retention is version-chain depth. A key rewritten `n` times inside the window carries `n`
> versions, and every read of that key walks past them to reach the newest — MVCC keys sort
> newest-first, so a point read stops at the first, but a **scan** touches every version of every
> key in its range, and a compaction rewrites them all. Doubling retention on a hot-rewritten table
> roughly doubles the bytes its scans read.
>
> The metric to watch is **versions per live key** on the tables with the longest retention, not
> disk usage — disk is cheap and answers the wrong question. A scan whose row count is flat while
> its bytes-read climbs is this cost arriving.

### What the collector has to do with the two records

The collector is `esker-store`'s (phase 5, deliverable 2) and lives below `esker-sql`, which it
does not link. Everything it needs is in §4's records plus one arithmetic rule.

PD publishes one safepoint (`TxnKvReq::GcSafepoint`), and it is a **floor**: no store may collect
above it, because PD is the only thing that knows the oldest active read. A per-table override
therefore composes as

```text
effective_safepoint(table) = min(published_safepoint,
                                 published_safepoint + ((default_ms - table_ms) << 18))
```

which is to say: **an override may make a table keep more, never less.** A table with a *longer*
retention than the default has its safepoint pushed back by the difference and works entirely
store-side, with no protocol change. A table with a *shorter* one gets no benefit yet — the `min`
discards it — and that is the honest limitation of doing this without touching the wire.

Making a shorter retention actually collect sooner needs PD to publish its floor computed from the
*smallest* retention in the cluster rather than the default, which means PD learning one number
from the SQL layer. That is a small protocol addition and it is the right one; it is named here so
that the store lane can build the store half now and the two can meet later.

Two details the collector must not get wrong, both of which are why this is a record and not a
constant: `RETENTION_FOREVER` (`u64::MAX`) is a **sentinel**, not a duration — subtracting it
underflows, and the rule is "collect nothing for this table"; and a table with no override is not
"retention zero", it is the cluster default, which is the difference between an absent key and a
key holding zero.

The one piece it needs that does not exist: a way to get `(tenant, table_id)` back out of a row or
index key. The layout is `'t' ++ tenant ++ table_id ++ ...` with memcomparable ids
(`esker_keys::prefix`), so it is a decode and not a parse, and it belongs in `esker-keys` beside
the encoders rather than being written twice.

## Decision 3: four verbs, in the order they are worth building

Each is named with what it costs, because the cheap ones are cheap for a structural reason and the
expensive one is worth its price for a different one.

**A checkpoint — free, and it is `pg_export_snapshot()`.** Take a timestamp and give it a name. It
writes one small record and *nothing else*: no snapshot, no copy, no flush. A checkpoint is a
number, and the data it refers to is kept by retention whether anybody named it or not. The catch
is exactly that — a checkpoint older than the window names data that is gone, so the record is a
claim to check rather than a guarantee, and a checkpoint may optionally **pin** its timestamp by
holding the safepoint back, which is the same mechanism PD already uses for an active read and the
same cost.

The verb must not be spelled `CHECKPOINT`: PostgreSQL owns that keyword for forcing a WAL
checkpoint, this node already answers `0A000` for it (§9, G25), and taking it would be the
compatibility layer inventing semantics for a word that has some. The right spelling is the one
PostgreSQL already pairs with `SET TRANSACTION SNAPSHOT` — `pg_export_snapshot()` — with a named
variant for a checkpoint meant to outlive the session.

**Reading at a checkpoint — free.** It is Decision 1 with the timestamp looked up instead of
computed.

**`DIFF` between two timestamps — two scans and a merge.** Open two read-only transactions at
`t1` and `t2`, scan the same key range in both, and walk the two sorted streams together: a key in
the second only is an insert, in the first only a delete, in both with different values an update,
in both with the same value nothing. Both scans are ordinary `TxnKv` scans over one table's row
range and the merge is `O(rows)` with no buffering beyond one row per side, because the key space
is ordered and both sides come back in that order.

What it is *not* is a changelog: it compares two states and cannot see a key that was written and
then written back, nor tell one update from five. Saying so is important, because "diff" invites
the other reading. A real changelog is the Raft log, and reading it is a different feature.

**`FLASHBACK TABLE t TO <ts>` — a write, and deliberately not a rewrite of history.** Implemented as
**compensating writes**: read the table as of `ts`, read it as of now, and write the difference as
an ordinary transaction at a fresh `commit_ts`. Every version that existed before the flashback is
still there, still readable `AS OF` an instant before it — so an undo is itself undoable, and the
audit trail survives the correction. This is the whole argument for the design: a flashback that
*mutated* history would be the one operation in the system that destroys evidence, and it would do
it precisely when somebody is trying to work out what happened.

The costs are real and are the reason it is last: it is `O(rows changed)` writes in one transaction,
so a large flashback needs the same batching-with-a-durable-cursor the online-DDL backfill needs
(ADR 0020), and every index has to be maintained by the same code path an `UPDATE` uses rather than
by a shortcut. `FLASHBACK` restoring a *dropped* table is a different and harder problem — the
catalog record is gone too, and the rows are below a safepoint nothing was holding back — and is
out of scope here.

## Decision 4: the records, which exist now

Built, because the collector consumes them and a format cannot wait for its feature.

```text
'm' ++ "sql" ++ 'd'                          the cluster default retention
'm' ++ "sql" ++ 'r' ++ tenant:u64 ++ id:u64  one table's override
```

Value, both: `version:u8 = 2 ++ retention_ms:u64` little-endian, nine bytes, behind the catalog
format version like every other record — an unknown version is a typed error, never a guess
(`CLAUDE.md` invariant 2). Golden-tested, including that a trailing byte is refused and that every
truncation is.

Three choices in that layout, each for a reason:

* **Its own kind byte, not a field of the table record.** The collector must read a retention
  without decoding a table definition, whose format is `esker-sql`'s business and not the store's.
  A prefix scan of `'m' ++ "sql" ++ 'r'` returns every override and nothing else, which is how the
  collector loads the whole map once per pass rather than asking per key.
* **The default under `'d'` and not under `'r'`.** Under `'r'` it would fall inside that scan and
  would have to be told apart from an override by its length. The test asserts it does not.
* **Milliseconds.** The unit of a timestamp's physical half, so retention-to-timestamp is a shift.

Setting a retention **does not bump the catalog version**, which is a deliberate asymmetry with
every other catalog write. Retention changes nothing about how a row is written or read; bumping
would make every node in the cluster discard its table cache to learn a number none of them uses.
The collector picks it up on its next pass, which is the only place it means anything.

An override is deleted with its table, so the collector's scan does not accumulate names of tables
that are gone.

There is no SQL surface yet — no `ALTER TABLE ... SET (retention = ...)`. That is a small piece of
the unit that builds the rest, and it lands with `AS OF` rather than ahead of it.

## What the rest of the system has to grow

* **`esker-client`** — `TxnClient::begin_at(start_ts)`, and a read-only transaction that refuses to
  commit writes. That is all.
* **`esker-pd`** — the safepoint arithmetic above, a pin for a named checkpoint (the same mechanism
  as an active read), and the "smallest retention in the cluster" input that makes a shorter
  override real.
* **`esker-store`** — the collector's per-table safepoint, from §4's records.
* **`esker-keys`** — `(tenant, table_id)` back out of a table key.
* **`esker-sql`** — the surface: `SET TRANSACTION SNAPSHOT` and the `esker.read_as_of` GUC (neither
  of which needs a parser change), the retention DDL, the checkpoint record and its export function,
  `DIFF` as a two-cursor merge over the existing scan, and `FLASHBACK` as a batched compensating
  transaction.
* **`esker-proto`** — one field on the GC safepoint message, and nothing else.

## The one thing this shares with ADR 0020

The retention window and the maximum stale-schema window are the **same number**: a read-only
transaction's lifetime is bounded by the safepoint, and ADR 0020's step interval has to outlast the
longest transaction. Raising retention to give agents a deeper time machine therefore slows every
online schema change by the same amount. Neither number should be tuned without the other in view,
which is the reason this sentence exists.
