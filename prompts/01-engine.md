# Phase 1 — The storage engine (`esker-engine`)

Build the LSM engine described in `docs/DESIGN.md` §4, bottom-up, in the order below. Each step is a
vertical slice: format + writer + reader + tests + commit. Do not start the next step until the current
one's crash test passes. Write `docs/plans/phase-1.md` first (file list, public types, test list, risks).

This is the phase where you re-derive LevelDB. When in doubt about a mechanism, prefer LevelDB's choice
and note it; where DESIGN.md departs from LevelDB (column families, prefix bloom, multi-threaded
compaction, group commit with sync modes), DESIGN.md wins.

## Steps

1. **WAL** (`wal/{format,writer,reader}.rs`): 32 KiB blocks, 7-byte header, FULL/FIRST/MIDDLE/LAST,
   the in-house CRC32C from phase 0 seeded with the type. Writer buffers in user space and issues one `write` per group; `sync()`
   calls `fdatasync`. Reader yields records and reports `Torn` (tail) vs `Corrupt` (elsewhere) distinctly.
   Tests: proptest over random record sizes crossing block boundaries; golden file; torn-tail truncation
   at every byte offset of a 3-record log recovers exactly the complete records; bit-flip at every byte
   is detected.
2. **WriteBatch + internal key** (`batch.rs`, `dbformat.rs`): serialization from DESIGN.md §4.3; internal
   key `user_key ++ seqno:u56 ++ kind:u8`; the internal comparator (user order, then seqno descending).
3. **Memtable** (`memtable.rs`): skiplist over internal keys, arena-free (rely on `crossbeam-skiplist`),
   approximate size accounting, iterator with seek/prev.
4. **SST** (`sst/{block,builder,reader,filter,footer}.rs`): block builder with restart points and prefix
   compression; index block; bloom filter with optional prefix extractor; properties; 48-byte footer with
   magic; per-block `compression ++ crc32c`; lz4 default (`lz4_flex`, the only codec). Reader: footer →
   index → block, with a block cache trait injected; the sharded LRU cache is written in-house
   (`cache/lru.rs`, intrusive doubly-linked list + hash map, no `lru` crate). Tests: golden SST; proptest "build from sorted map, read back every key and every
   range"; corrupt-any-byte detection; bloom false-positive rate measured ≤ 2% at 10 bits/key.
5. **Manifest / VersionSet** (`version/{edit,set,builder}.rs`): VersionEdit log in WAL format,
   `CURRENT` via temp + fsync + rename, `Version` with per-CF per-level file lists behind `Arc`,
   obsolete-file GC that respects pinned versions. Crash test: kill between manifest append and CURRENT
   rename in every order; reopen must always succeed and see either the old or the new version.
6. **Db: open / write / get / iter / snapshot** (`db/{mod,open,write,read,iter}.rs`): recovery from
   WAL, group commit (`WriteOptions::sync`, `wal_sync_mode`), memtable switch + flush to L0 on a
   background thread, merge iterator across memtables and levels, snapshot by seqno,
   `prefix_same_as_start`. Column families: shared WAL/seqno, per-CF memtables/levels/options, atomic
   cross-CF batches, `create_cf`/`drop_cf` as manifest edits.
7. **Compaction** (`compaction/{picker,job,scheduler}.rs`): leveled picker by score, L0→L1 overlap
   handling, `CompactionJob` as a pure function (inputs: iterators + output file writer + filter), a bounded
   thread pool, stall/slowdown states exposed as metrics, `CompactionFilter` trait. Tombstone dropping
   only at the bottom level or when no older overlapping data exists.
8. **Checkpoint + ingest** (`db/checkpoint.rs`, `db/ingest.rs`): hard-link SSTs of a CF/range plus a
   trimmed manifest; ingest external SSTs by assigning file numbers and a manifest edit (needed by
   phase 4 snapshots).
9. **`esker-cli`**: `sst-dump`, `wal-dump`, `manifest-dump`, and `bench` (fillseq, fillrandom,
   readrandom, readseq, overwrite; value size, batch size, sync flags; prints ops/s, MB/s, p50/p99).

## Required tests (beyond the per-step ones)

- **Model test:** proptest of random `put/delete/get/scan/snapshot/flush/compact` sequences against a
  `BTreeMap<(cf, key), value>` model with per-snapshot views; 10,000 cases in CI, more with `--ignored`.
- **Crash loop:** a test binary that spawns the engine in a child process writing a known pattern with
  `sync = true`, kills it with SIGKILL at a random moment, reopens, and verifies every acknowledged
  write (the child reports acks over a pipe) is present and nothing torn is visible. Run 200 iterations
  in CI, with fault injection into `FileSystem` (partial writes, failed fsync, failed rename).
- **Concurrency:** 8 writer threads + 8 reader threads + flush/compaction for 30 s; then model check.

## Acceptance

All tests above pass; `esker-cli bench fillrandom --value-size 100 --num 1000000` completes and the
number is recorded in `docs/bench/phase-1.md` together with `readrandom` and `fillseq --sync`; the crash
loop has run 1,000 iterations locally without a failure; DESIGN.md §4 matches the code; every on-disk
format has a golden file. Report the crate's line count split by module.
