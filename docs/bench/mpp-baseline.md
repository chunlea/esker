# MPP baseline — where a distributed aggregate's time goes today

[ADR 0022](../adr/0022-columnar-learner-replica.md) **milestone 5** is the one milestone that ADR
gates on a number: MPP exchange is built *"last, and only if measured"*. This file is that number,
and [`docs/plans/phase-16-mpp.md`](../plans/phase-16-mpp.md) §10 is the verdict written from it.

Not a gate. `CLAUDE.md` keeps benchmarks runnable and recorded so regressions are visible, and out
of the test gate so nobody tunes before correctness is proven.

> **Status: run 1 taken, and it found something bigger than the number it went looking for.**
> Numbers in [§7](#7-run-1--2026-09-05); the finding that decides the verdict is
> [§8](#8-why-there-is-no-fragment-axis-and-what-that-means). This file records the method
> *before* the numbers deliberately: a measurement whose method is written afterwards is a
> measurement whose method was chosen by its results.

## 1. The question, stated so it can be wrong

> When a high-cardinality `GROUP BY` runs on the columnar path across N stores, **what share of
> wall time is the SQL node's finishing step** — the second level of the two-level aggregate — and
> how does that share move with N and with cardinality?

An exchange moves exactly that share somewhere else. It does not make the scan faster, it does not
reduce the bytes the stores send, and it does not touch anything a query does before the partials
exist (`phase-16-mpp.md` §2). So the share *is* the ceiling on what milestone 5 can buy, and a
share that is small is a verdict on its own.

## 2. The command

One command builds nothing else and leaves nothing running:

```bash
ESKER_REPO=<worktree> ESKER_TARGET_VOL=esker-target-linux-mpp \
  ~/workspace/lab/esker-docker/in.sh bash -c '
    cargo build --release -p esker-cli -p esker-sql
    /target/release/esker-cli bench-mpp \
      --stores 6 --rows 2000000 --groups-high 100000 \
      --region-split-size 33554432 --repeats 3 --base-port 24160'
```

It starts a placement driver, six stores and one SQL node as child processes, loads a seeded
table, asks for a columnar copy with the `ALTER TABLE ledger SET (columnar_replicas = 1)` a user
would write, waits until the planner actually answers from the columns, measures, and kills
everything. `--dir PATH --keep` leaves the data and every child's stderr behind.

**Inside the container** (`~/workspace/lab/esker-docker`), because the decomposition in §4 is read
from `/proc` and because a host build has taken this machine from measured passes more than once.

## 3. What is measured, and what proves it

### The four queries

| name | statement | what it is for |
|---|---|---|
| `control-scan` | `SELECT count(*), sum(amount) FROM ledger WHERE day < 300` | **the control.** A full scan with a filter and one output row. No exchange can make it faster — there is nothing to shuffle. If its arms move with the others', the run measured the machine. |
| `group-low` | `SELECT g32, count(*), sum(amount) FROM ledger GROUP BY g32` | 32 groups. The finish is a handful of rows, so what this costs above the control is the **round trips**, not the merge. |
| `group-high` | `SELECT ghigh, count(*), sum(amount) FROM ledger GROUP BY ghigh` | an answer comparable in size to the scan. What this costs above `group-low` is the **shipping and the merge** — the quantity an exchange takes away. |
| `join` | `SELECT count(*) FROM ledger JOIN dim ON ledger.ghigh = dim.k WHERE dim.bucket = 3` | two large tables on a key neither is partitioned by. It runs on rows whatever the session says: ADR 0040 Decision 4 substitutes one plan shape and a join is not it. Its number is about the row engine, and that is the point. |

The pair `group-low` / `group-high` is the decomposition. Both scan the same rows and project the
same number of columns; they differ in how many groups come back. Subtracting one from the other
isolates the cardinality-dependent cost and leaves the scan and the round trips behind — and it
needs no counter that does not exist.

**They do not decode equally, and that is why both sides are measured separately.** `g32` has 32
distinct values and `ghigh` has a hundred thousand, so `ghigh`'s column compresses far worse and
costs more to decode — which would inflate a difference read off wall time alone. That extra work
is the *store's*, and it lands in the stores' CPU column, not the SQL node's. So the quantity an
exchange would move is the **SQL-node CPU delta** between the two, and the stores' delta beside it
is what says how much of the wall-time difference was never the SQL node's to give up. Measuring
the two sides separately is what turns this confound into a reading.

The cardinality axis is swept by re-running with different `--groups-high` rather than by adding a
fifth query, so the four queries stay the four the question is about.

### The table

Seven columns, and every measured query reads two of them, because the routing rule is a **ratio**:
a query is planned on columns when it projects at most half the table's columns
(`esker_sql::plan::routing::RATIO`). A narrower table would fail the ratio and there would be
nothing to measure.

```sql
CREATE TABLE ledger (
    id int8 PRIMARY KEY, day int8, g32 int8, ghigh int8, amount int8, label text, payload text
);
```

The grouping columns are `id % k`, not random, and two properties follow:

* the cardinality is **exactly** `k`, so the record says "100,000 groups" rather than "about that";
* every region's rows cover **every** residue, so each region ships nearly the whole group set and
  one node merges `regions × groups` partials down to `groups`. That is the worst case for a
  two-level aggregate and precisely the case an exchange exists to remove. A grouping key
  correlated with the primary key would have each region ship a disjoint slice, which the SQL node
  concatenates for free — and would make this measurement say the opposite thing.

`amount` is seeded pseudo-random so the sums are not answerable from the stripe statistics.
`payload` is never projected: it is the width that makes the ratio real and the bytes a columnar
scan gets to skip.

### Two assertions on every single timed run

**The engine.** Every timed statement is preceded by an `EXPLAIN ANALYZE` of the same statement,
and the `Engine:` line must say what the arm asked for. A routed plan that fell back to the rows is
**silent to the client by design** (ADR 0040: the answer is the one the snapshot always had), so
without this assertion a run of four fallbacks would report four plausible numbers about a plan
nobody asked for. A columnar arm whose plan says `Engine: rows` fails the run.

**The answer.** Both arms must return the number of rows the shape predicts. Two engines
disagreeing is the worst failure this feature can have (ADR 0022, "the honest cost"), and a
benchmark is a place it would otherwise pass unnoticed.

### Interleaving, the warm-up, and the load

Arms are **adjacent in time**: for each repeat, for each query, columnar then rows. Not a block of
one arm followed by a block of the other — on a machine shared with five other lanes that measures
the machine. Three repeats; medians reported with the min and the max beside them, and the median
of an even count is a value that was actually measured rather than a mean of two that were not.

One untimed pass runs first and is discarded, so the first measured repeat is not paying for a cold
page cache on either engine.

The kernel's one-minute load average is printed **at the start and at the end** of the measured
phase, in the record. A run whose load moved is a run whose arms are not comparable, and the reader
gets to see that rather than trust it.

## 4. The instrument: `/proc`, because the node has no clock

The SQL node reports **no time at all**. `esker_proto::fragment::ScanStats` carries five counters —
stripes considered, stripes read, chunks decoded, rows scanned, rows matched — and `EXPLAIN
ANALYZE` prints those and no execution time, for any plan, routed or not. So the share this file
exists to report cannot be read from the node by anybody.

Adding a counter was the obvious answer and is the wrong one: a measurement must not change what it
measures, and the crates under measurement are ones this lane is forbidden. So the split is read
from outside, per process, from the kernel, around every timed statement:

| what | where | what it means here |
|---|---|---|
| `utime + stime` | `/proc/<pid>/stat` | the stores' CPU is the scan; the SQL node's is planning, encoding fragments, decoding every partial and merging them |
| `rchar` | `/proc/<pid>/io` | a SQL node holds no engine, so what it reads is very nearly what arrived from the stores |
| `VmHWM` | `/proc/<pid>/status` | whether the two-level finish is a bounded cost or the whole answer materialised |

**What this can say:** what each process spent while one statement ran.

**What it cannot say:** what a *fragment* cost. There is no per-answer timing and no observed
byte count on the wire, because there is no seam to observe them at — a store registers with PD the
address it was told to listen on (`crates/esker-cli/src/server.rs:279`), so there is no way to put
a counting proxy between a SQL node and a store without changing one of them. The four instruments
that are missing, and where each would go, are `phase-16-mpp.md` §9a.

Resolution is one clock tick, conventionally 10 ms, which is why §5 sizes the workload so a
statement takes something over a second.

## 5. Sizing, and the two ways to get it wrong

Sizes that fit in one store's page cache show no I/O and sizes that swap show only swapping, so the
run states the container's memory and chooses between them. The reasoning, to be checked against
run 1's own numbers rather than assumed:

* **Rows.** Enough that a statement takes seconds rather than milliseconds — the tick resolution
  above — and few enough to load through the SQL node in a reasonable time. Loading is the binding
  constraint, not scanning: `docs/bench/columnar-learner.md` records 4.8 ms for a single-row put,
  so rows arrive in batched multi-row `INSERT`s and the load rate is reported beside the results.
* **Regions.** One fragment per region, chosen with `--region-split-size` and read back from
  `EXPLAIN`'s own `Fragments: N asked` rather than assumed. The splits happen **during the load,
  before the columnar copy is asked for** — a region that split after its copy was built is not
  routed at all (ADR 0040), so the order is not a detail.
* **Stores.** Six, not four. Four is the minimum that can hold a columnar learner at all — a region
  has three voters and PD places a learner on a store with **no peer of that region** — and PD
  picks the store with the fewest regions, so more stores is what lets learners for different
  regions land on different ones. **The record reports both numbers**: how many regions have a
  learner, and how many distinct stores hold one. They are not the same number and a reader will
  assume they are.
* **The scan is deliberately warm.** The bytes an exchange moves are `regions × groups × partial`,
  which has nothing to do with whether the scan hit the disk. A cold scan would add I/O to both
  arms and bury the quantity being measured under it.

## 6. Why run 1 is empty

**`esker-sql` panics on every connection when it is given real store addresses.** The connection
completes the whole startup handshake and the connection task then panics at `for_session` on a
tokio worker thread; no statement ever runs. Present at `v1.0.0` by inspection, introduced
2026-09-03 by `0510b44e`. Nothing caught it because no automated test starts a SQL node against
real stores — the Rails scoreboard runs one with no store addresses and an in-process backend.

Owned by the `h1` lane. The reproduction, the full backtrace and the fix shape are in the
coordinator's `scratchpad/pgwire-cluster-repro.md`.

`esker bench-mpp` is what found it, which is the one useful thing to say about a benchmark that has
not produced a number: the first thing it asserts is that a real SQL node over a real cluster
answers `SELECT 1`, and that assertion is what turned red.


## 7. Run 1 — 2026-09-05

| field | value |
|---|---|
| commit | `09477207`, containing main `78ff5b70` |
| binaries | `cargo build --release -p esker-cli -p esker-sql`, in the container |
| machine | aarch64 Linux container, MemTotal 15.7 GiB, cgroup limit **max (uncapped)**, 16 CPUs |
| cluster | 4 stores, 1 placement driver, 1 SQL node, all child processes on loopback |
| heartbeats | region 60 s, tick 1 s — the shipped defaults, which is what `esker cluster start` uses |
| rows | 200,000, loaded in 44 statements of 5,000 at **2,152 rows/s** |
| groups | 32 (`g32`), 20,000 (`ghigh`) |
| repeats | 3, interleaved, after one discarded warm-up pass |
| load average | **5.94 at the start, 1.59 at the end** — inside a quiet window the coordinator held other lanes out of |

The join is not in this set (`--no-join`): at this size it costs tens of seconds against the
aggregates' tens of milliseconds, and it cannot reach the columnar path at all, so under a time
budget it is the first thing to drop. Its shape is recorded in §7c from the 40,000-row pilot.

### 7a. The numbers

Every row is the median of 3 runs, and every routed row was asserted by `EXPLAIN ANALYZE` to have
actually run on the columns.

| query | engine | wall median | wall min–max | SQL-node CPU | stores' CPU | loopback bytes |
|---|---|---|---|---|---|---|
| `control-scan` | columnar | **0.019 s** | 0.018–0.020 | 0.00 s | 0.01 s | 6.4 KiB |
| `control-scan` | rows | 6.610 s | 6.576–6.628 | 0.10 s | 7.00 s | 20.6 MiB |
| `group-low` (32) | columnar | **0.030 s** | 0.029–0.036 | 0.00 s | 0.03 s | 8.0 KiB |
| `group-low` (32) | rows | 6.633 s | 6.598–6.809 | 0.11 s | 7.00 s | 20.6 MiB |
| `group-high` (20,000) | columnar | **0.055 s** | 0.055–0.057 | 0.01 s | 0.04 s | 1013.2 KiB |

A second point taken immediately afterwards, at a 4 MiB region-split size instead of 512 MiB,
reproduced it to the millisecond — 0.018 / 0.030 / 0.054 s, 6.4 KiB / 8.0 KiB / 1013.5 KiB. That
agreement is a **reproducibility check and not a second data point**, for the reason §8 gives.

### 7b. What the decomposition says

The three columnar rows hold the rows scanned and the columns projected constant and vary only the
number of groups, so the differences are the cardinality-dependent cost and nothing else:

```text
1 group        19 ms      6.4 KiB shipped
32 groups      30 ms      8.0 KiB          +11 ms, +1.6 KiB
20,000 groups  55 ms   1013.2 KiB          +25 ms, +1005 KiB over 32 groups
                                           +36 ms, +1007 KiB over one group
```

**The entire cardinality-dependent cost of a 20,000-group aggregate over 200,000 rows is 36 ms**,
and the SQL node's own CPU inside the whole 55 ms is 0.01 s — at the 10 ms clock tick, so it is
*at most* 10 ms and the instrument cannot say less. Shipping is **≈50 bytes a group** (1,007 KiB
over 20,000 groups), which is the quantity an exchange redistributes rather than removes.

The control moves not at all across the three: 0.019 s with one group, and 0.018 s in the repeat
point. That is what says the run measured the query rather than the machine.

### 7c. The other half, which cuts the other way

Columnar against rows at this size is **6.610 s → 0.019 s**, about 350×, and 20.6 MiB shipped
against 6.4 KiB. That is not a claim about columnar storage in general and must not be quoted as
one: the row arm is a full `SeqScan` through the MVCC layer returning one aggregate row, which is
the shape ADR 0022 exists to fix, and `docs/bench/columnar-m2.md` records the other direction — a
scan that reads *every* column is **slower** columnar than row-wise.

The join, from the 40,000-row pilot: 8.1 s and 7.7 s on the two arms, identical because
`esker.engine` cannot move it — 1.2 s of SQL-node CPU against 10.7 s of stores' CPU. It runs on
rows whatever the session says.

## 8. Why there is no fragment axis, and what that means

**Every point in §7 has one region and one fragment**, and that is not for want of asking. The two
points differ only in `--region-split-size`, 512 MiB against 4 MiB, over roughly 20 MB of table.
Both answered `| regions | 1 | with a columnar learner | 1 |`. The threshold made no difference
because the size being compared against it is **zero**:

* `esker pd inspect` reports the region holding 200,000 rows as `~0 bytes`;
* `esker_store::split::approximate_size` (`crates/esker-store/src/split.rs:174`) measures
  `db.approximate_size(cf::DEFAULT, ['r' ++ start, 's'))` — the **RawKV** namespace,
  `esker_keys::prefix::RAW = b'r'`, in the **default** column family;
* SQL rows are written under **`prefix::TXN = b'x'`** by the Percolator layer, into the `write` and
  `default` column families — which `docs/bench/columnar-learner.md`'s own WAL dump shows as
  `cf=2 Put "xt…"`.

So a region holding nothing but SQL data measures as empty, never crosses any threshold at any
setting, and never splits. The boundary search beneath it has the same shape: `split.rs:115` finds
its split key by `strip_prefix(&[prefix::RAW])`.

**The consequence for this ADR is larger than the missing axis.** MPP exchange shuffles
intermediate results *between* columnar nodes, and a SQL table that is always one region always has
exactly one fragment — so there is nothing to shuffle, at any data size, for any query. Milestone 5
is unreachable from SQL until a SQL table can occupy more than one region.

This is `esker-store`'s and is escalated rather than acted on: a measurement must not change what it
measures, and this lane does not own that crate.



## 9. The multi-region correctness half — 2026-09-05

ADR 0073 landed and a SQL table splits, so §8's missing axis became reachable and the correctness
half was run before the timing half. It found a real defect — and this section's **first version
described it wrongly**, which is recorded below rather than quietly fixed, because the way it was
wrong is the more useful half.

### 9a. What is true

Five regions, each confirmed from its own leader, each with a columnar learner placed:

| statement | row engine | columnar |
|---|---|---|
| `count(*)`, whole table | `08006 … key is not in region 1` | **1 row** |
| aggregate with a filter | `08006` | **1 row** |
| `GROUP BY`, low cardinality | `08006` | **32 rows** |
| `GROUP BY`, high cardinality | `08006` | **5 rows** |
| the join (semi-join shape) | `08006` | **1 row** |
| range scan across the boundary | `08006` | `08006` |
| full row scan, ordered | `08006` | `08006` |
| point read below the split / past it | 1 row | 1 row |
| insert past the split | commits | (duplicate key, proving the first did) |

**The row scan path does not iterate regions.** It asks the first region for keys the first region
does not hold, and `KeyNotInRegion` reaches the client as `08006` instead of moving on. Past a
table's split threshold every row-path `SELECT` that is not a point read stops working, which is
the whole of scale-out from SQL. Owned by `esker-client`'s range scan and `esker-sql`'s cursor;
`crates/esker-cli/tests/cross_region_scan.rs` reproduces it in twelve seconds.

**The fragment path does.** Five regions, five fragments, answers that agree with what the single
region gave — including the semi-join. That is
[ADR 0040](../adr/0040-the-engine-a-query-runs-on.md)'s *"one fragment per region"* demonstrated
across a real boundary, and it means the working traversal and the broken one are in the same tree.

The two rows that fail on **both** engines are consistent rather than contradictory: a bounded
range and an ordered scan are never routed to the columns (ADR 0022 rule 1), so both fall back to
the row path by design and meet the same defect.

### 9b. The version of this that was wrong, and why

The first run reported *"every scan fails across a region boundary, **on both engines**"*. That
sentence was true of that run and false of the system. That run had **0 of 10 regions with a
columnar learner**, so every columnar query refused and fell back to the row plan — and the row
plan is the broken one. What was measured was the fallback; what was reported was the columnar
path.

**It is the denominator mistake, made in the one place nothing checked it.** `routing_differential`
exists because *"a query that fell back agrees with the row engine for free"*, and it asserts
`Engine: columnar` on every comparison for exactly this reason. A hand-run diagnostic matrix
asserted no such thing, so a column headed `columnar` held numbers the row engine produced.

The fix to the harness is the same rule: **`--diagnose` reports the `ALTER`'s outcome and samples
placement until every region has a learner before it believes a column labelled `columnar`.**

### 9c. Placement after a split: a clean negative

The same weakness produced the same false reading twice, so it was tested rather than assumed.
`ALTER TABLE … SET (columnar_replicas = 1)` **committed** — reported now instead of swallowed —
and placement was then sampled rather than read once:

```text
| at   | regions | with a columnar learner |
|  3ms |    5    |            0            |
|  30s |    5    |            1            |
|  45s |    5    |            3            |
|  60s |    5    |            5            |
```

**A split half inherits `columnar_replicas` and gets a learner placed.** PD's wish list is keyed by
key range and a half is inside the table's range, so the rule predicted it and the measurement
confirms it. The earlier "0 of 10" was one reading taken immediately after the `ALTER`, at a 60 s
region heartbeat — a number equally consistent with *not yet* and with *never*, which is why a
series and not a sample is what settles it.

**No defect. Nothing for `esker-pd` or `esker-store`.**

### 9d. Five learners, two stores — the number the exchange would actually be limited by

The five learners landed on **two** distinct stores, not five. PD places one on the *healthiest
store without a peer of that region* (`esker_pd::schedule`), and with three voters and four stores
few stores are free for any given region — so learners cluster.

That is worth more to milestone 5 than it looks. An exchange's parallelism is bounded by the number
of **distinct nodes** holding the fragments, not by the number of regions, and this cluster has
five fragments on two nodes. A verdict that reasoned from region count would overestimate the
available parallelism by more than twice on the very first multi-region cluster anyone measured.
`--diagnose` reports both numbers for that reason, and §10's re-measure must read the second.

### 9e. What is still unanswered

* **The cache refreshing on `EpochNotMatch`** — not reachable while the row path fails before a
  refresh would be exercised.
* **The engines agreeing across regions** — the columnar side answers and the row side errors, so
  there is no pair to compare. This is what the differential is for and it needs the scan fix.
* **The timing half.** The exchange re-measure and the join before/after need both engines to
  complete; neither is blocked on a quiet window, and scheduling one before the scan fix would
  waste it. §7's single-region numbers stand and remain labelled single-region.


## 10. A multi-region columnar query returns N× the right answer — 2026-09-05

The correctness half was re-run on the fixed main with learners placed. It found the failure
[ADR 0022](../adr/0022-columnar-learner-replica.md) names as the worst this feature can have.

Four regions, a columnar learner on each, **every fragment answered**, `Engine: columnar`:

| query | row engine | columnar |
|---|---|---|
| `count(*)` | 20,000 | **80,000** |
| `count(*), sum(amount) WHERE day < 300` | 16,490 / 8,254,155,880 | **65,960 / 33,016,623,520** |
| `GROUP BY g32`, every group | 625 | **2,500** |
| `GROUP BY ghigh`, every group | 40 | **160** |
| the join | 2,520 | **10,080** |

**The multiplier is the region count**, and the learners were co-located — four on two stores. So a
fragment reads its store's columnar runs rather than only its own region's, and every row is
counted once per region. The row engine is correct throughout; the columnar side is silently wrong.

### Why the epoch did not catch it

ADR 0040 foresaw the shape and priced it as harmless:

> Nothing prunes a parent's copy on a split, so a fragment to each half could count a row twice —
> the epoch pins it: shards carry the epoch the planner saw, a split bumps it, and the store
> refuses the stale one. **A performance bound, not a wrong answer**, and a split-aware columnar
> copy is `esker-store`'s.

The reasoning does not hold in the order things now happen. **The splits occur before the learner
is placed**, so no shard is ever stale: every fragment carries a current epoch, is legitimately
routed, is answered — and reads too much. The epoch guards against a topology that changed under a
plan, and this is a topology that was already settled when the plan was made.

**Owner: `esker-store`**, by that same sentence — the columnar copy has to be region-scoped.

### The interim guard

A wrong answer must not stay reachable while the fix is built, so
`crates/esker-sql/src/exec/fragment.rs` refuses a table in more than one region:

```text
Engine: rows  (no fragment expresses a table in more than one region:
               the columnar copy is not region-scoped yet)
```

Verified on the cluster that produced the table above: every routed statement now names the guard,
and the two engines agree on all ten. It is **temporary**, and what retires it is
`multi_region_differential` going green — the test asserting the engines agree across regions with
`Engine: columnar` on every comparison.

The guard sits **before** the learner check: a multi-region table will not be routed whether or not
a learner exists, and naming the learner first would send a reader to place one and watch nothing
change.

### Three notes on how this was nearly missed

* **`routing_differential` cannot see it.** It is single-region, and the differential ADR 0022 asks
  for by name is exactly the defence this needed. That is the gap
  `crates/esker-cli/tests/multi_region_guard.rs` and the owed multi-region differential fill.
* **A stale binary hid it, twice.** `--diagnose` drives `/target/release/esker-sql` and the test
  harness `/target/debug/esker-sql`; rebuilding one and reading the other produced a run where the
  row path looked broken after it had been fixed, and a run where the guard looked absent after it
  had been added. Both times the binary's timestamp was the tell.
* **"0 disagreements" meant nothing twice**, for opposite reasons: once because no learner was
  placed so both arms were the row engine, and once because the guard had put them there
  deliberately. A comparison is worth what its denominator is worth, which is why the guard's test
  asserts the `Engine` line and not the answer.

### The timing half

Was parked here for a second reason — the numbers it would take were wrong by a factor of the region
count — and the store's columnar copy became region-scoped the next day, which unparked it. It ran
in a second quiet window and is **§11**, where it found something else entirely: a table that splits
under a bulk load cannot be loaded at all, so the multi-region timings this section was waiting for
still do not exist.

## 11. The timing half, run at last — 2026-09-05, second quiet window

| field | value |
|---|---|
| commit | `e841beb7` (binaries rebuilt at it before the window opened) |
| machine | aarch64 Linux container, MemTotal 15.7 GiB, uncapped, 16 CPUs |
| load average | **1.6 – 2.8 throughout** — the quietest the machine has been for any run here |
| window | 30 minutes, other lanes held out of it |
| rows | 20,000 (the plan said 60,000; §11a is why it could not be) |

The `--stores` axis was to be 4, 8 and 6 with rows and split size fixed, so that every point had the
same regions and the only variable was how many distinct stores their learners landed on. **None of
the three completed**, and what stopped them is worth more than the table would have been.

### 11a. A table that splits under a bulk load cannot be loaded

Every N point died during the `INSERT` phase, in under a minute, on an idle machine:

| N stores | rows | outcome |
|---|---|---|
| 4 | 20,000 | `08006: gave up after 10 attempts: peer is not the leader of region 34` |
| 8 | 20,000 | `08006: deadline passed after 14 attempts` |
| 6 | 20,000 | `08006: deadline passed after 14 attempts` |

Store count changes nothing, and 60,000 rows failed the same way earlier at row ~43,500. So the
control: **same rows, same stores, same machine, same binaries — only the split threshold moves.**

| `--region-split-size` | fact-table regions | load |
|---|---|---|
| 1 GiB | 1 | **succeeds**, 85 s |
| 16 MiB | 1 | **succeeds** |
| 4 MiB | several | **fails** |

That is the diagnosis rather than the observation: it is not volume, not store count, not the
machine, and not the quiet window. **A bulk load into a table that splits under it fails.**

The mechanism is the fourth unrepaired routing call site, named in `docs/plans/phase-16-mpp.md`
§J12. `router::repair_route` has three callers — both scans and the fragment enumeration — and
`Router::call`, the write path, is not one of them: `classify(KeyNotInRegion)` is `Verdict::Surface`
and `may_ask_again` is false for a write, so a write surfaces at once. Compounding it,
`learned_a_newer_epoch` deliberately does **not** reset the retry budget on a `NotLeader` hint,
because "chasing a leader around a region that is not changing is exactly the loop the budget was
put there to stop". That rule is right for a dead region and wrong for one that has just been
created by a split and is still electing — which is every region during a load that splits.

### 11b. The aggregates, one region, 20,000 rows

Median of 3, every routed row asserted by `EXPLAIN ANALYZE` to have run on the engine named.

| query | engine | fragments | wall median | SQL-node CPU | stores' CPU | loopback |
|---|---|---|---|---|---|---|
| control-scan | columnar | 1 of 1 | **0.003 s** | 0.00 s | 0.00 s | 9.8 KiB |
| control-scan | rows | 0 of 0 | 0.068 s | 0.01 s | 0.07 s | 2.1 MiB |
| group-low | columnar | 1 of 1 | **0.004 s** | 0.00 s | 0.01 s | 11.4 KiB |
| group-low | rows | 0 of 0 | 0.070 s | 0.01 s | 0.06 s | 2.1 MiB |
| group-high | columnar | 1 of 1 | **0.008 s** | 0.00 s | 0.01 s | 204.2 KiB |
| group-high | rows | 0 of 0 | 0.072 s | 0.01 s | 0.06 s | 2.2 MiB |

**9× to 23×**, and the ratio falls as the answer grows: the scan and the 32-group aggregate are
18–23×, the 4,000-group one 9×, because what the columns save is reading columns nobody projected
and that saving is fixed while the result is not.

### 11c. The join, which is what this unit was built for

| join arm | fragments | wall | SQL-node CPU | stores' CPU | loopback |
|---|---|---|---|---|---|
| columnar | 1 of 1 | **0.012 s** | 0.00 s | 0.00 s | 243.4 KiB |
| rows | 0 of 0 | 3.658 s | 0.58 s | 4.69 s | 25.1 MiB |

**305×, and 103× less network.** The row engine spends 4.69 s of store CPU; the columnar path
spends none that the sampler can see.

At the **default** `--groups-high` the same join does not route, and that is the bound working. The
planner says so in as many words — this is the diagnose row, not an inference:

```text
| join, semi-join shape | rows  (columnar refused: a join whose inner side has more keys
                                 than a fragment carries) | — | 1 row(s) | yes |
```

`dim` holds one row per high-cardinality group, so `bucket = 3` selects far more keys than
`esker_columnar::fragment::MAX_IN_VALUES` (4,096). The join is therefore measured with
`--groups-high` **below** the cap, and the number above is a routable join rather than a
representative one. Whether 4,096 is the right cap is a separate question this does not answer.

### 11d. The stale flag that nearly made this a wrong report

The join's numbers in the first two runs of the window read ~3.5–3.9 s **on both arms**, and the run
was green. `workload::Query::columnar_is_possible` was `false` for the join, with a comment giving
the reason: ADR 0040 Decision 4 substitutes one plan shape, so a join has no fragment to be pushed
into. That was true when it was written and the join unit falsified it.

The flag does not force an engine — it only relaxes an assertion — so the failure was silent in the
worst available way: **the join arm ran on rows, and the assertion required rows.** A comparison of
the row engine against itself was reported as a join measurement, and nothing was red.

Two lessons, both already in this file's vocabulary. A record of *why something cannot happen* has
to be re-read when the thing happens; and an assertion that encodes a limitation becomes, the moment
the limitation lifts, a guarantee that it never lifts.

### 11e. Fan-out: still one distinct store, and what that gates

Every successful run reports **1 region, 1 columnar learner, 1 distinct store**, because the only
configurations that load are the ones where the fact table never splits. The spread this window
existed to measure — 5 learners on 2 distinct stores, from §9 — could not be reached at all.

So the exchange question is not merely unanswered, it is **gated on the write path**. An exchange
distributes the merge across the nodes holding the data; until a table can be *loaded* while it
splits, there is no multi-region table to spread anything over, and no honest measurement of what an
exchange would save. That is now the first thing in front of ADR 0022 milestone 5 — ahead of the
shuffle protocol, the operator, and the spill.

One reporting weakness found here and worth fixing: the header counts **cluster** regions while the
fragment line counts the **table's**, so `regions | 2` beside `1 of 1 fragments` reads like a query
answered from half a table. It was not — the second region held `dim`, which never asked for a
columnar copy — but a reader should not have to run a second experiment to learn that.
