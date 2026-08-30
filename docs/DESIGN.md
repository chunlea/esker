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
'x' ++ user_key ++ enc_ts             TxnKV namespace (Percolator; see §8)
't' ++ tenant:u64 ++ table_id:u64 ++ 'r' ++ row_id      SQL rows (phase 6)
't' ++ tenant:u64 ++ table_id:u64 ++ 'i' ++ index_id ++ cols [++ row_id]   SQL indexes
'm' ++ ...                            cluster / catalog metadata
```

`enc_ts` = `!(ts as u64)` big-endian (bitwise NOT), so newer versions sort first within a user key.
The engine's prefix extractor for versioned CFs is "strip the last 8 bytes".

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
  fn checkpoint(&self, dir, Option<(cf, range)>) -> Result<()>;   // hard-links SSTs, copies manifest
  fn ingest(&self, cf, sst_paths) -> Result<()>;                   // phase 4 snapshots, phase 6 bulk load
  fn flush(&self, cf) / compact_range(&self, cf, range) / property(&self, name)
}
```

### 4.2 Write path

`write()` → assign seqno → group commit → WAL append (+ fsync if `sync`) → insert into the active
memtable of each touched CF → return. Group commit: the first writer to take the write lock becomes
leader, drains the queue (bounded by 1 MiB or 128 batches, *default*), writes one WAL record group,
syncs once, then wakes everyone. Sync mode per write; `Options::wal_sync_mode = {PerWrite, Interval(ms), Never}`.

### 4.3 WAL format (*fixed*)

LevelDB's block format: 32 KiB blocks; record header = `crc32c:u32 ++ len:u16 ++ type:u8` (7 bytes);
types `FULL/FIRST/MIDDLE/LAST`; a block trailer smaller than 7 bytes is zero-filled. Payload of one record
= one serialized `WriteBatch`: `seqno:u64 ++ count:u32 ++ entries…`, entry = `cf:u32 ++ kind:u8 ++ key ++ value`
(length-prefixed varints). One WAL segment per memtable generation, named `NNNNNN.wal`. The CRC is seeded
with the record type (as LevelDB does) so a header from another position cannot be replayed.

Recovery: read segments in order, verify every record, apply into fresh memtables, stop at the first torn
record of the **last** segment (normal), fail on corruption anywhere else unless `Options::paranoid = false`.

### 4.4 Memtable

`crossbeam-skiplist` (the one bought piece of concurrent code; an in-house arena skiplist is a
post-v1 replacement behind the same `MemTable` trait) keyed by internal key
`user_key ++ seqno:u56 ++ kind:u8` (LevelDB layout). One
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
deleted only after no `Version` references them.

### 4.7 Compaction

Leveled. L0 flush trigger 4 files, slowdown 8, stop 12 (*default*). L1 base 64 MiB, ×10 per level, 7
levels (*default*). Score-based picking; L0→L1 merges all overlapping L0 files. Compaction runs on a
bounded thread pool (2 threads *default*) via a `CompactionJob` that is a pure function of its inputs (so it
is unit-testable without the `Db`). `CompactionFilter` trait lets `esker-txn` drop MVCC versions below
the safepoint. Range deletions are implemented as range tombstones in v2; v1 rejects `DeleteRange`
across more than one SST boundary with an error (documented limitation, removed in phase 5).

### 4.8 Column families

Shared WAL and seqno space; separate memtables, levels, options (prefix extractor, block size,
compression, filter). `WriteBatch` across CFs is atomic. Built-in CFs on every store: `default`, `lock`,
`write` (Percolator, see §8) and `raft` (Raft logs and region metadata, §6). Dropping a CF is a
manifest edit followed by file deletion.

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
impl RawNode {
  fn tick(&mut self);                       // one logical tick; election/heartbeat timeouts count ticks
  fn step(&mut self, msg: Message) -> Result<()>;
  fn propose(&mut self, data: Bytes) -> Result<()>;
  fn propose_conf_change(&mut self, cc: ConfChange) -> Result<()>;
  fn read_index(&mut self, ctx: Bytes);
  fn has_ready(&self) -> bool;
  fn ready(&mut self) -> Ready;             // { hard_state, entries_to_append, snapshot_to_apply, messages, committed_entries, read_states }
  fn advance(&mut self, rd: Ready);         // caller has persisted hard_state+entries and sent messages
}
pub trait LogStorage { initial_state, entries(lo, hi, max_bytes), term(idx), first_index, last_index, snapshot }
```

Driver contract (implemented in `esker-store`): persist `hard_state` and `entries` (fsync) **before**
sending `messages`; apply `committed_entries` in order; then `advance()`. Violating the order breaks
safety — the simulator tests this explicitly.

Features by sub-phase: leader election with randomized timeouts (10–20 ticks *default*, tick = 100 ms),
log replication with batching and flow control (`max_inflight_msgs`), **pre-vote**, **check-quorum**,
leader **ReadIndex**, log compaction and **snapshots** (InstallSnapshot streamed by the store), single-server
**membership change** (add/remove one voter or learner at a time; joint consensus is an ADR for later),
**learners**, leadership transfer.

Determinism rules: no `Instant`, no `rand::thread_rng` — the RNG is injected and seeded; every decision is a
function of `(state, message | tick)`.

## 6. Store (`esker-store`)

One process = one store id, hosting many **regions**. `Region { id, start_key, end_key, peers: [Peer{store_id, peer_id, role}], epoch: {conf_ver, version} }`.
Regions cover the whole key space contiguously; the first region is `["", "")`.

- **Raft log storage:** engine CF `raft`, keys `'l' ++ region_id:u64 ++ index:u64` → entry;
  `'s' ++ region_id` → hard state + apply state; `'m' ++ region_id` → region metadata. One WriteBatch per
  Ready (entries + hard state), `sync = true`. (TODO(post-v1): move to a bitcask-style log like TiKV's
  Raft Engine once measured to be the bottleneck.)
- **Apply loop:** one worker per store (sharded by region id later); each committed entry is decoded into
  a `WriteBatch` on data CFs plus `apply_index`, written atomically; admin entries (split, conf change)
  are applied under the region lock and bump the epoch.
- **Split:** triggered by a periodic size check (region > 96 MiB *default*, or by an explicit admin
  command). Leader asks PD for new ids, proposes `Split{split_key, new_region_id, new_peer_ids}`; on
  apply both halves are created on every peer with the same membership; the new region's Raft group starts
  with the parent's peers. Merge is post-v1.
- **Snapshots:** `engine.checkpoint(range)` → SSTs + metadata, streamed as `Stream` frames in 1 MiB
  chunks with checksums; receiver `ingest()`s into place then applies the Raft snapshot metadata.
- **Transport:** one TCP connection per (store, store) pair carrying `RaftTransport::Batch` frames with
  `RaftMessage`s for all regions, batched per tick.
- **Heartbeats:** store heartbeat (capacity, load) every 10 s; region heartbeat from each leader every
  60 s or on change.

## 7. Placement driver (`esker-pd`)

Single binary, state kept in its own `esker-engine` instance; made highly available by running three PDs
replicated with `esker-raft` (sub-phase 4c — until then, one PD with durable state is acceptable).

- **Bootstrap:** first store to register receives region 1 covering everything.
- **Routing:** `GetRegion(key) → Region + leader hint`; clients cache and invalidate on epoch errors.
- **TSO:** `ts = physical_ms << 18 | logical`; PD persists a high-water mark 3 s ahead so restarts never
  hand out a smaller ts; allocated in batches to callers.
- **Scheduling (phase 4b):** replica repair (down store → add peer elsewhere), leader balance, region-count
  balance. Every operator is a small state machine with a timeout; PD never sends a second operator for a
  region while one is in flight.

## 8. Transactions (`esker-txn`)

Percolator, optimistic, snapshot isolation (the TiKV model). Three CFs:

| CF | key | value |
|---|---|---|
| `default` | `user_key ++ enc(start_ts)` | value (when > 255 bytes) |
| `lock` | `user_key` | `{ primary, start_ts, ttl, kind, short_value? }` |
| `write` | `user_key ++ enc(commit_ts)` | `{ kind: Put/Delete/Rollback/Lock, start_ts, short_value? }` |

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
frame   = len:u32 ++ crc32c:u32 ++ kind:u8 ++ request_id:u64 ++ body
kind    = Request | Response | Stream(chunk) | StreamEnd | Error | Ping | Pong
body    = method:u16 ++ hand-encoded fields (varints, length-prefixed bytes) — one `encode/decode`
          pair per message type in esker-proto, golden-tested, with an explicit `WIRE_VERSION`
```

The runtime side is `tokio` TCP with a per-connection writer task and a demultiplexer keyed by
`request_id`; `max_frame_size` 16 MiB (*default*); streams (snapshot transfer) are chunked frames.

Methods: `RawKv { Get, BatchGet, Put, BatchPut, Delete, DeleteRange, Scan, CompareAndSwap }`
(namespace `'r'`), `TxnKv { Get, Scan, Prewrite, Commit, Rollback, ResolveLock, Heartbeat, GcSafepoint }`
(namespace `'x'`), `Pd { Bootstrap, StoreHeartbeat, RegionHeartbeat, GetRegion, AllocId, Tso }`,
`RaftTransport { Batch }`. Every KV request carries `{ region_id, epoch, peer }` and every error is a
typed enum with redirect hints (`NotLeader{leader_hint}`, `EpochNotMatch{current_regions}`,
`KeyNotInRegion`, `ServerIsBusy`, `Locked{lock_info}`). Unknown methods and fields are errors, not
ignored — forward compatibility is handled by `WIRE_VERSION` negotiation on connect.

## 10. Client (`esker-client`)

Region cache keyed by key range; `GetRegion` on miss; automatic retry with backoff on redirectable
errors (bounded); `RawClient` and `TxnClient` (begin / get / scan / put / delete / commit / rollback).

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
| block cache | 256 MiB, 8 shards |
| compaction threads | 2 |
| region split size | 96 MiB |
| raft tick / election / heartbeat | 100 ms / 10–20 ticks / 2 ticks |
| max inflight raft msgs | 256 |
| store / region heartbeat | 10 s / 60 s |
| txn lock TTL | 3 s (heartbeat-extended) |

## 15. Open questions (turn into ADRs as they are decided)

Joint consensus vs single-server changes only · separate Raft log store · async commit / 1PC · range
tombstones design · leader leases vs ReadIndex only · PD HA timing · secondary-index encoding for
composite keys · how much Postgres surface for the first SQL milestone.
