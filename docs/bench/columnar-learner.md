# Columnar learner — what the apply target costs and what the read buys

`esker-store`'s columnar apply target and `esker-columnar`'s merged read.
[ADR 0022](../adr/0022-columnar-learner-replica.md) milestone 3. Plan:
[`docs/plans/phase-8-learner.md`](../plans/phase-8-learner.md).

Not a gate. `CLAUDE.md` keeps benchmarks runnable and recorded so regressions are visible, and out
of the test gate so nobody tunes before correctness is proven. `#[ignore]`d, run deliberately:

```text
cargo test --release -p esker-store --test bench_columnar \
    -- --ignored --nocapture --test-threads=1
```

`--test-threads=1` is not optional: the three cases each push rows through an engine, and run
concurrently they measure each other rather than their subjects.

## Run 1 — 2026-08-31, 20,000 rows, `id int8` + `name text`

```text
learner fragment scan, 1 of 2 columns              8.69 M rows/s       2.30ms
voter row scan, whole row                          4.38 M rows/s       4.57ms

columnar apply + seal                              0.84 M rows/s      23.91ms
row apply                                          0.00 M rows/s      95.94s

visible scan across 1 run(s)                       5.78 M rows/s       3.46ms
visible scan across 2 run(s)                       3.53 M rows/s       5.67ms
visible scan across 4 run(s)                       3.39 M rows/s       5.89ms
visible scan across 8 run(s)                       3.09 M rows/s       6.47ms
```

## The ingestion number is not a speedup, and must not be quoted as one

95.94 s for 20,000 single-row puts is **4.8 ms a row**, which is not the row engine writing. A
sample of the same benchmark at 200,000 rows put **2075 of 2114 stacks inside
`esker_engine::db::write::DbInner::commit_group` itself** — not the WAL flush (31 stacks), not the
memtable (1). Every single-row `put` forms its own commit group, and with one writer and nothing
to contend with it still pays for the coordination.

So the honest reading is *"this is what a one-entry-at-a-time apply costs on each side today"*,
which is the right question for a Raft apply loop, and **not** a claim about either engine's
throughput. Dividing one by the other produces a four-figure ratio that means nothing about
columnar storage and everything about a write path being used a row at a time.

This lane consumes `esker-engine`'s write path rather than owning it, so the observation is
recorded here rather than acted on. It is the largest single number in this file and the least
informative.

## The scan number, with the half that cuts the other way

**2.0×** for one column of two — 8.69 against 4.38 M rows/s. That is the shape ADR 0022's cost
rule predicts, and the reason it keeps point reads on a row replica.

It is also the flattering half, and [`columnar-m2.md`](columnar-m2.md) already records the other:
reading *every* column is **slower** columnar than row-wise (13.56 against 17.04 M rows/s there).
A projection of one column in two is close to the narrowest table where columnar can win at all;
the win grows with the columns a query does not read. Quoting 2.0× without that sentence would be
the same mistake as quoting m2's 3.5× alone.

## The merged path, which is the number this milestone owed

Resolving MVCC visibility is a property of the **region**, not of a file, so a read merges every
live run before it resolves versions ([`scan::merged`]). One run keeps the borrowed fast path;
several must materialise each row to merge them. Measured rather than asserted:

| runs | rows/s | against one run |
|---|---|---|
| 1 | 5.78 M | — |
| 2 | 3.53 M | 0.61× |
| 4 | 3.39 M | 0.59× |
| 8 | 3.09 M | 0.53× |

**The cost is the first step, not the count.** Going from one run to two costs 39%; going from two
to eight costs a further 12%. That is the borrowed-to-owned transition being the expense, and the
k-way merge itself being cheap — which is what the design predicted, and is worth having measured
because the opposite would have argued for a different structure entirely.

What it says operationally: compaction earns its keep by getting a region **off one run**, and
after that the marginal run matters little. A region that has drifted to eight runs is not in
trouble; a region that never compacts at all still pays only about half.

## What would move these numbers

* **The row-side ingestion figure** wants a batched write path, which is what the store's real
  apply loop has and what this benchmark deliberately does not use — it measures the same
  one-at-a-time shape on both sides so the comparison is like for like.
* **The merged path** materialises through `Value`, which owns its `Text` and `Bytea`. A borrowed
  merged row is possible and is a larger change than this milestone earned.
* **20,000 rows** is small, chosen so the row side finishes. The columnar figures are stable across
  sizes; the row side is linear in the same coordination cost throughout.
