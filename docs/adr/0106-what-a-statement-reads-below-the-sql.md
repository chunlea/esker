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
and ~~three quarters is in the 907 ordinary statements nobody had priced~~. r1's run 117 prices
them per statement; this ADR prices what they *read*, which is the half that says what to build.

> **Struck 2026-09-10 by run 117, and the section at the end is where it is answered.** Priced
> statement by statement, catalog introspection is **72.4%** of the file and `pk_and_sequence_for`
> alone is **71.8%**; the ordinary statements are 8.2 ms at the median. **The three quarters this
> ADR was built around does not exist.** Everything between here and *"What run 117 said"* is the
> record as it was written before that, kept because what it measured is still true and what it
> concluded from it is not.

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

**Eight of a point read's nine reads are catalog and one is the row.** ~~That is the sentence this
ADR exists to put in front of a decision: the 907 unpriced statements are not cheap statements, they
are statements that pay eight catalog reads to do one row read.~~

**The first half stayed true and the second was refuted.** Run 117 priced those statements at
8.2 ms (median), 4.5 ms and 308 ms depending on the shape — cheap, and between them 17.7% of the
file. Eight reads out of nine is a true fact about a cost that turned out not to matter, which is
the failure this ADR pre-registered against itself two sections down.

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

~~**B is the one that reaches the three quarters.** A is better by count on
`pk_and_sequence_for` — the statement the row was opened on — and B is what turns an ordinary
statement's eight catalog reads into one. Since the ordinary statements are 907 of 1,354 and about
three quarters of the time, an option that only helps the 447 buys about a quarter of the cost, which
is the arithmetic that opened this ADR.~~

**Struck: run 117 says the few *are* the cost** — `pk_and_sequence_for` alone is 71.8% — so this
paragraph's whole argument is void. **The recommendation survives on other grounds**, which are in
*"What changes in the ranking, and what does not"* below, and it is worth saying plainly that the
ranking outliving its reason is luck rather than judgement.

**A and B are not a ladder** — B removes the reads A batches, so choosing A and later wanting B
means doing B anyway. C is orthogonal to both and is worth least once either lands.

**Recommendation: B, and it waits on the user.** ~~It is the one whose saving scales with the number
of statements rather than with the size of one~~ — struck, and replaced by run 117's own grounds:
A would make a 4.5 ms statement pay an O(catalog) scan to fix a 2.4 s one, and B removes the five
repeated whole-catalog loads that **are** the 2.4 s. It aligns with
[ADR 0102](0102-the-catalogs-read-path.md)'s finding (the catalog *version read* is a small fraction
of a statement, so the cost is in the reads nobody was counting), and its correctness premises are
three rules the node already has rather than three it would need. What it asks the user for is the milestone: a cache whose
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

## What run 117 said

`esker-rails-harness/results/run-117.md`, 2026-09-10, at main `47cee86a`: `transactions_test`
finished green — 105 runs, 484 assertions, 0 failures — in **1,465 s**, inside the watchdog it had
stopped at twice. Every shape priced:

```text
shape                        n      p50      p95      max    subtotal   share
pk_and_sequence_for        483   2382.4   3370.0   3489.1    1145.4 s   71.8%
other                      592      8.2   1078.3   1220.4     189.2 s   11.9%
app SELECT                 315    307.9    639.3    695.5      91.0 s    5.7%
DDL DROP TABLE             274    317.2    352.1    786.4      85.6 s    5.4%
txn control                661      0.1     93.9    121.8      13.4 s    0.8%
fixture MAX(id)            470      4.5      5.6      7.7       2.1 s    0.1%
catalog introspection total                          1,155 s = 72.4% of statement time
```

### The three questions, answered — and the first one refutes this ADR's argument

**1. What does an ordinary statement cost, and how much is below the SQL?** The question assumed
"ordinary" was one bucket. It is not: `fixture MAX(id)` is **4.5 ms**, `other`'s median is
**8.2 ms**, and an `app SELECT` is **308 ms** — a factor of 68 across three shapes this ADR called
ordinary. And the load-bearing claim above is **wrong**: catalog introspection is **72.4%** of
statement time, not the ~26.5% run 114 projected, so the "other three quarters" this ADR was built
around does not exist. The 907 unpriced statements were priced and they are cheap.

**So the sentence *"eight of a point read's nine reads are catalog and one is the row"* stayed
true and stopped mattering.** It is a true fact about statements that are 5.7%, 0.1% and 11.9% of
the file. That is exactly the failure this ADR pre-registered — *"a true fact about a cost that
does not matter"* — and it landed on the argument rather than on the recommendation.

**2. What is transaction control worth?** **0.8%**, p50 **0.1 ms** over 661 statements. The census
said it makes zero KV reads; the price says that zero is also cheap. So the reads really are where
the time is, and question 2's alternative — *"if they are expensive the cost is the TSO or the
prewrite and none of the three options touches it"* — is closed.

**3. Does `pk_and_sequence_for` still hold 26.5%?** No: **71.8% on its own**, 483 statements at
p50 2,382 ms. r1 names both of the compounding errors as its own — pricing the statement from
`schema_test` (1,051 ms) instead of `transactions_test` (2,382 ms), and pricing the residue at
52.5 ms instead of 8.2 ms — and both pushed the same way.

**4 (the one nobody asked for). Is it bimodal?** `pk_and_sequence_for` is **not**: p50 2,382,
p95 3,370, max 3,489, a 1.5x spread — a hundred five-second statements would have shown here and
did not. **But `other` is, violently.** 592 statements, median **8.2 ms**, p95 **1,078 ms**,
subtotal 189 s: 592 x 8.2 ms is 4.9 s, so **184 of its 189 seconds are in the tail** and its mean
is 39x its median. `txn control` has the same shape (p50 0.1 ms, p95 93.9 ms). **After
`pk_and_sequence_for`, that tail is the second-largest thing in the file, and nobody has asked what
is in it.** It is worth one `sort -k p95` before any of this ADR's options is built.

### The number that looks like it refutes #49, and does not

Run 117's catalog-stats section reads **184,361 catalog views x 195 us = 36 s = 2.3%** of statement
time, and draws the conclusion: *"the catalog READ is 2.3% of this file, while the
catalog-INTROSPECTING statements are 72%. The cost is in what those statements do above the read
path, not in the reads they make."*

**Read as written that would refute every option here** — if reads are 2.3%, removing reads cannot
buy two thirds. It does not, and this ADR's census is what says why: **that 2.3% is the *version*
read and nothing else.** It is the one point read ADR 0102 instrumented, the one every catalog view
opens with. `pk_and_sequence_for` makes **60** KV reads and **two** of them are that read. The
other fifty-eight — five whole-catalog loads, sixteen repeats of one schema key, fifteen
tenant-wide scans — are precisely *"what those statements do above the read path"*, and they are
reads too. They were simply not the read that instrument watched.

That is the ADR's contribution to run 117 rather than the other way round, and it is why the two
numbers stand together: **the version read is 2.3%; the reads nobody was counting are most of the
72.4%.**

### And the census predicts the 2.3x that r1 calls its own error

The same statement costs 1,051 ms in `schema_test` and 2,382 ms in `transactions_test`. The census
says why it *could*: five of its reads are **tenant-wide scans**, so its cost is O(relations), and
the two files reach it with different-sized catalogs. **That is a prediction and not a finding** —
what would test it is one number per file, the relation count at the moment the statement runs,
against the ratio 2.3. If they track, the O(catalog) reading is right and option B's estimate is
conservative; if they do not, something else varies between the files and this ADR has not found it.

### What changes in the ranking, and what does not

**The recommendation stands and its reason does not.** This ADR ranked B above A because B helps
the many and A helps the few; run 117 says **the few are the cost**, so that argument is dead. B
still wins, on two grounds the measurement supports:

* **A's downside got sharper, not softer.** A makes every statement pay an O(catalog) scan, and the
  file has 470 `fixture MAX(id)` at 4.5 ms and 661 transaction-control statements at 0.1 ms. Making
  a 4.5 ms statement O(catalog) to fix a 2.4 s one is a trade this file would notice.
* **B removes the repetition that *is* the 2.4 s.** Five whole-catalog loads per statement, four of
  them exact repeats, in a file where that statement runs 483 times.

**r1's own projection of #49 is the number to be judged against**: 35 -> 3 round trips takes the
file from 1,466 s to **496 s = 8.3 min**, two thirds off. That is the before-number, measured at
full file length on a quiet box.

### One figure that did not reconcile

The summary relayed to this lane gave the version read as **452,832 reads x 175 us = 79 s = 5.0%**.
`results/run-117.md` says **184,361 x 195 us = 36 s = 2.3%**, and the relayed figures appear nowhere
in it. Both say the same thing about ADR 0102 — the version read is a small fraction — so nothing
here turns on it, and it is recorded rather than reconciled because a number that differs by 2.2x
between a report and its summary is worth one question to whoever holds the tap.

**Not decided here, and stated so a reader does not infer it:** whether any of this is in v1.2 at
all, and whether run 117's per-statement prices change the ranking. If the 907 statements turn out
to be dominated by something this census cannot see — the TSO, the commit, the wire — then none of
the three options is the answer and the row moves.
