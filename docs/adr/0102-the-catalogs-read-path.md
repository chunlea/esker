# ADR 0102 — The catalog's read path (draft, no decision)

Status: **proposed, with a measurement plan and its criteria fixed in advance** (2026-09-09) ·
Number 0102 reserved by the coordinator. The instrument is built (`751e2299`); **nothing else is
decided here**, and the criteria below are written before the numbers exist so that reading them
cannot choose the answer — the same discipline that refuted
[ADR 0100](0100-a-region-between-leaders-waits-on-the-callers-deadline.md) and closed
`docs/plans/debts-v1.1.md` #40.

## Context — one region is on the path of every statement, and that is the key space, not a bug

The reserved layout (`docs/DESIGN.md` §3) orders the namespaces by their first byte:

```text
'm' ++ …   cluster metadata — the catalog lives here ('m' ++ "sql" ++ 't'/'v'/'n' ++ tenant ++ …)
'r' ++ …   RawKV
't' ++ …   the SQL logical key
'x' ++ …   TxnKV — where every SQL row is actually written
```

`'m'` sorts below `'x'`, and a split boundary is chosen inside the data, so **the catalog stays in
the left-most region for the life of the cluster**. It is region 1 on every cluster this project
has run.

`Catalog::view_at` is on the path of every statement and reads **two counters** from that region —
the tenant's `catalog_version` and the cluster's, summed — plus a layout check, **once per
transaction**. So every statement on every SQL node makes at least two point reads of one region,
and its answer must come from the leader of that region.

**The measured consequence** is `docs/plans/debts-v1.1.md` #34: when that one region has no leader
for a moment, *every* statement on the node is refusing, and seven of fourteen sightings of a
leaderless region named it — not because region 1 is special to Raft, but because it is the only
region every statement touches. #40 is the mechanism behind those moments and
[ADR 0101](0101-a-batch-of-ticks-never-carries-a-whole-election.md) removes one of its causes; none
of that changes the shape this file is about.

## What any answer has to keep

Three properties, all from `crates/esker-sql/src/catalog/mod.rs`'s own module docs, and each one is
a correctness rule rather than a preference:

1. **One version for a whole transaction.** A statement cannot see two shapes of one table, and a
   transaction cannot see a table appear halfway through it.
2. **A transaction that has run DDL reads its own writes and publishes none of them.** Its view
   sits at a version no committed transaction has reached, so that view is never cached.
3. **A cache may not serve a definition from a transaction's own future.** A transaction whose
   snapshot is older than the cached version reads through to the store.

## The three shapes

### (a) A cached version — and the lease it needs is already built

The node keeps the version it last read and answers later transactions from it, instead of reading
the counters again.

**The draft said this needs a push and a new wire tag. It does not.**
[ADR 0028](0028-the-schema-lease.md) already ships the guarantee: `Pd::SchemaLease` (0x0307)
answers `lease_ms` — *how long a node may serve writes from a cached schema* — and PD's schema-change
step clock is timed against it, so a step cannot outrun a live lease. That is exactly the promise a
version cache needs, and `Backend::schema_lease_remaining` already returns it, already fails closed
on `None`, and is already refreshed once per lease period rather than per statement.

**So the proposal, concretely:**

* **What is cached**: for one tenant, the version `view_at` computes — the sum of that tenant's
  `catalog_version` and the cluster tenant's — together with the instant it was read. One entry per
  tenant, on the node, beside the definition cache that is already keyed by that version.
* **When it may be used**: while `schema_lease_remaining()` is `Some(d)` **and** the entry is
  younger than the lease that was live when it was read. Both halves, because a lease that lapsed
  and came back is not the same lease: a DDL may have stepped in between, which is the whole reason
  the clock is a timer.
* **Where it is dropped**, and this is the list that has to be exhaustive:
  1. `schema_lease_remaining()` returns `None`, or a shorter remaining time than the entry's age —
     the fail-closed path ADR 0028 already has;
  2. this node commits catalog DDL — the same points `Executor::catalog_written` is set, which the
     pin below already hooks;
  3. the entry's own age passes `lease_ms`, whether or not a refresh has happened since;
  4. `LeaseRefresher` observes a lease it did not renew — a gap, not a renewal.
* **What it does not cover**: another node's DDL *within* the lease window. It does not have to —
  that is what the lease's timer buys, and it is the same bound writes already run under.

### How it relates to `view_pinned`, which is already in the tree

They are not the same mechanism and the difference is the safety argument:

| | `view_pinned` (built) | the version cache (this option) |
|---|---|---|
| scope | inside **one transaction** | across transactions, on the node |
| why it is safe | the transaction's snapshot is fixed, so the version cannot move | PD promised not to step a schema change faster than the lease |
| what it removes | the second and later reads of one statement — measured 22 → 12 | the **first** read of each transaction, which is what is left |
| what invalidates it | the transaction ending, or writing the catalog | the four points above |

The pin is the cheap half and it is done. The cache is the half that crosses a snapshot boundary,
which is why it needs a promise from outside the node rather than an argument about one.

### The test that has to exist before it lands

**"After an invalidation, no statement reads the old version"** — asserted on the *consequence*
rather than on the counter, which is this register's own rule (`a-catalog-write-must-bump-the-version`):

1. two sessions on one node: session A runs `ALTER TABLE t ADD COLUMN c`, commits; session B — a
   different session, so a different transaction and a different pin — then runs
   `INSERT INTO t (…, c) VALUES (…)` and **must** see the column. This is the local invalidation,
   point 2 above;
2. the lease's own path, which is the one a unit test cannot fake: a node whose
   `schema_lease_remaining` is driven to `None` must re-read rather than answer from the entry, and
   the assertion is again a consequence — a statement that would be wrong under the old version;
3. and the one that decides whether the bound is real: **two SQL nodes**, DDL on the first, a
   statement on the second before its lease would have expired. Under the proposal that statement
   may still see the old definition, and the test's job is to pin *how long* that window is and
   that it never exceeds `lease_ms`. `cluster_harness` starts one SQL node today; this needs two.

### (b) A lease read, or a follower read, of the version

Keep the read; stop requiring the leader to answer it. A learner already answers fragments this way
(`Store::catch_up` runs a `ReadIndex` round through `RaftPeer::read_index_as_learner`), and the
same machinery would answer a catalog counter from a replica.

* **Consistency**: a `ReadIndex` round is linearizable — the replica establishes the leader's commit
  index and waits to apply through it — so rules 1 to 3 hold unchanged. It does not remove the
  dependency on the region *having* a leader, though: the round asks one.
* **The contract**: unchanged. This is the same read from a different peer.
* **What it needs**: no new tag — the round already exists — but the SQL node must be able to
  address a *replica* of the catalog region, which today it cannot: routing hands out the leader.
  That is a client and PD change rather than a protocol one.
* **What it saves**: nothing on a healthy cluster, and a great deal when the leader is busy: the
  read stops queueing behind whatever else that one region is doing.

### (c) A split exemption: the catalog gets a region nothing else can make busy

Cut a region at the `'m'`/`'r'` boundary at bootstrap and never split or merge it, so the catalog's
region holds the catalog and nothing else.

* **Consistency**: none of the three rules moves. This changes *what else shares* the region, not
  how the version is read.
* **The contract**: unchanged.
* **What it needs**: a PD change — a region PD creates and never chooses as a split candidate — and
  a rule in `esker_store::split` that refuses a boundary inside `'m'`. No wire change at all, which
  makes it the cheapest of the three by a wide margin.
* **What it saves**: it removes the *coupling*, not the round trip. Every statement still reads the
  version over the network; what it stops is a table's load, a table's splits and a table's
  elections deciding whether the catalog can be read. On the evidence in #34 that coupling is the
  expensive half.

## The measurement plan — written before the numbers, like the last one

### ① What is measured, and why each number decides something

| number | why it decides | where it comes from |
|---|---|---|
| **catalog views per statement**, by statement class — point read, range, write, DDL | if a write already pays for a dozen round trips, one more is noise; if a point read pays for two and does one, it is half the statement | the instrument below, against a workload whose statement mix is known |
| **the share of those reads that reach the store** rather than the cache | (a) is worth building only if the cache is missing often; a cache that already answers is a lease with no lease | the same instrument's `of the store` count |
| **what a catalog read costs**, mean and tail | this is the *whole* of what (a) and (b) can remove | the same instrument's histogram |
| **the catalog region's share of a node's traffic**, against another region | (c) is about **coupling**: it is worth doing when one region carries a load nothing else can shed | **not this instrument** — see the limitation below |

**The limitation, stated rather than discovered later.** The instrument counts and times the read;
it does not say **which region** answered it. The catalog's keys sort below all data, so it is the
left-most region by construction on every cluster this project has run — but *proving* that per
read needs the client's routing to report what it chose, which is a larger change than an
instrument. The fourth row above therefore needs a second instrument, and until it exists (c) is
argued from the key space rather than measured.

### ①b What the two runs actually measured, and on which topology

| | run 111 (Rails suite) | run 112's calibration (two files) |
|---|---|---|
| backend | **`MemoryBackend`** — `esker-sql HOST:PORT` with no store addresses: an in-process `BTreeMap`, no store, no PD, no Raft, no socket | **the real topology**, node `534604a5` |
| catalog views | 4,790,406 | 53,387 |
| mean per read | **≤0.5 µs** — a table lookup | **232 µs** |
| distribution | `<100us` 4,790,317 · `<1ms` 86 · `<10ms` 3 | `<100us` 903 · `<1ms` 52,183 |
| repeats of the same version | not measured — the field was a constant then | **41,061, or 77%** |

**Three things follow, and the third is the one that decides.** The read is **not free** once a
store is under it. The distribution **moved as a whole** rather than growing a tail — 52,183 of
53,387 land in the same bucket — so this is a cost, not a stall. And **three quarters of the reads
return the version the read before them returned.**

For scale beside it: the same calibration puts the real topology at about **110× the wall clock of
the fake backend per file** (229 s against 2 s). The Raft round trip a statement makes is the bulk
of that; a catalog read is one of its parts.

### ② Where the instrument lives

`crates/esker-sql/src/catalog/stats.rs`, counting at `Catalog::view_at` — **the one place every
statement passes through**, and nowhere else. Off unless `ESKER_CATALOG_STATS` is set: an
environment variable and not a cargo feature, because the run that should produce these numbers is
the ActiveRecord suite against the *released* node binary and a feature would mean a special build
nobody has. Off costs a cached atomic load. On, one line every ten seconds under
`esker::catalog::stats`:

```text
catalog views N, of the store N, mean N us, worst N us, buckets <100us N <1ms N <10ms N rest N
```

### ③ What to run it against

**The ActiveRecord suite, on the run that was going to happen anyway.** It is a real statement mix
rather than one this lane would have invented, it is thousands of statements rather than a
microbenchmark's one shape, and it costs no machine time that was not already being spent — r1
starts the node with the variable set and copies the line into the run report. A synthetic
`esker-cli bench` arm is worth having **only as a control**, to say what the number looks like when
the statement mix is deliberately uniform; it decides nothing on its own, because the question is
about a real workload's shape.

### ④ The criteria, fixed now

1. **Catalog reads are under 2% of a statement's latency at the median, and the store-side share is
   under 10%** → **nothing is built**: the row is recorded as *measured and not worth moving*, and
   ADR 0102 closes as answered. A cache that is already hit 90% of the time is the cheap version of
   (a) that exists today.
2. **Either number is above its bar** → the option is chosen by which number is above it:
   * the **store-side share** is what (a) removes → a cached version with a pushed invalidation,
     and its lease gets its own ADR because that is where its correctness lives;
   * the **cost of the read itself**, with the share already low, is what (b) removes → a follower
     read, which needs no new tag and does need the client to be able to address a replica;
   * neither, and the pain is that one region carries everything → **(c)**, which is the cheapest
     and the only one that removes the coupling rather than the round trip.
### A unit that is not this ADR's, and comes before it

**Measured, and the first claim about it was wrong.** This paragraph said "thirteen places, and
each one re-reads the two version counters", which was inferred from counting call sites and never
measured. Counting them per statement instead
(`crates/esker-sql/tests/catalog_reads.rs`, with the instrument on):

| statement | views | of which repeats |
|---|---:|---:|
| `CREATE TABLE` | 1 | 0 |
| `INSERT`, one row | 2 | 1 |
| `INSERT`, three rows | 2 | 2 |
| `SELECT`, point | 2 | 2 |
| `SELECT`, range | 2 | 2 |
| `UPDATE`, point | 2 | 2 |
| `DELETE`, point | 2 | 2 |
| `ALTER TABLE ADD COLUMN` | 3 | 3 |
| `SELECT`, self join | 4 | 3 |
| `BEGIN` / `COMMIT` | 0 | 0 |
| **the whole run** | **22** | **19** |

**Two per ordinary statement, not thirteen** — the call sites do not all fire — and **19 of 22 are
repeats**: 86% of the reads return the version the read before them returned. So the redundancy is
real and it is a factor of two, not of thirteen. Against run 111's 4,790,406 views that is about
2.4 million statements, which is the right order for that suite.

That is not this ADR's question — every one of its three shapes changes *where* the version is read
from, and this changes *how many times* — and it should be a small unit of its own, for three
reasons:

* **it is cheaper than any shape here**: caching the view per transaction inside the executor needs
  no wire tag, no placement-driver change, no lease and no ADR — and it halves the reads rather
  than removing them, which is worth knowing before it is built;
* **it comes first, or this ADR measures the wrong thing.** A statement that reads the version
  thirteen times multiplies whatever a read costs by a number that belongs to the executor's
  structure and not to the catalog's read path. Measuring (a), (b) or (c) against that is measuring
  the executor;
* **and the obvious worry is already answered, which is why this is a cost question and not a
  correctness one**: every one of those reads goes through the same `txn`, and a transaction's
  snapshot is fixed, so the version cannot move between them. A statement cannot see two shapes of
  one table by taking two views. What it can do is pay for the same answer thirteen times.

The shape of the unit, in the order this lane has learned to do them: **count the reads per
statement class first** — done, above — **then merge them**, **then count again**, so the change is
reported as a difference and not as an intention.

**Done, and measured on both sides** (`crates/esker-sql/tests/catalog_reads.rs`, the same
instrument):

| statement | before | after |
|---|---:|---:|
| `CREATE TABLE` | 1 | 1 |
| `INSERT`, one row | 2 | **1** |
| `SELECT`, point | 2 | **1** |
| `SELECT`, range | 2 | **1** |
| `UPDATE` / `DELETE`, point | 2 | **1** |
| `ALTER TABLE ADD COLUMN` | 3 | 3 |
| `SELECT`, self join | 4 | **1** |
| **the whole run** | **22** | **12** |

**An ordinary statement now reads the catalog version once, and a self-join once instead of four
times.** `ALTER TABLE` is unchanged and must be: a transaction that has written the catalog reads
its own uncommitted DDL, so it takes an uncached view every time. At run 112's 232 µs a read that
is about **232 µs saved per ordinary statement** and 700 µs on the self-join.

**Why it is safe, in one sentence**: the version is a property of the transaction's snapshot, so it
cannot move while the transaction lives — and the pin is dropped at every place `catalog_written`
is set or cleared, which is every point a transaction begins, ends, or becomes a DDL one.

**What the middle step waited on, and no longer does.** Halving a read is worth
building when the read costs something; run 111's half-microsecond is a `MemoryBackend` table
lookup and says nothing about a read that crosses a socket and Raft. Building the merge before that
number exists would be the mistake
[ADR 0100](0100-a-region-between-leaders-waits-on-the-callers-deadline.md) records: a change that
looks like a fix, ships a smaller number, and leaves the question unanswered.

3. **The tail is what decides against the median.** A mean of 200 µs with a `rest` bucket that is
   never empty is a different system from a flat 200 µs, and the second is the one nothing needs to
   be done about.

## Not decided here — and the number that would change that is now collectable

This file exists so the choice is made against the same facts by whoever makes it, and so that the
consequence recorded in #34 has somewhere to point. **It decides nothing**, and each of the three
shapes above needs its own ADR when it is chosen — (a) in particular is two decisions wearing one
name, since the lease is where its correctness lives.
