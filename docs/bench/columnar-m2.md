# Columnar — milestone 2 scan numbers, and milestone 1's compression

`esker-columnar`, [ADR 0022](../adr/0022-columnar-learner-replica.md) milestones 1 and 2. Plan:
[`docs/plans/phase-7-columnar.md`](../plans/phase-7-columnar.md).

Not a gate. `CLAUDE.md` keeps benchmarks runnable and recorded so regressions are visible, and
keeps them out of the test gate so nobody tunes before correctness is proven. Both measurements
below are `#[ignore]`d tests, run deliberately:

```text
cargo test --release -p esker-columnar --test bench_scan   -- --ignored --nocapture
cargo test --release -p esker-columnar --test compression  -- --nocapture
```

## Run 1 — 2026-08-31

| Field | Value |
|---|---|
| commit | `92913db` (format version 2) |
| toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)` |
| machine | aarch64-apple-darwin, Darwin 27.0.0 |
| filesystem | `esker_engine::memfs::MemFileSystem` — in memory, so these are **decode** rates, not I/O rates |
| corpus | `crates/esker-columnar/tests/corpus.rs`: a ledger of ascending ids and timestamps, a five-value label, a boolean that runs, amounts in a narrow band, and a payload of random bytes |

Reading these numbers: the filesystem is in memory on purpose. A disk would add I/O to both sides
and *widen* every columnar advantage, because the columnar side reads fewer bytes. What is
measured here is the part that is ours.

## Scan throughput — 200,000 rows, 4 stripes

```text
3 583 184 bytes columnar          9 321 241 bytes row-format (values only)

write            4.14 M rows/s
scan, 6 columns 13.56 M rows/s    24 chunks decoded
scan, 1 column  46.87 M rows/s     4 chunks decoded    3.5x the six-column scan
count(*)        86.90 M rows/s     0 chunks decoded
row-wise sum    17.04 M rows/s    decodes all six columns per row
```

### The projection effect is the whole feature

**3.5×** between the same scan over six columns and over one. That ratio is what ADR 0022's cost
rule is stated in — *"`rows scanned × columns projected` against `rows scanned × columns stored`"*
— and it is the number to quote when somebody asks what columnar buys. It is not 6× because the
six columns are not the same size: the incompressible `payload` column dominates the wide scan and
the `id` column the narrow one.

`count(*)` decodes **no chunks at all**, which is the extreme of the same property. It is not free
— the rows are still walked — and it is not answered from the footer, though it could be: a
fragment whose only aggregate is `count(*)` and which has no filter could be answered by summing
the stripe index. That optimisation is deliberately not taken, because it is a special case in the
evaluator and this milestone has not yet earned one.

### Columnar loses the wide scan, and that is the honest result

**13.56 against 17.04 M rows/s**: reading every column is *slower* columnar than row-wise. Which
is exactly what the cost rule predicts, and why ADR 0022's rule 1 keeps point reads and small
bounded ranges on a row replica. The same rule the other way round gives **2.8×** to columnar for
the one-column aggregate against the row-wise scan that has to decode all six to reach it.

The row baseline is deliberately a *fast* one — the row value format
(`esker_sql::row::encode_row`), decoded per row, with no I/O, no key encoding and no MVCC. It is
better than the real row path, so columnar has to beat a row store that does not exist.

## Pruning

```text
predicate keeps   stripes read   chunks   rows scanned   elapsed
     everything              4        4         200000     4.08ms
          ~half              3        3         134464     2.41ms
       ~a tenth              2        2          68928     1.04ms
   ~a hundredth              1        1           3392    56.29µs
        nothing              0        0              0    625.00ns
```

Time tracks the rows actually scanned rather than the rows in the file, which is what a working
pruner looks like. Note the last row: a predicate no stripe can satisfy costs **625 nanoseconds**
— the footer is already in memory, and the answer comes out of the statistics without a byte being
read.

There is no rate column on purpose. Dividing 200,000 rows by the time of a scan that looked at
3,392 of them would flatter the pruner by giving it credit for work it did not do.

Pruning granularity is the stripe, so the numbers step rather than curve: a predicate keeping 1%
of the rows still reads the whole stripe those rows are in. That is the cost of a 64Ki-row default
stripe and the reason [the plan](../plans/phase-7-columnar.md) records both a row budget and a
byte budget for sealing one.

## Compression — milestone 1's table, which owed itself a home

50,000 rows of the same corpus, values only. Two handicaps keep it honest: the row baseline counts
no keys, though every row in an SST carries one, and the corpus contains a column of random bytes
that nothing can compress and that puts a hard floor under every ratio.

```text
raw rows            2 329 831 bytes
lz4 rows (4 KiB)    1 643 314          1.42x
columnar, plain     1 020 068          2.28x vs raw   (the encodings alone)
columnar, lz4         896 176          2.60x vs raw, 1.83x vs lz4 rows
```

ADR 0022 assumed **3×** against what Esker already stores. The measured answer is **1.83×**, and
the ADR's disk arithmetic should be read with 1.8–2.6× rather than 3×. Its conclusion survives the
correction — one columnar learner is still single-digit to low-double-digit percent of a
three-replica total — but the number should not be quoted without it.

## What would move these numbers

* **A real filesystem**, which adds I/O to both sides and widens every columnar advantage.
* **Vectorised evaluation.** The scan decodes a chunk into owned buffers and then walks rows one
  at a time through a `ValueRef` enum. A filter evaluated over a whole column at once is the
  standard next step and is milestone 2's declared non-goal — `CLAUDE.md` forbids tuning before a
  profile, and this file is the profile.
* **A smaller stripe**, which sharpens pruning and coarsens compression. Both budgets exist; no
  workload has yet asked for a different default.
