# ADR 0106 — What a statement reads below the SQL

Status: **proposed** (2026-09-10) · Debt `debts-v1.1.md` #49 is this row · Numbered 0106 by the
coordinator.

**No decision is made here.** This records what a statement actually reads, what each of three
shapes would save, and what each costs — so the milestone is the user's to set. Nothing is built.

## Context — the number that sizes the problem is not the one the row was opened on

Run 114's desktop projection
(`esker-rails-harness/results/run-114-transactions-projection.md`, r1's) puts `transactions_test`
at **519 ms a statement** on the real topology, and inside it:

```text
pk_and_sequence_for   447 statements x 1.05 s = 477 s = 26.5% of the pass
everything else       907 statements          = the other 73.5%, unpriced
```

r1 measured one `pk_and_sequence_for` at **35 round trips** — 23 point reads and 12 range scans —
and taking that to 3 buys back about **24%**. So the row as opened is about a quarter of the cost,
and **three quarters is in the 907 ordinary statements nobody had priced.** r1's run 117 prices
them per statement; this ADR prices what they *read*, which is the half that says what to build.

## What a statement reads, measured

`crates/esker-sql/tests/statement_reads.rs`, 2026-09-10, with `ESKER_STMT_STATS=1
ESKER_STMT_STATS_TRACE=1`; the full trace is in [`docs/bench/statement-reads.md`](../bench/statement-reads.md).
Counted at the store boundary, where `Txn::get` and `Txn::scan` are the only two doors.

**Topology-independent by construction, and that is the point of taking it here.** Which keys a
statement reads is a property of the catalog code; only the *round-trip* count depends on the
client and its buffer. So these numbers are not r1's 35 and are not meant to be — they are its
composition.

| statement | reads | point | range |
|---|---|---|---|
| `pk_and_sequence_for` | **60** | 37 | 23 |
| `SELECT a FROM pk0 WHERE id = 1` | **9** | 9 | 0 |
| `INSERT INTO pk0 … RETURNING id` | **8** | 8 | 0 |
| `BEGIN` / `SAVEPOINT` / `RELEASE` / `COMMIT` | **0** | 0 | 0 |

### `pk_and_sequence_for`, all sixty

```text
point reads (37)                         range scans (23)
 16 x schema(t1,"esker")                   5 x name(t1)*
  5 x table(t1,#16384)                     5 x type(t1)*
  5 x name(t1,"pg_class" | "pg_attribute"  5 x view(t1)*
        | "pg_depend" | "pg_constraint"    5 x sequence(t1,#16384)*
        | "pg_namespace")                  3 x schema(t1)*
  5 x view(t1, the same five)
  2 x version(t1)
  2 x version(t<cluster>)
  2 x layout
```

Three shapes in that, and each points at a different option:

**1. One key, sixteen times.** `schema(t1,"esker")` is read sixteen times inside one statement, at
one snapshot, and the answer cannot change between the first and the sixteenth. **Fifteen of the
sixty are one memo away.**

**2. One hydration bundle, five times.** The sequence

```text
scan name(t1)*   get table(t1,#16384)   scan sequence(t1,#16384)*   scan type(t1)*   scan view(t1)*
```

appears **five times** — positions 8–12, 39–43, 45–49, 50–54, 55–59 of the trace — once per
relation in the query's `FROM` list. Four of the five are exact repeats. Their callers are
`pg_relations::Relations::read` (the `name` scan and the per-table point read),
`catalog::table_sequences` and `catalog::column_user_types` → `catalog::user_types` (inside
`catalog::hydrate`), `catalog::views`, and `catalog::schemas`.

**3. Three of those scans are tenant-wide, and they are the ones that grow.** `name(t1)*`,
`type(t1)*` and `view(t1)*` scan a whole prefix of the tenant's catalog, so each costs O(relations).
Fifteen of the twenty-three range scans are of that kind, and **twelve of those fifteen are exact
repeats within one statement.** `tests/pk_and_sequence_cost.rs` asserts this statement costs what it
*selects* and not the catalog crossed; it does not assert that it reads the catalog **once**, and
the two are different promises.

### The ordinary statements — the other three quarters

```text
SELECT a FROM pk0 WHERE id = 1        INSERT INTO pk0 … RETURNING id
  4 x schema(t1,"esker")                3 x schema(t1,"esker")
  1 x view(t1,"pk0")                    1 x view(t1,"pk0")
  1 x version(t1)                       1 x version(t1)
  1 x version(t<cluster>)               1 x version(t<cluster>)
  1 x layout                            1 x layout
  1 x row(t1,#16384)   <- the data      1 x row(t1,#16384)
```

**Eight of a point read's nine reads are catalog and one is the row.** That is the sentence this
ADR exists to put in front of a decision: the 907 unpriced statements are not cheap statements, they
are statements that pay eight catalog reads to do one row read.

### Which of them a version counter can cache, and which fall in one region

Two questions per read, because the two options ask different ones. **Cacheable** means: is the
answer a function of the catalog version, so that an unchanged version means an unchanged answer?
**Same region** means: does the key share the contiguous `'m' ++ "sql"` prefix, so that one scan
could fetch it?

| read | what it is | cacheable by version | same region as the rest |
|---|---|---|---|
| `version(t1)` | the tenant's catalog version | **no — it *is* the validator** | yes |
| `version(t<cluster>)` | databases and roles | no, same reason | yes |
| `layout` | the key layout, one per store | **constant**: read once per process | yes |
| `schema(t1,"esker")` | one schema record | **yes** | yes |
| `scan schema(t1)*` | every schema | **yes** | yes |
| `name(t1,"pg_class")` | a relation's name record | **yes** | yes |
| `scan name(t1)*` | every relation name | **yes** | yes |
| `table(t1,#16384)` | a table record | **yes** | yes |
| `scan sequence(t1,#16384)*` | one table's sequences | **yes** for the *definition*; the sequence's **value** is a different key (`sequence-value`) and is not | yes |
| `scan type(t1)*` | every user type | **yes** | yes |
| `view(t1,"pk0")`, `scan view(t1)*` | view definitions | **yes** | yes |
| `row(t1,#16384)` | the data | **no**, and it is the one read that is the point | **no** — a row key is `'t' ++ tenant ++ table ++ 'r'`, a different prefix and a different region |

**Every catalog read in the census is cacheable except the two version keys and the layout**, and
the layout is a constant. That is the whole of option B's case: the validator is one point read and
everything it validates is the other 58.

**And every catalog read is in one prefix.** `'m' ++ "sql" ++ kind ++ tenant` puts them in one
contiguous stretch of the key space, which is option A's case — with the caveat that "one prefix"
is not "one region": the placement driver may split anywhere, and nothing in the SQL layer can hold
it to a promise it never made.

### Transaction control reads nothing

`BEGIN`, `SAVEPOINT`, `RELEASE` and `COMMIT` make **zero** KV reads. They do not reach the
statement guard at all, which is why the census clears its trace before each statement — without
that they reported the previous statement's list, and four rows of this table were the insert's
eight until it did.

**A zero here is not "free".** `COMMIT`'s prewrite and commit are round trips this instrument does
not count, and a transaction's first statement fetches a timestamp from the TSO. What the zero says
is only that **transaction control has nothing to save below this door** — so an option aimed at
reads cannot move it, and if run 117 finds these statements expensive the cause is somewhere this
ADR is not looking.

## The three shapes

### A. Batch one statement's catalog reads into one same-region scan

Every catalog key is `'m' ++ "sql" ++ kind ++ tenant ++ …` — one contiguous prefix, and on a
cluster that has not split inside it, one region. So a statement could take **one** scan of that
prefix instead of sixty reads.

* **Round trips saved** — `pk_and_sequence_for` 60 → **1**; a point select 9 → **2** (one catalog
  scan, one row); an insert 8 → **2**. Best case of the three by count.
* **Correctness premises** — the scan must be at the statement's own `reading_ts`, which it already
  would be; the read set for serializable validation becomes the whole prefix rather than the keys
  touched, which **widens every catalog reader's conflict footprint** — a DDL anywhere in the
  tenant would then conflict with every concurrent statement (ADR 0062's phantom rule is what makes
  a range a read-set entry).
* **Change surface** — `esker-sql/src/catalog/mod.rs` (`hydrate`, `user_types`, `views`,
  `schemas`), `catalog/pg_relations.rs` (`Relations::read`), and a new prefetch seam in
  `exec::query` where a statement's relations are known.
* **Risks** — the scan is **O(catalog)** whatever the statement needs, which is the cost
  `backend::mod`'s `a_scan_costs_its_range_and_not_the_store` was written to keep out: with 866 relations a
  `SELECT … WHERE id = $1` would walk the whole catalog to read one row. And a region **split
  inside the meta prefix** silently turns one scan into several, so the "one region" premise is not
  one the node can hold. A narrower form — batch only the keys a statement's relations need, from a
  two-phase plan — keeps the count win without the O(catalog) scan and is a bigger change.

### B. A node-side catalog cache, validated by the version counter

Keep the decoded catalog in memory on the node and validate it with **one** point read of
`version(t1)` per statement: unchanged means the cache stands, changed means reload.

**And most of that is already built, which is the correction this ADR's first draft needed.**
`catalog::Catalog` holds a `Cache` of `names` and `tables` keyed on the summed version, `View`
answers `relation()` and `table_by_id()` from it, `usable_at` drops it when the version moves, and
a transaction that has written the catalog gets `catalog: None` so it reads its own DDL. The
mechanism is there, the invalidation is there, and the version read is the one ADR 0102 timed.

**What the census shows is who goes around it**, and that is a different and much smaller change
than building a cache:

* **`catalog::schema_exists(txn, …)`** is a free function on `&dyn Txn` with no cache behind it —
  the sixteen identical `schema(t1,"esker")` reads;
* **`pg_relations::Relations::read(txn, tenant)`** loads the tenant's whole catalog itself, scan
  and per-table `hydrate` included, without ever touching `View` — **27 call sites**, and the five
  catalog views in `pk_and_sequence_for`'s `FROM` list are five of them. That is the bundle that
  repeats five times;
* **`catalog::views`, `user_types`, `schemas`, `table_sequences`** are free functions on
  `&dyn Txn` too, called from inside `hydrate` and from `pg_catalog`'s row builders — the
  tenant-wide scans.

So option B is *"give the cache the readers it does not have"*, not *"add a cache"*. Same numbers,
a fraction of the risk, and it is the shape this project keeps meeting: one mechanism, and a
second reader that never learned about it.

* **Round trips saved** — `pk_and_sequence_for` 60 → **2** (`version(t1)` and the cluster's);
  a point select 9 → **3** (two versions and the row), and **2** if the cluster version is
  validated once per session rather than per statement; an insert 8 → **2**. `layout` is a
  constant and is read once per process.
* **Correctness premises** — (1) **every catalog write bumps the version**, which is already the
  rule and already has a test; (2) a transaction that writes the catalog must read **its own**
  writes, so a DDL transaction bypasses the cache after its first write; (3) cross-node
  invalidation is the schema lease ([ADR 0028](0028-the-schema-lease.md)) and is already there —
  the cache's lifetime is the lease's; (4) the cluster-tenant version (`version(t<cluster>)`)
  covers databases and roles and needs the same treatment or it becomes the new floor.
* **Change surface** — `esker-sql/src/catalog/mod.rs` (`Cache` gains schemas, views, types and
  sequences; `schema_exists` and the four free functions take a `&View`), `catalog/pg_relations.rs`
  (`Relations::read` takes a `&View`), and its **27 call sites**. No new mechanism and no change to
  the lease. `docs/plans/debt-49-catalog-cache.md` is the file list and the API sketch.
* **Risks** — a stale cache is a **wrong answer**, not a slow one, which is the class this project
  refuses everywhere else; the failure mode is invisible until the *next* statement, which is
  exactly what `a-catalog-write-must-bump-the-version` records. Mitigated by the fact that the
  version read is per statement, so the window is one statement wide and the same width the node
  already trusts.

### C. Client-side read pipelining

Issue the reads of one statement that do not depend on each other concurrently, instead of one
after another.

* **Round trips saved — none.** This changes **latency**, not count: five independent hydration
  bundles that today are sequential could go out together, so the catalog part of a statement's
  wall time falls by roughly the depth of its longest chain rather than its total. Against a 519 ms
  statement whose reads are serial that is the largest single lever *without changing what is
  read*.
* **Correctness premises** — every read of a statement is at one `reading_ts`, so they are
  order-independent by construction; a read must not be reordered ahead of a **write** in the same
  statement, which the buffer already serialises; and the client's retry rule has to stay
  per-request rather than per-batch, or one region's leader change fails a whole bundle.
* **Change surface** — `esker-client/src/router.rs` and `txn.rs` (a batched/pipelined read), and
  `esker-sql/src/backend/store.rs` at the `Txn::get`/`Txn::scan` boundary, which today is
  synchronous and one call at a time. **`Backend`'s trait is synchronous**, so this is the option
  that changes a contract rather than an implementation.
* **Risks** — concurrency in the read path with none of the counts moving means a regression here
  is a *correctness* bug bought for a latency win; and it composes badly with A and B, both of
  which remove the reads it parallelises.

## What the numbers say, and what they do not

**B is the one that reaches the three quarters.** A is better by count on
`pk_and_sequence_for` — the statement the row was opened on — and B is what turns an ordinary
statement's eight catalog reads into one. Since the ordinary statements are 907 of 1,354 and about
three quarters of the time, an option that only helps the 447 buys about a quarter of the cost, which
is the arithmetic that opened this ADR.

**A and B are not a ladder** — B removes the reads A batches, so choosing A and later wanting B
means doing B anyway. C is orthogonal to both and is worth least once either lands.

**Recommendation: B, and it waits on the user.** It is the one whose saving scales with the number
of statements rather than with the size of one, it aligns with [ADR 0102](0102-the-catalogs-read-path.md)'s
finding (the catalog *read path* is 0.4% of a statement's time, so the cost is in how many reads
there are and not in what each costs), and its correctness premises are three rules the node already
has rather than three it would need. What it asks the user for is the milestone: a cache whose
staleness is a wrong answer is a decision, not a refactor.

## What run 117 has to answer, written before it lands

r1's run 117 prices `transactions_test` per statement on the real topology. **The criteria are
fixed here, before the numbers, for the reason [ADR 0102](0102-the-catalogs-read-path.md) fixed
its own in advance: so that reading them cannot choose the answer.** Three questions, and each has
an outcome that would move this ADR rather than confirm it.

1. **What does an ordinary statement cost, and how much of it is below the SQL?** The census says
   such a statement makes 9 KV reads, 8 of them catalog. If the 907 ordinary statements come out
   near the pass mean (519 ms) **and** their round trips are near their read count, option B is
   sized right. **If their round trips are far below their read count**, the client is already
   answering most of them from its buffer and B buys less than this ADR claims.
2. **What is a `BEGIN` / `SAVEPOINT` / `RELEASE` / `COMMIT` worth?** They read nothing, so if they
   are *cheap*, the reads really are where the time is. **If they are expensive**, the cost is the
   TSO, the prewrite or the wire — none of which any of the three options touches — and the row
   moves to a different ADR.
3. **Does `pk_and_sequence_for` still hold 26.5%?** Run 114's is a desktop projection. **If the
   real per-statement prices redistribute it**, the arithmetic that ranks B above A changes with
   it, and the ranking is what this ADR is for.

**A fourth thing to look for that nobody asked about**: whether the per-statement times are
*bimodal*. A mean of 519 ms over 1,354 statements is consistent with every statement costing
519 ms and with a hundred costing five seconds — and only the second is a hunt with a target. The
tap prints per statement, so this is free to check and is the first thing to check.

**What run 117 said** — to be filled from the report; empty on purpose until it exists, because an
ADR with a section titled after a measurement it does not have is how a projection becomes a fact.

**Not decided here, and stated so a reader does not infer it:** whether any of this is in v1.2 at
all, and whether run 117's per-statement prices change the ranking. If the 907 statements turn out
to be dominated by something this census cannot see — the TSO, the commit, the wire — then none of
the three options is the answer and the row moves.
