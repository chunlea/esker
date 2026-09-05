# MPP baseline — where a distributed aggregate's time goes today

[ADR 0022](../adr/0022-columnar-learner-replica.md) **milestone 5** is the one milestone that ADR
gates on a number: MPP exchange is built *"last, and only if measured"*. This file is that number,
and [`docs/plans/phase-16-mpp.md`](../plans/phase-16-mpp.md) §10 is the verdict written from it.

Not a gate. `CLAUDE.md` keeps benchmarks runnable and recorded so regressions are visible, and out
of the test gate so nobody tunes before correctness is proven.

> **Status: method fixed, no numbers yet.** The driver is built, gated and committed; the run is
> blocked on a defect outside this lane — see [§6](#6-why-run-1-is-empty). This file records the
> method *before* the numbers deliberately: a measurement whose method is written afterwards is a
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
