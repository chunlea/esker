# Debt wave c3 — measured

## `WalSyncMode::Never` did not disable what it names

`docs/bench/columnar-learner.md` §"And the sharper reading" recorded 4.8 ms per single-row put with
the mode set to `Never`, and could not explain it: a sample put 2075 of 2114 stacks inside
`commit_group` and only 31 in the WAL flush, so it concluded that whatever the write path was
waiting on, "it is not an `fsync`". It was an `fsync`. `WriteOptions::default()` was `sync: true`
and the write path took the **union** of that and the mode, so the mode could only ever add
syncing; `bench_columnar.rs` writes through `Db::put`, which takes the default, so every put
fsynced on a database opened not to. The sample was reading the syscall's frames as its caller's —
`wal.writer.sync()` is called from inside `commit_group`, under the WAL lock.

### The command

Both numbers below are the same command on the same machine, minutes apart, differing only in the
one line of `crates/esker-engine/src/db/write.rs` that decides whether a write waits:

```
cargo test -p esker-store --test bench_columnar ingestion_columnar_against_row \
    -- --ignored --nocapture
```

20,000 single-row applies, one writer, no contention, `WalSyncMode::Never`, macOS on Apple silicon,
`fdatasync` against a local APFS volume, a debug build (`--release` is not what the test gate runs
and the comparison is between two runs of the same build).

### The numbers

| | row apply, 20,000 rows | per row | rows/s |
|---|---|---|---|
| **before** — `sync = options.sync \|\| mode == PerWrite` | **90.37 s** | 4.5 ms | 0.0002 M |
| **after** — `sync = options.wants_sync(mode)` | **170.26 ms** | 8.5 µs | 0.12 M |

**531×**, and the before-number reproduces the recorded one almost exactly: 90.37 s here against
the 95.94 s written down in `columnar-learner.md`, which is the same measurement on a busier box.

The columnar side of the same benchmark did not move — 71.45 ms before, 84.27 ms after, which is
run-to-run noise on a shared machine — because it never went through the engine's write path at
all. That is the point of putting both columns in one table.

### What this does to the ratio nobody should have quoted

`columnar-learner.md` was careful to say its four-figure columnar-to-row ratio "means nothing about
columnar storage and everything about a write path being used a row at a time". It was right to
refuse the number and wrong about which half was at fault: the row side was not paying for
row-at-a-time coordination, it was paying for 20,000 `fdatasync` calls. Corrected, the same
benchmark puts columnar apply at **0.24 M rows/s** against row apply at **0.12 M rows/s** — a
factor of two, on a workload built to be the worst case for the row side, and still not a claim
about either engine's throughput. Neither number is a benchmark of storage; both are what a
one-entry-at-a-time apply costs on each side.

### What did not change

Nothing on the durability path, and this is the assertion that matters most.

* Under the default `WalSyncMode::PerWrite`, a write that expressed no preference still syncs. Every
  caller that took `WriteOptions::default()` before takes the same behaviour now.
* `esker-store` has **no** `WriteOptions::default()` write site: every one of its writes says
  `synced()` or `unsynced()` explicitly, so the store's behaviour is bit-for-bit what it was. The
  store has always opened its engine `Never` and has always done its own syncing.
* `crates/esker-engine/tests/crash_kill.rs` and the rest of the engine suite: **409 tests, 409
  passed, 7 skipped.**
