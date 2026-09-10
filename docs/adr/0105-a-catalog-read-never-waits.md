# 0105 — A catalog read never waits

*Status: proposed. Debt #50. Written 2026-09-10, after ADR 0104 §4's diagnostic made the failure
name itself.*

## Context — one key, every statement, and a lock nobody can see

Every statement that resolves a relation reads the catalog's **version counter** — one key per
tenant, `'m' ++ "sql" ++ 'v' ++ tenant` (`catalog/record.rs`) — plus the cluster's, and a layout
stamp. `Catalog::view_at` is where that happens and ADR 0102 measured it: **two views per ordinary
statement**, on the path of everything.

Every DDL statement *writes* that counter (`bump_version`). A write is a Percolator lock, and the
SQL layer takes **no node-local lock for a catalog key** — `crate::backend::Reach` appears nowhere
under `catalog/`. So a reader that meets a DDL's lock there does what a reader does anywhere: it
spends the client's resolution budget and then fails.

    a reader waited 4.28 s behind a DDL's commit and was refused: could not serialize access
    due to concurrent update: a lock from the transaction at 1012 could not be cleared for a read

That is `crates/esker-sql/tests/catalog_contention.rs`, against three real stores, deterministic.
The `for a read` is ADR 0104 §4's diagnostic naming the producer — the first thing this
project has been able to say about that message without guessing.

**PostgreSQL does neither half of that.** Under READ COMMITTED a plain `SELECT` never raises
`40001`, and an uncommitted DDL is not visible to other sessions at all. A catalog lookup there uses
an MVCC snapshot and does not wait for anybody's uncommitted work.

### What the debt assumed, and what is actually true

#50 was written as *"session A holds `BEGIN; DROP …; CREATE …` open and session B's ordinary
`SELECT` waits behind the counter"*. It does not, and the difference matters for every option below:
a catalog write is `txn.put`, which **buffers**, so an open DDL transaction has left nothing in the
store. `an_uncommitted_ddl_does_not_block_another_sessions_select` pins that.

**The window is the DDL's commit** — from its prewrite to its commit record — and that is a window
every DDL opens, on a key every statement of every session reads. Rails' `setup` drops and creates
two tables per test, so the suite opens it thousands of times; run 114's
`a lock … could not be cleared` is the shape it produces.

## Options

### (a) A catalog read reads past the lock

A reader that meets a lock on a catalog key does not resolve it and does not wait. It re-reads at
`lock.start_ts - 1`: the newest state strictly before the transaction that holds it. Bounded by
construction — each retry lowers the timestamp — and **no wire change**, because `TxnKv::Get`
already takes the timestamp to read at.

The statement then sees the catalog as it was before the in-flight DDL, which is exactly what
PostgreSQL shows it: an uncommitted DDL is not there.

*Cost*: one extra round trip in the contended case, none otherwise. *Risk*: it is a deliberate read
of a slightly older catalog — see the consequence below.

### (b) The counter's bump is its own short transaction

The DDL commits its catalog rows, then bumps the counter in a separate one-key transaction. The
lock's lifetime shrinks from "the DDL's whole commit" to "one key's 2PC".

*Cost*: the two are no longer atomic. A crash between them leaves catalog rows a version counter
does not announce, which is the invariant ADR 0028's lease and `Catalog::view_at` both rest on. It
makes the window smaller and never closes it; a reader unlucky enough to land inside still waits its
whole budget and still gets `40001`. **A mitigation, not a fix.**

### (c) A node-local watcher cache for the counter

The node subscribes once and keeps the current version in memory, so ordinary statements never read
the key at all. `pgwire/server.rs` already has a watcher to hang it on.

*Cost*: a cache with an invalidation rule, on the path of every statement, and a new failure mode —
a node whose watcher is behind serves a stale catalog to *every* session rather than one. The first
reader after an invalidation still reads the key and can still meet the lock, so this does not close
the window either; it makes it rarer. ADR 0102 already looked at caching this read and closed it as
"answered" at 0.4%, so the cost is being paid twice for a smaller problem.

## Decision — (a)

**A catalog read never waits.** It is the only one of the three that makes the failure impossible
rather than unlikely, it is the behaviour PostgreSQL has, it needs no wire change and no format
change, and it is confined to the catalog's read path.

The rule is stated on the read, not on the key: `Txn::get_without_waiting` answers *the newest
committed value at or below the oldest lock in the way*, and `Catalog::view_at` is its only caller.
A **required** trait method, not a defaulted one — a defaulted `Txn` method is a silent opt-out for
every wrapper, and `savepoint::Recording` has now swallowed three of them
(`lock`, `validate_reads`, `changed_since_statement`).

### The consequence to state plainly: a DDL that commits under a running statement

A statement resolves its relations once, from the view it took at its start. If a DDL commits after
that, the statement carries on with the catalog it read. That is a divergence worth naming rather
than discovering:

* **PostgreSQL** takes `AccessExclusiveLock` on the objects a DDL names, which blocks *other
  sessions' statements on those objects* until the DDL commits — and blocks nothing else. A session
  reading an unrelated table is never delayed, and a session reading the altered table waits and
  then sees the new definition.
* **This node** blocks nobody and shows every statement the catalog as of its own start. A statement
  already running against the altered table finishes against the old definition.

So the two agree for the unrelated reader — which is the case #50 is about and the case Rails hits
thousands of times — and diverge for a reader of the table being altered: PostgreSQL serialises it,
this node lets it finish on the older view. That is the same class of divergence ADR 0020 (online
schema change) already carries, and it is the one this ADR accepts rather than closes: closing it
needs a per-object lock, which is a cluster-wide mechanism and its own decision.

### What it cost, both directions

    before   a reader waited 4.28 s behind a DDL's commit and was refused
             `40001 … a lock from the transaction at 1012 could not be cleared for a read`
    after    0.088 s, and the row

The duration is asserted as well as the answer. A fix that only shortened the wait would still be a
reader waiting behind a DDL, and an assertion on the answer alone would not have noticed —
`MAX_LOCK_RESOLUTIONS` is a knob and turning it down would have made this test green while leaving
the defect exactly where it was.

## Consequences

* A reader can no longer be refused `40001` by the catalog. The `for a read` producer of
  `LockNotCleared` should go to zero for catalog keys; if ADR 0104 §4's measurement still finds it,
  what it found is a *row* lock and a different problem.
* A statement may act on a catalog older than an in-flight DDL, deliberately. Its own writes are
  still validated at commit, so a DDL that commits first does not silently lose a race — the writer
  meets it there, where a conflict is what `40001` is for.
* Nothing changes on the write path, on the wire, or on disk.
* (b) and (c) stay available and are not needed: (a) removes the wait that made them attractive.

## References

* [ADR 0102](0102-the-catalogs-read-path.md) — the measurement that put two catalog views on every
  statement, and the caching question it closed.
* [ADR 0104](0104-where-a-conflict-becomes-40001-and-where-40p01.md) §4 — the diagnostic that named
  this producer, and the refutation that asked for the measurement this ADR answers.
* `catalog/record.rs` — the counter's key, and the comment that first described this contention.
* `crates/esker-sql/tests/catalog_contention.rs` — both facts: the open DDL that blocks nobody, and
  the commit that blocks everybody.
