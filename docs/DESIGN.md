# Esker design

Status: living document. Every phase updates the sections it touches. Numbers marked *default* are
configuration knobs with the stated default; numbers marked *fixed* are part of a format.

## 1. Goals and non-goals

Goals: a from-scratch, crash-safe, linearizable, horizontally scalable ordered KV with MVCC and
distributed transactions, designed so that a stateless SQL layer and object-storage tiering can be added
without changing the lower layers.

Non-goals for v1: multi-region geo-replication, encryption at rest, online schema change, pessimistic
locks, secondary-index-aware engine features, anything that needs a wall clock for correctness.

## 2. Request path

```
client ──RPC (esker-proto)──▶ store (leader of region R)
                   │  1. check region epoch, leadership, key range
                   │  2. propose to esker-raft (RawNode::propose)
                   │  3. Ready: append entries to raft log (engine CF `raft`, fsync), send AppendEntries
                   │  4. quorum acks → committed → apply loop
                   │  5. apply: WriteBatch{data CFs + apply_index} → esker-engine (WAL → memtable)
                   ▼  6. respond
                 followers: same steps 3–5 without 1, 2, 6
PD: heartbeats from stores/regions → routing table + scheduling; TSO for esker-txn
```

Reads on the leader use ReadIndex (or lease reads once leases are implemented); followers may serve
stale reads only when the request explicitly allows it.

## 3. Key space and encoding (`esker-keys`)

The engine, Raft, store and PD treat keys as opaque byte strings ordered by `memcmp`. Semantics live in
`esker-keys`, which provides a **memcomparable codec**: encoded byte order equals logical order.

- `u64`/`i64`: 8 bytes big-endian, sign bit flipped for signed.
- `bytes`/strings: TiDB-style group encoding — 8-byte groups, each followed by a marker byte
  `0xFF - pad_count`; the last group is padded with zeros. Guarantees prefix-free ordering
  (`"ab" < "abc"`, and `"ab"` is not a byte-prefix of `"abc"`'s encoding).
- Tuples: concatenation of encoded fields.

Reserved layout (all keys in a cluster):

```
'r' ++ user_key                       RawKV namespace (byte-opaque, no MVCC)
'x' ++ enc(user_key) ++ enc_ts        TxnKV namespace (Percolator; see §8)
't' ++ tenant:u64 ++ table_id:u64 ++ 'r' ++ row_id      SQL rows (phase 6)
't' ++ tenant:u64 ++ table_id:u64 ++ 'i' ++ index_id ++ cols [++ row_id]   SQL indexes
'm' ++ ...                            cluster / catalog metadata
```

`enc_ts` = `!(ts as u64)` big-endian (bitwise NOT), so newer versions sort first within a user key.
The engine's prefix extractor for versioned CFs is "strip the last 8 bytes".

A versioned key's user part is **group-encoded first** (`enc` above is `encode_bytes`), because
appending a fixed suffix to a raw key only keeps one key's versions contiguous when keys are
prefix-free. They are in the SQL layouts and are not in `TxnKV`: raw, `"a"`'s versions interleave
with `"ab"`'s, and the prefix check that should catch it passes, because `"a"` *is* a prefix of
`"ab"`. The unversioned `'r'` namespace has no suffix and so needs no encoding.
[ADR 0015](adr/0015-txn-record-encodings.md) works the bytes through.

## 4. Storage engine (`esker-engine`)

LevelDB's skeleton with RocksDB's three features we need: column families, prefix bloom filters, and
multi-threaded compaction. Byte-opaque; ordered by a pluggable `Comparator` (default `memcmp`).

### 4.1 Public API (sketch)

```rust
pub struct Db;                       // one directory, N column families, one WAL
pub struct WriteBatch;               // ordered list of (cf, Put|Delete|DeleteRange)
pub struct Snapshot(SeqNo);          // pins a seqno; iterators/gets see <= seqno
pub trait Iterator { seek, seek_for_prev, next, prev, key, value, valid }

impl Db {
  fn open(path, Options) -> Result<Db>;
  fn write(&self, batch: WriteBatch, WriteOptions { sync: bool }) -> Result<SeqNo>;
  fn get(&self, cf, key, ReadOptions { snapshot, fill_cache }) -> Result<Option<Bytes>>;
  fn iter(&self, cf, ReadOptions) -> Result<impl Iterator>;   // prefix_same_as_start option
  fn snapshot(&self) -> Snapshot;
  fn create_cf(&self, name, CfOptions) -> Result<u32> / drop_cf(&self, name) -> Result<()>;
  fn checkpoint(&self, dir, Option<(cf, range)>) -> Result<()>;   // hard-links SSTs, writes a manifest
  fn ingest(&self, cf, sst_paths) -> Result<()>;                   // phase 4 snapshots, phase 6 bulk load
  fn flush(&self, cf) / compact_range(&self, cf, range) / property(&self, name)
}
```

**`WriteOptions::sync` defaults to `true`**, which is the deliberate inverse of LevelDB's and
RocksDB's default. Invariant 1 says a write is acknowledged only once its bytes are durable
"unless the caller explicitly passed `sync = false`", so the un-durable acknowledgement is the
thing a caller opts into rather than the thing they have to know to opt out of. A `Db` used
without reading its documentation is therefore slow and correct rather than fast and lossy.

A `Snapshot` belongs to the `Db` instance that issued it and is refused by any other: sequence
numbers survive a reopen, so a stale handle names a plausible number, but the reopened
database's snapshot list has never heard of it and the compaction floor can pass it by.

### 4.2 Write path

`write()` → assign seqno → group commit → WAL append (+ fsync if `sync`) → insert into the active
memtable of each touched CF → return. Group commit: the first writer to take the write lock becomes
leader, drains the queue (bounded by 1 MiB or 128 batches, *default*), writes one WAL record group,
syncs once, then wakes everyone. Sync mode per write; `Options::wal_sync_mode = {PerWrite, Interval(ms), Never}`.

Two rules the implementation is not free to relax. The queue lock is **never held across the
`fsync`**, or every arriving writer serialises behind a disk flush and group commit becomes a
queue with extra steps. And a follower learns its sequence number **only after the leader has
published**, so no batch is observable at a sequence number before it is readable.

The leader does the log write for everyone, so **its failure is everyone's**: each batch in the
group is refused with `Error::GroupCommit` carrying the leader's message, because reporting
success to a writer whose bytes never reached the log would break invariant 1 for a write that
looked fine. A `sync = false` batch that rides a synced group gets durability for free, which
is correct — `sync = false` is permission to acknowledge early, never a requirement to.

A failed *append* ends the log segment for good. A partial write and a full disk are
indistinguishable from inside `write(2)`, so after one the segment's length is unknown: writing
on would lay the next record over the tail of a half-written one, leaving a valid header,
plausible bytes and a failing checksum — corruption in the middle of a log rather than a torn
record at its end, which is the difference between a database that reopens and one that does
not. Every later write on that segment fails until a memtable rotation opens a fresh one.

### 4.3 WAL format (*fixed*)

LevelDB's block format, all integers little-endian: 32 KiB blocks; record header =
`crc32c:u32 ++ len:u16 ++ type:u8` (7 bytes); types `FULL=1/FIRST=2/MIDDLE=3/LAST=4` (`0` is reserved and
never valid, so an all-zero header is not an empty record); a block trailer smaller than 7 bytes is
zero-filled. One WAL segment per memtable generation, named `NNNNNN.wal`. The CRC is seeded with the
record type (as LevelDB does) so a header from another position cannot be replayed; it covers the type
byte and the payload and never the checksum field, so LevelDB's rotation *mask* is not applied.

Payload of one record = one serialized `WriteBatch`: a 12-byte header `seqno:u64 ++ count:u32` followed by
`count` entries, entry = `cf:varint ++ kind:u8 ++ key:varint-prefixed [++ value:varint-prefixed]`.
`kind` is `Delete=0 / Put=1 / DeleteRange=2`; `Delete` stores no value and `DeleteRange`'s value is the
exclusive end of the range. Entry `i` of a batch is stored at sequence number `seqno + i`, so a batch of
`n` entries consumes `n` of them — that is what makes two writes to the same key inside one batch
ordered rather than ambiguous.

Recovery: read segments in order, verify every record, apply into fresh memtables, stop at the first torn
record of the **last** segment (normal), fail on corruption anywhere else unless `Options::paranoid = false`.

### 4.4 Memtable

`crossbeam-skiplist` (the one bought piece of concurrent code; an in-house arena skiplist is a
post-v1 replacement behind the same `MemTable` trait) keyed by internal key
`user_key ++ tag:u64` little-endian with `tag = (seqno << 8) | kind` (LevelDB layout, so the kind byte
physically precedes the 56-bit sequence number), ordered by user key ascending then tag **descending** so
the newest version of a key sorts first. One
active + a bounded queue of immutable memtables per CF. Flush when active reaches
`write_buffer_size` (64 MiB *default*). Stall policy: slow down at `max_immutable = 2`, stop at 4 — and
expose both as metrics so the stall is visible, never mysterious.

### 4.5 SST format (*fixed*, version 1)

Block-based table: data blocks (4 KiB *default*) with prefix-compressed entries and restart points every
16 entries; an index block (one entry per data block: separator key → block handle); a filter block
(bloom, 10 bits/key *default*, built over the prefix-extracted key when the CF has a prefix extractor);
a properties block (entry count, raw sizes, smallest/largest key, creation seqno range, format version);
a footer of 48 bytes *fixed*: index handle, filter handle, properties handle, format version, magic
`0x45534B455253535431` ("ESKERSST1"). Every block ends with `compression_type:u8 ++ crc32c:u32`.
Compression per block: none / lz4 (*default*, via `lz4_flex`, the only compression codec — no zstd).
CRC32C is implemented in-house (`esker-engine::crc32c`: slicing-by-8 table, `std::arch` hardware
instruction behind `cfg(target_feature)`), with a golden test against known vectors.

### 4.6 Manifest and versions

`MANIFEST-NNNNNN` is a WAL-format log of `VersionEdit`s (add/delete file per level per CF, log number,
next file number, last seqno, comparator name, CF create/drop). `CURRENT` names the active manifest and is
replaced by write-temp + fsync + rename. `VersionSet` keeps the live `Version` (per CF, per level: sorted
file metadata) behind an `Arc`; readers pin a `Version`, compaction installs a new one. Obsolete files are
deleted only after no `Version` references them — and a file being *written* counts as
referenced, or a sweep on one thread deletes an output another is still producing.

The order in `log_and_apply` cannot be rearranged: build the new version (so an edit that
cannot be applied is never logged), append it to the manifest and sync, replace `CURRENT` if
this is a new manifest, and only then install the version in memory. A crash between any two
steps leaves the database readable, because a rename either happens or does not.

An **error** between them is the harder case, and the answer is `Error::Poisoned`: after a
failed manifest sync or a failed `CURRENT` rename we cannot tell whether the bytes landed, so
the version set refuses every later edit rather than carrying an in-memory version the disk may
not share. Reopening re-derives the truth from what actually reached the disk. Every edit also
carries the sequence number reached so far, because once a flush lets the log segments behind
it be deleted the manifest is the only remaining record of how far numbering got.

### 4.7 Compaction

Leveled. L0 flush trigger 4 files, slowdown 8, stop 12 (*default*). L1 base 64 MiB, ×10 per level, 7
levels (*default*). Score-based picking; L0→L1 merges all overlapping L0 files. Compaction runs on a
bounded thread pool (2 threads *default*) via a `CompactionJob` that is a pure function of its inputs (so it
is unit-testable without the `Db`). `CompactionFilter` trait lets `esker-txn` drop MVCC versions below
the safepoint.

**Range deletions.** `DeleteRange` is a *format* in v1 and not a feature. The entry kind exists in
the `WriteBatch` and log layouts (§4.3) so that making range deletes real in phase 5 is not a
format change — but no read path honours it: the memtable, `get` and both iterators treat it as a
point `Delete` at the range's `begin`. So **the engine refuses it**: `Db::write` returns
`Error::Unsupported` for any batch containing one, before the batch is logged, and a refused batch
changes nothing. Storing it instead would delete one key while telling the caller a range was
gone, which is the failure mode this rule exists to prevent — and which phase 2 found from the far
side of a socket, where `esker-store` had to answer for it.

Until phase 5, a **ranged delete is the store's job**: `RawKv DeleteRange` is served as a bounded
scan plus point deletes in one atomic batch (ADR 0006). Real range tombstones land in phase 5 with
`esker-txn`, and remove both the store's workaround and this refusal.

### 4.8 Column families

Shared WAL and seqno space; separate memtables, levels, options (prefix extractor, block size,
compression, filter). `WriteBatch` across CFs is atomic.

The engine creates no column family of its own: `Db::open` opens the ones the caller names,
creating any that are missing, and also opens any the database already holds — hiding data a
database contains is worse than opening more than was asked for. `create_cf` and `drop_cf` work
on an open database; both are manifest edits, made durable before memory changes, and a drop is
followed by file deletion. The names `default`, `lock`, `write` (Percolator, §8) and `raft`
(Raft logs and region metadata, §6) are constants in `esker_engine::cf` — the set that
`esker-store` creates at bootstrap, since phase 2, not something this layer imposes. All four are
created by the first `Store::open` even though phase 2 writes only to `default`: creating the other
three when `esker-txn` and `esker-raft` arrive would make every database built before then a
migration.

### 4.9 Read path

Get: memtable → immutables → L0 (newest→oldest) → L1..Ln by index lookup, bloom before disk.
Iterators: a merge iterator over memtables and per-level two-level iterators; snapshot filtering by
seqno; `prefix_same_as_start` short-circuits when the prefix changes. Block cache: sharded LRU
(8 shards, 256 MiB *default*), keyed by `(file_number, block_offset)`.

## 5. Consensus (`esker-raft`)

A faithful implementation of Raft (Ongaro's dissertation) as a pure state machine, modeled on
etcd/raft-rs's `RawNode`/`Ready` split.

```rust
pub struct RawNode<S: LogStorage>;
impl<S: LogStorage> RawNode<S> {
  fn new(config: Config, storage: S) -> Result<Self>;
  fn tick(&mut self);                       // one logical tick; election/heartbeat timeouts count ticks
  fn step(&mut self, msg: Message) -> Result<()>;   // never fails on a message's content, never panics
  fn propose(&mut self, data: Bytes) -> Result<()>;
  fn propose_conf_change(&mut self, cc: ConfChange) -> Result<()>;
  fn read_index(&mut self, ctx: Bytes);
  fn campaign(&mut self) -> Result<()>;     // stand for election now; the simulator drives elections with it
  fn transfer_leader(&mut self, target: NodeId);
  fn has_ready(&self) -> bool;
  fn ready(&mut self) -> Ready;             // { hard_state, entries, snapshot, messages, committed_entries, read_states }
  fn advance(&mut self, rd: &Ready);        // caller has discharged the contract below
  fn role(&self) -> Role;  fn term(&self) -> Term;  fn leader(&self) -> Option<NodeId>;
  fn commit_index(&self) -> Index;  fn status(&self) -> Status;
  fn storage(&self) -> &S;  fn storage_mut(&mut self) -> &mut S;
}
pub trait LogStorage { initial_state, entries(lo, hi, max_bytes), term(idx), first_index, last_index, snapshot }
pub struct MemStorage;                      // in-memory LogStorage, for tests and the simulator
```

Driver contract (implemented in `esker-store`), in order — the normative text is the doc comment on
`Ready` itself, and `docs/raft-spec.md` gives each rule a row:

1. persist `hard_state` and `entries` (fsync) **before** sending any of `messages`;
2. apply `snapshot` before `entries` when both are present;
3. apply `committed_entries` in order, exactly once;
4. answer a `read_state` only once the state machine has applied through its index;
5. then `advance()`.

Violating rule 1 breaks safety — it is also what makes the leader's own bookkeeping sound, since a
leader counts itself as holding an entry before any fsync and may only do so because it cannot send
before persisting. The simulator tests violations explicitly. A `Ready` that is dropped without
being advanced re-offers its state; its `messages` are taken, which is safe because the network may
lose a message anyway.

Features by sub-phase: leader election with randomized timeouts (10–20 ticks *default*, tick = 100 ms),
log replication with batching and flow control (`max_inflight_msgs`), **pre-vote**, **check-quorum**,
leader **ReadIndex**, log compaction and **snapshots** (InstallSnapshot streamed by the store), single-server
**membership change** (add/remove one voter or learner at a time; joint consensus is an ADR for later),
**learners**, leadership transfer. The message set folds heartbeats into `AppendEntries` and
acknowledges a snapshot with `AppendEntriesResponse` — [ADR 0007](adr/0007-raft-message-set.md).

Determinism rules: no `Instant`, no `rand::thread_rng` — the RNG is injected and seeded, and the node
id selects its PCG stream so one seed reproduces a whole cluster; no `HashMap` appears in the crate,
because its iteration order would be a decision input; every decision is a function of
`(state, message | tick)`. [ADR 0008](adr/0008-raft-determinism-and-the-driver-contract.md).

## 6. Store (`esker-store`)

One process = one store id, hosting many **regions**. `Region { id, start_key, end_key, peers: [Peer{store_id, peer_id, role}], epoch: {conf_ver, version} }`.
Regions cover the whole key space contiguously; the first region is `["", "")`.

- **Raft log storage:** engine CF `raft`, keys `'l' ++ region_id:u64 ++ index:u64` → entry;
  `'s' ++ region_id` → hard state + apply state; `'m' ++ region_id` → region metadata. Ids and indices are
  big-endian, so a scan over one region's entries runs in index order. One WriteBatch per
  Ready (entries + hard state), `sync = true`. (TODO(post-v1): move to a bitcask-style log like TiKV's
  Raft Engine once measured to be the bottleneck.)
- **The `'m'` record is what a restart replays from.** A store reads every `'m'` record at open and starts
  a peer for each — never re-deriving its region set from its configuration, and never taking it from PD's
  reply, which is a different question: PD's view is built out of this store's own heartbeats. A record
  whose peer list does not name this store is **not** started; that is what a crash between `RemovePeer`
  applying and the data being deleted leaves behind, and starting it would return a voter to a group that
  has removed it. A fresh database with no records is the only bootstrap.
- **Apply loop** *(4a: one driver thread per region; pooling is 4d)*: each committed entry is decoded into
  a `WriteBatch` on data CFs plus `apply_index`, written atomically; admin entries (split, conf change)
  are applied under the region lock and bump the epoch. **`apply_index` is per region and one batch never
  spans two regions**, so a restart replays each region from its own `apply_index + 1` and entries of one
  region can never interleave with another's. Where that work *runs* is the part still open: 4a gives each
  region the driver thread phase 3e gave the single one, which is one OS thread per region and does not
  reach fifty. One worker per store — what this line said before 4a — is not the fix either, because then
  one region's `fsync` blocks every other region's consensus; the shape to measure in 4d is a **pool
  sharded by region id**, sized independently of the region count, as TiKV's store and apply pools are.
- **Split:** triggered by a periodic size check on the **leader** (region > 96 MiB *default*;
  `TODO(phase-4d)` an explicit admin command as well). The leader picks a boundary from the region's own
  data ([ADR 0012](adr/0012-split-key-selection.md) — the engine exposes no per-range key sample, so it is
  a bounded sampling scan), asks PD for one id per new region plus one per peer, and proposes
  `Split{split_key, new_region_id, new_peer_ids}` through its own group. On apply, **on every peer**: the
  parent shrinks to `[start, split_key)` and the child takes `[split_key, end)` with fresh peer ids on the
  *same stores*, both halves' `version` bumps, and both `'m'` records go into the **same batch as
  `apply_index`** — so a crash has both or neither. The child's log starts at index 0 and its group elects
  from scratch; its `conf_state` is the split-time membership, which for a log beginning at index 0 is
  exactly what `InitialState::conf_state` means. Merge is post-v1.
  - **A write the split overtook is refused at apply, not at propose.** A command ordered after the
    `Split` entry is checked against the parent's *narrowed* range and answered `EpochNotMatch` — the
    proposer routed correctly and the region moved under it, so the refusal is retryable and its hint is
    what the retry routes from. `KeyNotInRegion` would be terminal, and would fail a write that one
    refresh completes. Every peer's range at a given entry is the product of the same log prefix, so the
    refusal is deterministic, which is the only kind apply may make.
  - **Replay is idempotent by the range.** After the split, `split_key` is no longer strictly inside the
    parent, so a re-applied entry does nothing. There is no marker to write and nothing to keep in step.
- **Snapshots:** a peer whose next index is below the leader's first index has a hole no
  `AppendEntries` can fill, so it is sent the region's *state*. The **receiver asks**
  (`RaftTransport::Snapshot`, §9) and the leader answers with a run of `Stream` frames in 1 MiB
  chunks, each checksummed; the `InstallSnapshot` Raft message is only the announcement and carries
  no data, because the core reads nothing but its metadata (§5).
  - **Key-value pairs, not SST files, in v1.** §6 originally described `engine.checkpoint(range)` →
    `ingest()`, and two things stop it: a checkpoint links *whole files* and a file straddles a
    region boundary, so the receiver would get its neighbour's keys; and `Db::ingest` refuses any
    overlap including tombstones (§4.1), so a receive retried after a partial one could never
    ingest again. The bytes cross a network either way, so what is given up is one write on the
    receiver. `TODO(post-v1)`: sequence-number rewriting plus a range-clipped checkpoint makes the
    link-only transfer possible, and only `esker-store/src/snapshot.rs` changes.
  - **A snapshot is never half-visible**, which is the property everything else is arranged around.
    The receive is four durable steps — announce, stage, ingest, adopt — and nothing serves the
    region until the last. A crash leaves an announcement record naming the region, and the next
    open clears the keys that record's *range* names, so the retry finds the empty range it needs.
  - **Only into a range this store holds nothing in**, which is the case a new replica is. A peer
    that already has data is refused and stays behind; PD sees it in the heartbeats.
- **Membership:** `AddPeer` and `RemovePeer` ride on the answer to a region heartbeat (§7) and the
  leader proposes the matching conf-change entry. The **store id of a new replica travels in the
  change's context**, which `esker-raft` never interprets — so the core moves the membership and
  the store moves the region's peer list and `conf_ver` from the same entry.
  - **Learner first.** An `AddPeer` for an unknown peer adds a *learner*, which receives the log
    without voting and so never makes a quorum harder to reach while it catches up. The same
    operator for a peer that is already a learner is the **promotion** — and it is PD's call, not
    the leader's, because "has it caught up" is a comparison of two stores' applied indices and PD
    is the only party that sees both. A leader promoting on a guess puts a peer with no data into
    the quorum and the group stops committing.
  - **`RemovePeer` tears down the raft state and leaves the data.** Removing the keys means point
    deletes over the range (no range tombstones in v1, ADR 0006), and the tombstones that leaves
    are keys in the range — which is exactly the state that stops the range ever receiving a
    snapshot again. The keys stay, no region covers them, and nothing serves them.
- **Log compaction:** a peer throws away the head of its log once the apply index has run
  `RAFT_LOG_COMPACT_THRESHOLD` entries past the truncation point, keeping `RAFT_LOG_KEEP_ENTRIES`
  behind it so that a follower one entry behind does not need a snapshot. The record written with
  the truncation carries the term of the entry the log now begins after and **the membership as of
  that index** — not the membership in force, which a conf change above the index has already
  moved.
- **Transport:** one TCP connection per (store, store) pair carrying `RaftTransport::Batch` frames with
  `RaftMessage`s for all regions, batched per tick.
- **Heartbeats:** store heartbeat (capacity, load, region and leader counts) every 10 s; region heartbeat
  from each leader — and only from a leader — every 60 s **or on change**, where a change is an epoch bump
  or a leader change. The "or on change" half is the one that matters: an epoch bump is a split or a
  membership change, and every client cache in the cluster is wrong until PD knows, so waiting out the
  sixty seconds would leave `EpochNotMatch`'s hint as the only repair — which is the load that hint exists
  to avoid. Both cadences are counted in **ticks**, not read from a clock, so "every 60 s or on change" is
  a rule a test drives rather than waits for. `capacity`, `available`, `applied_bytes` and a region's
  `approximate_size` are reported as **zero in 4a** and documented as placeholders at each field: a
  filesystem's size needs `statvfs`, which `std` does not expose and no allowlisted crate provides without
  compiling C (an ADR of its own, when 4d's balance operators need the number), and per-region size comes
  from SST properties in 4b.

## 7. Placement driver (`esker-pd`)

Single binary, state kept in its own `esker-engine` instance (default column family only); made highly
available by running three PDs replicated with `esker-raft` (*sub-phase 4e — until then, one PD with
durable state is what 4a ships, and it is a single point of failure by design rather than by oversight*).

- **State (*fixed*, version 1).** PD's own key space, under the `'m'` metadata prefix of §3, with ids
  big-endian so a scan runs in id order:

  ```text
  'm' 'c'                        cluster record: the cluster id, the first region, the created ms
  'm' 'a'                        allocator record: the end of the reserved id batch
  'm' 'k' ++ tag:u8 ++ end_key   range index: which region ends here (tag 1 bounded, 2 = +∞)
  'm' 'r' ++ region_id:u64 BE    region record: the Region, the leader hint, the last heartbeat
  'm' 's' ++ store_id:u64 BE     store record: address, stats, the last heartbeat
  'm' 't'                        the oracle's high-water mark, in physical milliseconds
  ```

  Every value is `version:u8 ++ fields`, decoded strictly and golden-tested; the engine's own checksums
  cover the bytes, so records carry no second CRC. The **range index** is what makes `GetRegion` one
  seek: it is keyed by *end* key, tagged so that the region running to +∞ sorts last (`b""` alone sorts
  first), and a lookup seeks to `key ++ 0x00` because an end key is exclusive. Both keys of a region are
  written in one `WriteBatch`, so the index can never name a region that is not there.
- **Bootstrap:** the first store to register receives region 1 covering everything, with one voting peer
  on itself, and PD mints the **cluster id** that every later request is checked against. `Bootstrap` is
  idempotent and is also registration — a store calls it on every start, and only the first call in the
  life of a cluster comes back with a region to create; every other one answers `region: None` and the
  store reads the regions it hosts off its own disk (§6).
- **Routing:** `GetRegion(key) → Region + leader hint + the addresses of its peers' stores`; clients cache
  and invalidate on epoch errors. Before anything has bootstrapped it is a typed `NotBootstrapped`, never
  an empty answer. Region records are upserted by heartbeat under an **epoch guard**: a beat behind in
  either counter of `(conf_ver, version)` is dropped, and within one epoch the higher Raft term wins,
  because a leader election bumps the term and not the epoch. Heartbeats cross on the network whenever a
  leader changes, so the order PD accepts them in is the order they happened, not the order they arrived.
- **Ids:** `AllocId(count)` hands out consecutive cluster-unique ids from a reserved batch (1,000 by
  *default*). The end of a batch is persisted, `sync = true`, **before any id in it is handed out**, so a
  crash skips ids and can never repeat one.
- **TSO:** `ts = physical_ms << 18 | logical`, allocated in batches. PD persists a high-water mark 3 s
  *ahead*, fsynced before any timestamp at or above the old mark leaves, so **every timestamp handed out
  has `physical < mark`**; a restart resumes at `max(clock, mark)` and therefore cannot repeat one even
  when the wall clock jumps backwards. This is the only place in Esker that reads a wall clock, and it
  reads it through an injected `Clock` so that a test can make it misbehave.
- **Liveness:** a store is down when PD has not heard from it for `max_store_down_time`. The age is on
  **PD's clock** — the last-heartbeat stamp is written when the beat arrives and never taken from the
  store's own report, because comparing wall clocks across nodes is what invariant 6 forbids. A store PD
  has no record of is *not* down: unknown is not evidence, and a PD that read it as one would try to
  repair every region in the cluster on its first heartbeat after a restart.
- **Replica repair (4c):** every region with a peer on a down store and fewer than `target_replicas`
  live ones gets an `AddPeer` onto the emptiest live store that has no peer of that region (lowest store
  id breaks the tie, so the same data always chooses the same place); once the new replica is a voter,
  the dead peer gets a `RemovePeer`. **Add before remove**, always: removing first takes a three-replica
  region with one dead peer down to one live replica out of two. A region that is merely
  under-replicated is left alone — growing a healthy cluster to its target is balance, not repair.
- **Operators ride on the heartbeat response.** At most one per region is in flight, and the same one is
  re-sent on every heartbeat until a heartbeat *shows* it happened, it is contradicted, or it stops
  making progress for `operator_timeout`. Progress is observed and never assumed: the peer list and the
  epoch are the only evidence. The receiving store checks the epoch the operator carries, so a repeat is
  refused rather than applied twice — which is what makes re-sending safe and a lost response cost one
  heartbeat interval. Scheduling happens **on the heartbeat and nowhere else**: a store going down is
  noticed by absence, so the trigger is a surviving leader's beat, and repair latency is
  `max_store_down_time` plus one region-heartbeat interval with no timer thread anywhere.
- **In-flight operators are not persisted.** A PD restart forgets them and re-derives what is needed
  from the next round of heartbeats, which is why the repair rule is a pure function of (routing table,
  store liveness, in-flight set). A re-derived `AddPeer` mints a *fresh* peer id from the persisted
  allocator: reusing one PD has forgotten could put two peers under one id while the first is still
  being added.
- **Balance (4d):** leader count and region count are spread across live stores, one region decided on
  its own heartbeat like everything else. A move is proposed only when the gap between the busiest and
  quietest store is **at least two**, because a move takes one from the busy store and gives one to the
  quiet one — so acting on a gap of one would turn 5 vs 4 into 4 vs 5 for ever, while acting at two makes
  every move strictly reduce the spread and the cluster settle at a gap of at most one and stop
  ([ADR 0018](adr/0018-balance-moves-the-spread-by-two.md)). Counts are **effective** counts: an
  operator's effect is applied when it is issued and withdrawn when it retires, so a round of decisions
  is a sequence rather than a hundred independent readings of the same stale numbers. Region count is
  decided before leader count, because moving a replica takes any leadership of that region with it; a
  replica move is add-then-remove, and the leader's own replica is not the one that moves. A per-region
  `balance_cooldown` is a second guard against a store whose reports lag; repair ignores it, and so does
  the second half of a move already begun. Balance can be switched off with repair left on.
- **Operator history.** The last 64 operator events — issued, done, cancelled, timed out — are kept in
  one bounded record on disk, so `esker pd inspect` can say what PD asked a cluster to do after the
  process is gone. A debugging record only: no decision reads it, and losing it costs an explanation
  rather than a repair.
- **Tools:** `esker pd serve --data-dir --listen` runs it; `esker pd inspect --data-dir` prints the whole
  state above, including the range index beside the records it points at.

## 8. Transactions (`esker-txn`)

Percolator, optimistic, snapshot isolation (the TiKV model). Three CFs:

| CF | key | value |
|---|---|---|
| `default` | `'x' ++ enc(user_key) ++ enc_ts(start_ts)` | value (when > 255 bytes) |
| `lock` | `'x' ++ enc(user_key)` | `{ kind, start_ts, ttl, primary, short_value? }` |
| `write` | `'x' ++ enc(user_key) ++ enc_ts(commit_ts)` | `{ kind: Put/Delete/Rollback/Lock, start_ts, short_value? }` |

`enc` is the group encoding of §3 and `enc_ts` the complemented timestamp; every key named in a
record, on the wire or by a client is the **user key**, and this layer applies the namespace on the
way to the engine, as the store rather than the client applies `'r'` (§10). `docs/txn-spec.md` is
this table written out to the byte, with the decision matrix and the SI guarantees; the encodings
themselves are [ADR 0015](adr/0015-txn-record-encodings.md).

Protocol: `start_ts` from TSO → buffered writes on the client → **Prewrite** all keys (primary first;
each key checks `write` for commit_ts > start_ts and `lock` for any lock, then writes `lock` +
`default` atomically) → `commit_ts` from TSO → **Commit** primary (write `write`, delete `lock`, atomic)
→ commit secondaries asynchronously. Readers that hit a lock inspect the primary: rolled forward if the
primary is committed, rolled back if its TTL expired, else wait/backoff. GC: PD publishes a safepoint;
a `CompactionFilter` drops versions below it (keeping the newest visible one).

## 9. Wire API (`esker-proto`)

Hand-rolled, pure Rust, no tonic/prost/serde. One TCP connection carries multiplexed request/response
frames (*fixed* framing, version 1):

```
frame   = len:u32 ++ crc32c:u32 ++ kind:u8 ++ request_id:u64 ++ body   (all little-endian)
len     = the bytes after the len field itself: 13 + body.len()
crc     = crc32c(kind ++ request_id ++ body) — NOT the body alone, and never the len
kind    = Request 1 | Response 2 | Stream 3 | StreamEnd 4 | Error 5 | Ping 6 | Pong 7
body    = tag:u16 ++ hand-encoded fields (varints, length-prefixed bytes) — one `encode/decode`
          pair per message type in esker-proto, golden-tested, with an explicit `WIRE_VERSION`
```

The checksum covers the kind and the request id because a flipped `request_id` whose body still
checksummed would deliver a response to the *wrong caller* with both frames otherwise intact — the
one framing failure no layer above could detect. Kind `0` is reserved and never valid, as in the
WAL record header (§4.3), so a run of zero bytes is not a readable frame. `tag` is the method in a
request or a response and the error code in an `Error` frame; `Ping`, `Pong`, `Stream` and
`StreamEnd` carry no tag, and a stream chunk's body *is* the chunk.

The runtime side is `tokio` TCP with a per-connection writer task and a demultiplexer keyed by
`request_id`; streams (snapshot transfer) are chunked frames. `request_id` is **client-assigned and
unique while in flight**: a duplicate is a typed error, never a silently replaced waiter. Nothing on
a connection is unbounded — the writer queue, the in-flight table and the stream buffers all have
limits, and a peer at one answers `ServerIsBusy` rather than queueing until it dies. A connection
that goes silent is pinged, and one that stays silent is dropped with every waiter failed.

`TransportConfig` holds the knobs, and both ends of a connection carry their own:

| Knob | Default | What it bounds |
|---|---|---|
| `max_frame_size` | 16 MiB | Largest frame read or written, `len` field included. Enforced on both sides — a peer never sends what it would refuse to receive. |
| `max_in_flight` | 4096 | Requests outstanding on one connection. Past it a caller gets `ServerIsBusy` and a server sheds rather than queues. |
| `write_queue` | 256 frames | Encoded frames held for the writer task. This is the backpressure that stops a slow socket becoming an unbounded queue. |
| `keepalive_interval` | 10 s | Silence before a `Ping` goes out. |
| `idle_timeout` | 30 s | Silence before the peer is presumed gone and every waiter is failed. |
| `request_timeout` | 30 s | How long a call waits for its answer before `Timeout`. |
| `shutdown_grace` | 10 s | How long a graceful shutdown waits for in-flight requests before closing anyway. Graceful cannot mean "for ever": a handler wedged on a stuck disk must not hold the process open. |

`HelloAck` reports the server's `max_frame_size` so a client can refuse an oversized request without
spending a round trip on it. **A client is not obliged to adopt it**, and `esker-client` does not:
`Transport::max_frame_size` returns the client's own limit, so a client configured more generously
than its server sends a frame the server refuses and learns about it from the framing error rather
than from a local check. That is a wasted round trip and a closed connection, never a wrong answer,
and it only arises when the two are configured differently — but the value is on the wire so that a
client that wants the cheaper failure can have it.

Every failure also says **whether the request may have taken effect**, because a retry is only free
when the first attempt provably did nothing: `NotSent` means the bytes never left and the request is
safe to send again, while `Closed` and `Timeout` mean it went out and no usable answer came back.
Collapsing those two would leave a client unable to tell a write it may repeat from one it may not,
and `esker-txn`'s `Prewrite` will need the same distinction (§8).

Methods: `RawKv { Get, BatchGet, Put, BatchPut, Delete, DeleteRange, Scan, CompareAndSwap }`
(namespace `'r'`), `TxnKv { Get, Scan, Prewrite, Commit, Rollback, ResolveLock, Heartbeat, GcSafepoint }`
(namespace `'x'`), `Pd { Bootstrap, StoreHeartbeat, RegionHeartbeat, GetRegion, AllocId, Tso }`,
`RaftTransport { Batch, Snapshot }` — `Snapshot` is the one **streamed** method, answered with a run
of `Stream` frames rather than a `Response` (§6). Every KV request carries `{ region_id, epoch, peer }` and every error is a
typed enum with redirect hints (`NotLeader{leader_hint}`, `EpochNotMatch{current_regions}`,
`KeyNotInRegion`, `ServerIsBusy`, `Locked{lock_info}`). A `Pd` request carries the **cluster id** in
place of the region header, since PD's answers are about the routing table rather than about a region,
and `Bootstrap` may send zero because asking is how a caller learns it; PD's own two refusals are
`NotBootstrapped` and `ClusterMismatch{expected, actual}`, and neither is retryable. Unknown methods and fields are errors, not
ignored — forward compatibility is handled by `WIRE_VERSION` negotiation on connect.

Method numbers are `service:method`, so a service's numbers stay contiguous and one can be reserved
before it is written: `0x00` system (`0x0001` Hello), `0x01` RawKv (`0x0101`–`0x0108`, in the order
listed above), `0x03` Pd (`0x0301`–`0x0306`, in the order listed above) and `0x04` RaftTransport
(`0x0401`), with `0x02` TxnKv reserved. `Hello`'s layout is
frozen for ever — a fixed four-byte version and nothing else — because reading it is how a peer at
another version turns a mismatch into `WireVersion` rather than a hang.

## 10. Client (`esker-client`)

`RawClient` now, `TxnClient` in phase 5 (begin / get / scan / put / delete / commit / rollback). The
core is synchronous and has no I/O of its own: bytes leave through a `StoreTransport` (addressed by
**store id**, not by address — resolving one to a socket is PD's job) and time enters through a
`Clock`. Both are injected, so every rule below is tested against a scripted transport and a clock
that jumps rather than waits.

- **Region cache** keyed by range, `GetRegion` on miss (`RegionResolver`). It is a *hint, never an
  authority*: every request carries the epoch the cache believes and the store checks it, so a stale
  entry costs a redirect and never a wrong answer. Keyed by **`start_key`**, walked backwards to the
  last region starting at or before the key and then checked to reach it: an empty `end_key` means
  `+∞` but sorts *below* every key, so a map keyed by `end_key` loses the last region of the cluster
  for ever. A resolver answers `Ok(None)` for a key no region covers — terminal, since waiting does
  not create one — and `Err` when it could not say, which is usually retryable; collapsing the two
  would turn a momentary PD outage into a terminal error on every call in the process.
- **Retries** are bounded by both a budget (8 retries *default*) and a per-call deadline (10 s
  *default*), whichever ends first. Which errors are retryable is `ProtoError::is_retryable()` —
  asked, not duplicated, so the client and the store cannot drift: `NotLeader` (follow the peer-id
  hint), `EpochNotMatch` (take the replacement regions), `RegionNotFound` (drop the entry, ask the
  resolver), `ServerIsBusy` (wait). Backoff is exponential to a 2 s ceiling with **equal jitter**
  from a per-client seeded PCG32, so a leader election does not reconverge every client in lockstep.
- **A write is re-sent only when the previous attempt provably did not commit.** Every retried error
  is a refusal, so `outcome() == NotApplied`. A request that went out and got no usable answer is
  never retried: a *mutation* in that position becomes `Error::AmbiguousResult` and the caller
  decides; a *read* is returned plainly, because re-reading is always safe. `esker-txn` depends on
  this distinction (§8), which is why it is a type and not a log line.
- **Bounded everything:** retries, calls in flight (256 *default*), and scan limits, capped so a
  response fits `max_frame_size`; an oversized request is refused before it is sent.
- **The client never namespaces a key.** The `'r'` prefix of §3 is applied by the store on every
  path, scan bounds and `DeleteRange` included.

## 11. Testing strategy (required per component)

| Component | Unit | Property (proptest) | Golden files | Crash / fault | Simulation / model |
|---|---|---|---|---|---|
| codec, WAL, SST, manifest | yes | encode/decode round trip, ordering | yes (fixed formats) | torn tail, bit flips, truncation, partial rename | — |
| engine (`Db`) | yes | random ops vs `BTreeMap` model | — | subprocess kill -9 loop under load; recovery must equal model | — |
| raft | yes | — | — | — | seeded discrete-event simulator (partition, delay, dup, reorder, crash/restart, disk stall); `stateright` model of safety properties |
| store / split / snapshot | yes | — | — | kill during split, during snapshot ingest | multi-node simulator over `esker-sim` network |
| pd | yes | — | — | restart mid-operator | — |
| txn | yes | — | — | crash between prewrite and commit | bank test (sum invariant), linearizability checker on single-key history |
| whole cluster | — | — | — | docker-compose chaos: 5 stores, partitions with `tc`/`iptables` | — |

`esker-sim` provides: a `Clock`, `Network` (trait implemented by the real TCP transport and by the simulator), a
`FaultPlan`, and a **history checker** — a Porcupine-style WGL linearizability checker for a single-key
register model, and a bank-transfer invariant checker for transactions. Every simulator failure prints its
seed; every seed is reproducible.

## 12. Observability

`tracing` spans per request with region/peer ids; Prometheus metrics (write stall state, L0 file count,
pending compaction bytes, raft proposal latency, apply lag, region count, TSO rate); `esker-cli` commands:
`sst-dump`, `wal-dump`, `manifest-dump`, `region ls`, `region split`, `bench`.

## 13. Roadmap hooks for SQL and serverless

- **SST tiering:** all SST reads go through a `FileSystem` trait (`open`, `read_at`, `list`, `rename`,
  `delete`, `fsync_dir`) with a local implementation now and an S3-backed one later; the block cache plus
  a local SST disk cache make S3-resident SSTs viable. WAL and Raft log stay local. The S3 client is a
  small in-house implementation of the handful of calls we need (PutObject, GetObject with range, List,
  Delete, SigV4 signing over an in-house SHA-256/HMAC). **TLS is the known hard case for the pure-Rust
  rule:** options, to be settled by ADR in phase 6b, are (a) plain HTTP to a local MinIO / a TLS-terminating
  sidecar, (b) `rustls` with a pure-Rust crypto provider, (c) accepting one vetted exception. Design so
  the transport is a trait and the choice is local to one module.
- **Stateless SQL nodes (`esker-sql`):** in-house PostgreSQL wire protocol v3 (startup, simple and
  extended query, `psql` compatibility — ~2k lines, no `pgwire` crate) → SQL parser (the one expected
  large dependency exception, `sqlparser`, PostgreSQL dialect, by ADR) → catalog in `'m'` key space →
  planner/executor over `esker-client` transactions, using the `'t'` key layout from §3. Postgres
  compatibility is a surface, not a storage format.
- **Scale-to-zero:** because SQL nodes are stateless and SSTs can live in object storage, an idle tenant
  costs only its Raft metadata; PD may later hibernate cold regions (ADR).
- **Multi-tenancy:** tenant id is the first field of every SQL key; RawKV/TxnKV users may adopt the same
  convention. Per-tenant quotas are a PD concern, out of v1 scope.

## 14. Defaults (one place)

| Knob | Default |
|---|---|
| WAL block | 32 KiB (fixed) |
| memtable | 64 MiB, max 4 immutables |
| SST data block / restart interval | 4 KiB / 16 |
| bloom | 10 bits/key, prefix-extracted for versioned CFs |
| L0 trigger / slowdown / stop | 4 / 8 / 12 files |
| L1 base / multiplier / levels | 64 MiB / 10 / 7 |
| compaction output file size | 8 MiB |
| block cache | 256 MiB, 8 shards |
| manifest roll size | 64 MiB |
| compaction threads | 2 |
| region split size | 96 MiB |
| raft tick / election / heartbeat | 100 ms / 10–20 ticks / 2 ticks |
| max inflight raft msgs | 256 |
| store / region heartbeat | 10 s / 60 s — the region interval is also the latency of a PD operator, which has no other way to reach a store |
| raft log compact threshold / tail kept | 4096 / 1024 entries |
| snapshot chunk / stream depth | 1 MiB / 4 chunks |
| PD operator timeout (store side) | 5 s |
| PD id allocation batch | 1,000 ids per persist |
| PD TSO save interval | 3 s ahead of what is handed out |
| `max_store_down_time` | 30 s |
| `operator_timeout` | 300 s, measured from the last observed progress |
| `target_replicas` | 3 |
| `balance_cooldown` | 300 s per region, after an operator retires |
| leader / region spread threshold | 2 (a constant, not a knob — see ADR 0018) |
| PD operator history | 64 events |
| txn lock TTL | 3 s (heartbeat-extended) |
| transport (`TransportConfig`) | §9 has the table — seven knobs, listed there because each one only means something next to the rule it bounds |
| store WAL sync mode | `Never` — the engine adds no `fsync` of its own, so each request's `sync` flag decides (§4.2, and `CLAUDE.md` invariant 1's opt-out) |

## 15. Open questions (turn into ADRs as they are decided)

Joint consensus vs single-server changes only · separate Raft log store · async commit / 1PC · range
tombstones design · leader leases vs ReadIndex only · PD HA timing · secondary-index encoding for
composite keys · how much Postgres surface for the first SQL milestone.
