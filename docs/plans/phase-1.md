# Phase 1 plan — the storage engine (`esker-engine`)

Status: **in progress**. Written before implementation; §9 records progress and §10 what changed.
Spec: `prompts/01-engine.md`. Constitution: `CLAUDE.md`. Design: `docs/DESIGN.md` §4, §11, §13, §14.

## 1. Scope

Build the LSM engine of `docs/DESIGN.md` §4 bottom-up: WAL, write batch, internal keys, memtable,
SST, manifest/versions, `Db` (open/write/read/iterate/snapshot/flush), compaction, checkpoint and
ingest, plus the `esker-cli` tools that make each format inspectable. Everything is byte-opaque:
no key semantics enter this crate (invariant 7).

Two lanes build it in parallel. **SST and the block cache are leaves** — they depend on nothing
above them, only on byte-level contracts — so the prompt's "steps in order" is honoured *within*
the spine.

| Lane | Owns | Steps |
|---|---|---|
| `wy-p1-spine` | everything in `crates/esker-engine/**` except `src/sst/**` and `src/cache/**`; `crates/esker-cli/**`; this plan; `docs/bench/phase-1.md` | 1, 2, 3, 5, 6, 7, 8, 9 |
| `cl-p1-sst` | `src/sst/**`, `src/cache/**` | 4 |
| `p1-test` (later) | model test, crash loop, concurrency, coverage | required tests |

The spine owns the **shared contract** (`fs.rs`, `dbformat.rs`, `cache_api.rs`, `options.rs`,
`error.rs`, `lib.rs`) and lands it in its first code commit so the SST lane can code against it.
After that commit the spine never touches `src/sst/**` or `src/cache/**`, and the SST lane never
touches anything else.

## 2. File list

```
crates/esker-engine/Cargo.toml         deps for both lanes, added once, up front
crates/esker-engine/src/lib.rs         module tree, crate docs, format constants (contract)
crates/esker-engine/src/error.rs       Error/Result for the whole crate                (contract)
crates/esker-engine/src/fs.rs          FileSystem/WritableFile/RandomAccessFile + local (contract)
crates/esker-engine/src/dbformat.rs    Comparator, EntryKind, internal key, SeqNo      (contract)
crates/esker-engine/src/cache_api.rs   CacheKey + BlockCache trait                     (contract)
crates/esker-engine/src/options.rs     Options/CfOptions/Read/WriteOptions, prefix extractor,
                                       compression                                     (contract)
crates/esker-engine/src/wal/{mod,format,writer,reader}.rs        step 1
crates/esker-engine/src/batch.rs                                 step 2
crates/esker-engine/src/memtable.rs                              step 3
crates/esker-engine/src/sst/**                                   step 4  — SST lane
crates/esker-engine/src/cache/**                                 step 4  — SST lane
crates/esker-engine/src/version/{mod,edit,set,builder}.rs        step 5
crates/esker-engine/src/db/{mod,open,write,read,iter}.rs         step 6
crates/esker-engine/src/compaction/{mod,picker,job,scheduler}.rs step 7
crates/esker-engine/src/db/{checkpoint,ingest}.rs                step 8
crates/esker-engine/tests/**                                     golden + crash tests per step
crates/esker-cli/src/**                                          step 9
docs/bench/phase-1.md                                            step 9 numbers
```

## 3. The shared contract

Pinned in the first code commit; shapes are binding, and a change to any of them is a message to
the coordinator, never a unilateral edit.

```rust
// fs.rs — every file touch goes through this (DESIGN §13): local now, S3 later, and a
// fault-injecting implementation for the crash tests.
pub trait FileSystem: Send + Sync {
    fn create(&self, path: &Path) -> Result<Box<dyn WritableFile>>;
    fn open(&self, path: &Path) -> Result<Box<dyn RandomAccessFile>>;
    fn list(&self, dir: &Path) -> Result<Vec<PathBuf>>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;    // atomic replace
    fn delete(&self, path: &Path) -> Result<()>;
    fn fsync_dir(&self, dir: &Path) -> Result<()>;
    fn size(&self, path: &Path) -> Result<u64>;
    fn exists(&self, path: &Path) -> Result<bool>;
}
pub trait WritableFile: Send  { fn append(&mut self, &[u8]) -> Result<()>; fn sync_data(&mut self) -> Result<()>; }
pub trait RandomAccessFile: Send + Sync { fn read_at(&self, u64, &mut [u8]) -> Result<usize>; fn size(&self) -> Result<u64>; }

// dbformat.rs
pub trait Comparator: Send + Sync + Debug { fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering; fn name(&self) -> &'static str; }
pub struct BytewiseComparator;            // memcmp, the default
pub enum EntryKind { Delete = 0, Put = 1, DeleteRange = 2 }
pub struct InternalKeyComparator(Arc<dyn Comparator>);   // user asc, then tag DESC

// cache_api.rs — the SST lane implements this in src/cache/lru.rs; the spine injects it.
pub struct CacheKey { pub file_number: u64, pub offset: u64 }
pub trait BlockCache: Send + Sync {
    fn insert(&self, key: CacheKey, block: Arc<[u8]>, charge: usize);
    fn lookup(&self, key: &CacheKey) -> Option<Arc<[u8]>>;
}

// options.rs — what TableOptions is built from.
pub enum Compression { None, Lz4 }
pub trait PrefixExtractor: Send + Sync + Debug { fn prefix<'a>(&self, key: &'a [u8]) -> &'a [u8]; fn in_domain(&self, key: &[u8]) -> bool; fn name(&self) -> &'static str; }

// Delivered by the SST lane, consumed by the spine from step 6:
// sst::TableBuilder::new(TableOptions, Box<dyn WritableFile>) / add(key, value) sorted / finish() -> TableProperties
// sst::TableReader::open(Box<dyn RandomAccessFile>, file_number, TableOptions, Option<Arc<dyn BlockCache>>)
//                  / get / iter (seek, seek_for_prev, next, prev)
```

## 4. Formats frozen in this phase

| Format | Layout |
|---|---|
| WAL block | 32 KiB; record = `crc32c:u32 ++ len:u16 ++ type:u8 ++ payload`, all little-endian; type ∈ {FULL 1, FIRST 2, MIDDLE 3, LAST 4}; the CRC is seeded with the type byte and covers type ++ payload; a tail shorter than 7 bytes is zero-filled |
| WriteBatch | `seqno:u64 ++ count:u32` (fixed LE, 12-byte header) then `count` entries `cf:varint ++ kind:u8 ++ key:varint-len ++ value:varint-len`; no value for `Delete`, `value = end_key` for `DeleteRange` |
| Internal key | `user_key ++ tag:u64` little-endian, `tag = (seqno << 8) \| kind`; order = user key ascending by the user comparator, then tag **descending** (LevelDB exactly) |
| MANIFEST | a WAL-format log of `VersionEdit` records; `CURRENT` is `temp → fsync → rename → fsync_dir` |
| SST | DESIGN §4.5 — the SST lane's |

Every one of them gets a golden file under `crates/esker-engine/tests/golden/`.

## 5. Public API sketch (spine)

```rust
pub struct Db;
impl Db {
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Db>;
    pub fn open_with_fs(path, Options, Arc<dyn FileSystem>) -> Result<Db>;   // crash tests inject faults
    pub fn write(&self, batch: WriteBatch, opts: &WriteOptions) -> Result<SeqNo>;
    pub fn get(&self, cf: &str, key: &[u8], opts: &ReadOptions) -> Result<Option<Bytes>>;
    pub fn iter(&self, cf: &str, opts: &ReadOptions) -> Result<DbIterator<'_>>;
    pub fn snapshot(&self) -> Snapshot;
    pub fn create_cf(&self, name: &str, opts: CfOptions) -> Result<()>;
    pub fn drop_cf(&self, name: &str) -> Result<()>;
    pub fn flush(&self, cf: &str) -> Result<()>;
    pub fn compact_range(&self, cf: &str, range: Range<Option<&[u8]>>) -> Result<()>;
    pub fn checkpoint(&self, dir: impl AsRef<Path>) -> Result<()>;
    pub fn ingest(&self, cf: &str, paths: &[PathBuf]) -> Result<()>;
    pub fn property(&self, name: &str) -> Option<String>;   // stall state, L0 count, pending bytes
}
pub struct WriteBatch;   // put / delete / delete_range / put_cf … ; count, byte size
pub struct Snapshot;     // pins a seqno, released on drop
pub trait Iterator { fn seek(&mut self, &[u8]); fn seek_for_prev(&mut self, &[u8]); fn next(&mut self);
                     fn prev(&mut self); fn key(&self) -> &[u8]; fn value(&self) -> &[u8]; fn valid(&self) -> bool; }
```

## 6. Test list

| Step | Tests |
|---|---|
| WAL | golden file of a known log; proptest over random record sizes that cross block boundaries; truncation at **every** byte offset of a 3-record log recovers exactly the complete records and reports `Torn`; a bit flip at **every** byte offset is reported `Corrupt`; a header copied to another offset does not replay (the CRC type seed) |
| batch/dbformat | golden bytes for both layouts; proptest round trip; proptest that internal-key order equals (user asc, seqno desc, kind desc); malformed batches are errors, never panics |
| memtable | ordering and overwrite semantics; iterator seek/seek_for_prev/next/prev against a `BTreeMap` model; approximate size grows monotonically; snapshot reads by seqno |
| version | `VersionEdit` round trip + golden; reopen after a kill between manifest append and `CURRENT` rename **in every order** sees old or new, never neither; obsolete files survive while a `Version` pins them |
| db | recovery from a torn tail in the last segment only; corruption elsewhere fails under `paranoid`; group commit: a `sync=false` batch riding a sync group is durable; `wal_sync_mode` honoured; CF create/drop across reopen; cross-CF atomicity; `prefix_same_as_start`; snapshot isolation |
| compaction | `CompactionJob` as a pure function on synthetic inputs; tombstones dropped only at the bottom level or with no older overlap; picker scores; stall metrics visible |
| checkpoint/ingest | checkpoint reopens independently and is unaffected by later writes; ingest assigns file numbers and survives reopen |
| cross-cutting | the model test, the crash loop and the concurrency test of `prompts/01-engine.md` (test lane) |

Fault injection is a `FileSystem` implementation (partial write, failed fsync, failed rename,
short read) so every layer above can be crashed without a subprocess where a subprocess is not
needed; the kill -9 loop still runs for real durability.

## 7. Risks

1. **Torn vs corrupt.** A torn record is legal only at the tail of the *last* segment. Getting it
   backwards silently drops acknowledged writes, so the reader reports the two distinctly and the
   caller — not the reader — decides which is tolerable, per segment.
2. **macOS durability.** `File::sync_data` is `fdatasync`-like; real power-loss durability on
   macOS needs `F_FULLFSYNC`. v1 targets process-crash (kill -9) durability. Documented in
   `fs.rs`, with `F_FULLFSYNC` left as a named knob (`TODO`), never silently assumed.
3. **Group-commit lock discipline.** The writer-queue lock is never held across the fsync, and a
   follower observes its seqno only after the leader publishes it. A `sync=false` batch that joins
   a sync group gets durability for free; that is correct.
4. **Recovery seqno.** The high-water seqno must come from *both* the manifest and the replayed
   WAL: a flush that raced the crash can leave either one ahead.
5. **Skiplist keys.** `crossbeam-skiplist` orders by `Ord`, so the key wrapper must delegate to
   the internal comparator; entries are never removed (a memtable dies whole), because removal
   would break iterator snapshot assumptions.
6. **Manifest replay order.** `VersionEdit`s apply strictly in log order, and a partially written
   final edit is a torn tail under the same rule as the WAL.
7. **Contract drift with the SST lane.** The contract is committed before either lane builds on
   it; a mismatch is reported to the coordinator and stubbed behind `// TODO(sibling)`, never
   fixed by editing the other lane's files.
8. **`unsafe`.** Warn-level in this crate, but every site still needs `// SAFETY:` and a test.
   We expect to need none in the spine.

## 8. Non-goals for this phase

- No range-tombstone machinery (`DeleteRange` keeps the v1 limitation of DESIGN §4.7).
- No compression codec other than `lz4_flex`; no dictionary compression.
- No in-house skiplist (`crossbeam-skiplist` stays until it is measured to matter).
- No S3 `FileSystem`, no TLS, no tiering (phase 6b).
- No performance tuning before the correctness tests are green; benchmarks are recorded, not chased.
- No `esker-store`/Raft integration (phase 2+); the engine stays byte-opaque.

## 9. Progress

- [x] step 0 — this plan
- [x] step 0b — contract commit (`fs.rs`, `dbformat.rs`, `cache_api.rs`, `options.rs`, `error.rs`,
      `lib.rs`)
- [x] step 1 — WAL (`wal/{format,writer,reader}.rs`, `memfs.rs`, golden + exhaustive damage tests)
- [x] step 2 — WriteBatch + internal key (`batch.rs`, goldens for both layouts)
- [x] step 3 — memtable (`memtable.rs`, model test + concurrency test)
- [x] step 4 — SST + block cache *(SST lane, accepted at the coordinator's gate)*
- [x] step 5 — manifest / `VersionSet` (`filename.rs`, `version/{edit,builder,set}.rs`,
      the full `CURRENT`-swap crash matrix)
- [x] step 6 — `Db` — open/recovery/group commit/snapshots (6a), memtable switch, background
      flush to L0 and level reads (6b), the merge cursor and `DbIterator` (6c), runtime
      `create_cf`/`drop_cf` (6d)
- [ ] step 7 — compaction *(next)*
- [ ] step 8 — checkpoint + ingest
- [ ] step 9 — `esker-cli` tools + bench numbers

## 10. Changes vs plan

1. **`fs.rs` returns `io::Result`, not the crate's `Result`.** An implementation of the
   filesystem seam then owes nothing to the engine's error type; call sites attach the path
   with `IoResultExt::at`. §3 of this plan originally sketched it the other way.
2. **`Comparator`, `FileSystem`, `BlockCache` and `PrefixExtractor` require `Debug`.** Without
   it nothing holding one can derive `Debug`, which the workspace's
   `missing_debug_implementations` lint requires of every public type. `WritableFile` and
   `RandomAccessFile` deliberately do *not*, because the SST lane implements them; `LogWriter`
   and `LogReader` write their `Debug` out by hand instead.
3. **`Comparator` gained two provided methods**, `find_shortest_separator` and
   `find_short_successor`, for SST index separators. They default to doing nothing, which is
   always correct, so a comparator may ignore them.
4. **`WriteOptions::sync` defaults to `true`**, unlike LevelDB and RocksDB. `CLAUDE.md`
   invariant 1 makes the un-durable acknowledgement the thing a caller opts into.
5. **The WAL omits LevelDB's CRC rotation mask.** It exists there because the checksum covers
   bytes that can contain checksums; ours covers the type byte and the payload only.
6. **A new `Error::Poisoned`.** When a manifest sync or a `CURRENT` rename fails, we cannot
   tell whether the bytes landed, so the version set refuses to continue rather than carry an
   in-memory version the disk may not share. Reopening re-derives the truth.
7. **`Error::GroupCommit`** carries a leader's failure to the followers that shared its group.
8. **`memfs.rs` is a normal module, not test-only.** The simulator will want an in-memory
   filesystem in a normal build; only deliberate misbehaviour belongs behind a feature, which
   is where the SST lane put its `testing::FaultFileSystem`.
9. **The `Cursor` trait lives in `src/iterator.rs`**, not in `dbformat.rs`: it is a shape, not
   a format. `MemTableIter` implements it; `sst::TableIter` is adapted by a wrapper in
   `db/iter.rs` until the SST lane implements it directly.
10. **The memtable cursor owns its table** and navigates by key rather than holding a
    `crossbeam-skiplist` entry, which borrows the map. The alternative was a self-referential
    struct — a lifetime threaded through every caller, an `unsafe` lifetime extension, or a
    banned crate. It costs `O(log n)` per step and a copy of the entry; `CLAUDE.md` says to
    prefer safe code and optimise after a profile, and the in-house skiplist removes the cost.
11. **`Db` is not `Clone`.** It owns the background flush thread and stops it on drop; share it
    with an `Arc`, as `LevelDB` and `RocksDB` are shared.
12. **Three bugs the tests found, recorded because each was silent.**
    *A failed WAL append now ends the segment* — writing after a partial write laid the next
    group over a half-written record, turning a torn tail into mid-log corruption and losing
    every acknowledged write past it (found by the crash lane's injector).
    *Every manifest edit stamps the sequence number* — once a flush deletes the segments behind
    it, the manifest is the only record of how far numbering got, and an edit that forgot it
    restarted numbering and made flushed writes invisible.
    *Shutdown sets its flag under the lock the background thread waits on* — outside it, the
    wake-up is lost in the window between the thread's check and its wait, and the join in
    `Drop` hangs.
13. **`Error` gained `GroupCommit` and `Poisoned`.** A failed group commit belongs to every
    writer in it; a failed manifest write means memory and disk may disagree, and the honest
    answer is to stop rather than guess.
