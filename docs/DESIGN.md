# Esker design

Status: living document. Every phase updates the sections it touches. Numbers marked *default* are
configuration knobs with the stated default; numbers marked *fixed* are part of a format.

## 1. Goals and non-goals

Goals: a from-scratch, crash-safe, linearizable, horizontally scalable ordered KV with MVCC and
distributed transactions, designed so that a stateless SQL layer and object-storage tiering can be added
without changing the lower layers.

Non-goals for v1: multi-region geo-replication, encryption at rest, pessimistic
locks, secondary-index-aware engine features, anything that needs a wall clock for correctness.

**Online schema change left that list during v1**: `CREATE INDEX CONCURRENTLY` is four states and
a batched backfill ([ADR 0020](adr/0020-online-schema-change.md)), driven by the statement that
starts it and answered when the change is over — which is what a client sees on a real server
([ADR 0083](adr/0083-a-concurrent-build-answers-when-it-is-built.md)). `SET
esker.concurrent_index_build = 'stage'` is the other half of that contract — the job written and
left for a driver, which is what a cluster steps and what the state machine's own tests drive by
hand. It belongs to §13, the SQL surface, and this list is where it used to be.

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
  fn write(&self, batch: WriteBatch, WriteOptions { durability: Durability }) -> Result<SeqNo>;
  fn get(&self, cf, key, ReadOptions { snapshot, fill_cache }) -> Result<Option<Bytes>>;
  fn iter(&self, cf, ReadOptions) -> Result<impl Iterator>;   // prefix_same_as_start option
  fn snapshot(&self) -> Snapshot;
  fn create_cf(&self, name, CfOptions) -> Result<u32> / drop_cf(&self, name) -> Result<()>;
  fn checkpoint(&self, dir, Option<(cf, range)>) -> Result<()>;   // hard-links SSTs, writes a manifest
  fn ingest(&self, cf, sst_paths) -> Result<()>;                   // phase 6 bulk load; refuses a shared key
  fn flush(&self, cf) / compact_range(&self, cf, range) / property(&self, name)
}
```

**A write says what it wants of the log with a `Durability`, which has three states and not two**
([ADR 0036](adr/0036-a-write-may-have-no-opinion-about-durability.md)). `Durable` and `Buffered` are
invariant 1's demand and its one sanctioned opt-out, and they outrank the database's policy in both
directions. The third, `Policy`, is the default and is the one a `bool` could not express: **no
opinion**, which leaves the decision to whoever opened the database. Without it a caller taking the
default was indistinguishable from one demanding durability, `WalSyncMode` had nothing left to
decide, and `Never` disabled nothing.

The default remains slow and correct rather than fast and lossy, because the default `WalSyncMode`
is `PerWrite`: a write that expressed no preference is still synced. What moved is that a database
opened `Never` or `Interval` can now actually be opened that way.

A `Snapshot` belongs to the `Db` instance that issued it and is refused by any other: sequence
numbers survive a reopen, so a stale handle names a plausible number, but the reopened
database's snapshot list has never heard of it and the compaction floor can pass it by.

### 4.2 Write path

`write()` → assign seqno → group commit → WAL append (+ fsync if `sync`) → insert into the active
memtable of each touched CF → return. Group commit: the first writer to take the write lock becomes
leader, drains the queue (bounded by 1 MiB or 128 batches, *default*), writes one WAL record group,
syncs once, then wakes everyone. Each write states a `Durability`; the database's policy decides for
those that state `Policy`, and `Options::wal_sync_mode = {PerWrite, Interval(d), Never}` is that
policy — `Interval` by a real background thread, and `Never` syncing only for a `Durable` write and
at a clean close.

Two rules the implementation is not free to relax. The queue lock is **never held across the
`fsync`**, or every arriving writer serialises behind a disk flush and group commit becomes a
queue with extra steps. And a follower learns its sequence number **only after the leader has
published**, so no batch is observable at a sequence number before it is readable.

The leader does the log write for everyone, so **its failure is everyone's**: each batch in the
group is refused with `Error::GroupCommit` carrying the leader's message, because reporting
success to a writer whose bytes never reached the log would break invariant 1 for a write that
looked fine. A `Buffered` batch that rides a synced group gets durability for free, which is
correct — `Buffered` is permission to acknowledge early, never a requirement to.

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

An **in-house arena skiplist** ([ADR 0041](adr/0041-the-in-house-arena-skiplist.md)) keyed by internal
key `user_key ++ tag:u64` little-endian with `tag = (seqno << 8) | kind` (LevelDB layout, so the kind byte
physically precedes the 56-bit sequence number), ordered by user key ascending then tag **descending** so
the newest version of a key sorts first. One
active + a bounded queue of immutable memtables per CF. Flush when active reaches
`write_buffer_size` (64 MiB *default*). Stall policy: slow down at `max_immutable = 2`, stop at 4 — and
expose both as metrics so the stall is visible, never mysterious.

`crossbeam-skiplist` held this place until phase 11's debt was cleared, and was the last piece of
concurrent code the project bought rather than wrote; it and the two crates behind it are gone from
the runtime graph. One `type Selected = …` line in `memtable.rs` names the storage, behind a `Store`
trait, because the benchmark that decided it was mixed and a third layout may yet be tried.
The structure is single-writer (group commit serialises inserts, §4.2), multi-reader, and **append-only**
— nothing is ever removed, so no node is freed until the last `Arc<MemTable>` drops and the reclamation
question a lock-free map has to answer never arises. Key and value bytes and the node's forward pointers
live in an arena of chunks that are allocated once and never moved, addressed by `u32` offsets, so a
cursor is a node offset and `next` is a pointer hop into a borrow rather than a re-find of the
remembered key and a copy of the entry. Node heights come from a seeded PCG32 held on the table, so a
memtable's shape is a function of its seed and a failing test replays from one.

### 4.5 SST format (*fixed*, version 1)

Block-based table: data blocks (4 KiB *default* on local disk, **16 KiB when the database's SSTs
are tiered** — a block is one ranged `GET` there rather than a page-cache read, measured in
`docs/bench/phase-11-engine.md` §3) with prefix-compressed entries and restart points every
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
next file number, last seqno, comparator name, CF create/drop, and since phase 6b a file's tier
location). The location travels as its own record rather than as a field of the add, because an upload
finishes *after* the edit that names the file (§13, ADR 0024) and a promotion should cost four varints
rather than a second copy of every key bound. It is emitted only when the location is not `Local`, so a
database that never tiers writes byte-identical manifests to a phase-5 one. `CURRENT` names the active manifest and is
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

**Level scores are not the only trigger.** A score answers "is this level over its target", and a
store whose write buffer never fills has no levels to score — run 127e watched twenty files over
forty-six minutes and saw no compaction at all, while the versions it was keeping were collectable
the whole time. So a **rising safepoint** also asks for one: `esker_store::collect::Sweeper` sweeps
`default`, `lock` and `write` when the published number actually moves, debounced from the *end* of
the last sweep (`COLLECT_DEBOUNCE`, one store heartbeat, since that is when a store learns the
number at all). `raft` is left alone — it holds nothing a safepoint makes collectable.

**Two compactions must not touch one file, nor write overlapping ranges into one level.** The pool
is bounded but not serial, and `Db::compact_range` runs one inline on the caller's thread beside it,
so both halves of that rule are load-bearing. A plan reserves its **input files** by number and the
**key range it will write**, per `(column family, output level)`; a plan that cannot have all its
inputs, or whose range overlaps a running plan's range in the same level, is dropped rather than
queued — the picker produces it again in a moment against a version that has moved on. The range
claimed is the union of the plan's inputs in *user*-key order, which is the widest its outputs can
be. Two compactions into different levels never contend, which is what keeps the pool parallel; two
into the same level are ordered, which for `L0 → L1` means one at a time — where `LevelDB` and
`RocksDB` also arrive.

The inputs alone are not sufficient, and this section used to imply they were: L0 files legitimately
overlap each other, so two `L0 → L1` plans can hold disjoint input sets and still write overlapping
ranges into L1, at which point L1 stops partitioning the key space. Measured at 1 in 20 attempts
with a concurrent writer and 0 in 20 without
([ADR 0079](adr/0079-compaction-concurrency-reserves-the-output-range.md)). `version::builder`'s
`check_disjoint` still validates each level as a version is built, but it is the backstop rather
than the first line — it turns the race into a failed operation on a legal workload instead of a
corrupt level, which is what the reservation exists to prevent reaching at all.

**What a compaction drops, and the compaction that drops nothing.** A rewrite drops three kinds of
entry: a version no live snapshot can reach, a point tombstone with nothing beneath it, and whatever
the `CompactionFilter` refuses. All three are decisions the merge makes while reading, so a
compaction that does not read makes none of them — and a **trivial move**, which re-labels a single
non-overlapping input as belonging to the next level with one manifest edit and no bytes, does not
read. That is correct while the file is on its way down and wrong the moment it arrives: a point
tombstone becomes droppable exactly when no level below the output can still hold an older value,
and a move carries it past that moment unread, into a level nothing will ever compact it out of. So
the shortcut stands down when the output level is the last one — as it already did for a compaction
filter and for range tombstones — and the file is read once, on arrival. Above the bottom the move
stays free. Skipping this cost a column family with no filter every put and every delete it had ever
written (debt #62): 3,546 entries, unchanged by a full compaction.

**Range deletions.** `DeleteRange` is real ([ADR 0017](adr/0017-range-tombstones.md)). A range
tombstone `[begin, end)` is stored *beside* the sorted run rather than in it — a list in the
memtable, a block in the tables a flush writes — because it hides keys the run has never seen. A key
found at seqno `s` is hidden from a read at snapshot `t` when a tombstone covers it with
`s < tombstone.seqno <= t`; both bounds matter, and the first is what lets a write *after* a range
delete survive it. An empty or inverted range is refused as `InvalidArgument` rather than treated as
a no-op.

**A tombstone never reaches an SST below L0.** A compaction whose inputs carry one becomes a
*discharge*: it takes every file at every level that the tombstone covers, drops the covered keys
outright, and drops the tombstone with them. That keeps the read path's per-level binary search — which
assumes a level partitions the key space — correct without change, at the cost of one compaction over
the deleted range. It runs on the compaction thread pool, not on the write: a range delete is logged
and acknowledged like any other entry (invariant 1), and reads honour it from the memtable and L0
until the discharge comes round. A tombstone above the compaction floor is not consumed at all, because
a snapshot older than the delete still has to see what it deleted.

`esker-store`'s `RawKv DeleteRange` workaround — a bounded scan plus point deletes in one atomic batch
(ADR 0006) — can now become one `WriteBatch` entry.

### 4.8 Column families

Shared WAL and seqno space; separate memtables, levels, options (prefix extractor, block size,
compression, filter). `WriteBatch` across CFs is atomic.

**One shared WAL means one shared retirement rule**, and it is sharper than it looks: a segment
may be deleted only when *no* family still needs it, so the slowest family sets the pace for
every other. A family with unflushed data in segment *N* keeps *N* and everything after it. A
family that is **empty** keeps nothing — it has no unflushed data, and a write arriving later
lands in whatever segment is current then. Getting that second clause wrong is how an idle
family pins the log for ever: `esker-store` opens four families and a RawKV workload writes to
two, so `lock` and `write` would otherwise hold every segment the database ever wrote. See
`db/flush.rs`'s `oldest_log`, which carries the reasoning and the measurement.

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

**L0 is the exception to "per-level", and it has to be.** A level below L0 partitions the key
space, so a scan of it is inside one file at a time and the level is one cursor that opens the file
it has reached (`db/level_iter.rs`). L0's files overlap, so any of them can hold the next key and
all of them are cursors at once. That is also why the range-tombstone set is collected by walking
L0 alone: §4.7 discharges a tombstone rather than writing one below L0, so no deeper file can
carry one.

Open SST readers are a second cache in front of the block cache — a reader holds a file descriptor
and a resident index and filter — bounded by `Options::max_open_tables` (256 *default*) and
evicted **least-recently-used**. The rule matters more than it looks: file numbers rise
monotonically, so evicting the lowest discards the file that has survived the most compactions,
which is the deepest and most-read one in the tree.

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
  fn progress(&self) -> Vec<PeerProgress>;  // a leader's view of each peer; empty on a follower (4d)
  fn report_snapshot(&mut self, to: NodeId, status: SnapshotStatus);  // Finished | Failed (4d)
  fn storage(&self) -> &S;  fn storage_mut(&mut self) -> &mut S;
}
pub trait LogStorage { initial_state, entries(lo, hi, max_bytes), term(idx), first_index, last_index, snapshot }
pub struct MemStorage;                      // in-memory LogStorage, for tests and the simulator
```

`progress()` is a **snapshot, not a view**: `{ id, matched, next, is_learner, recent_active,
pending_snapshot }` per peer, sorted by id, copied out. It is deliberately not part of `Status`,
which is what a peer knows about itself and is answered by every role; this is what a *leader*
knows about others and is empty on anyone else, so a caller that reads it from a follower gets
nothing rather than a stale answer. The store uses it to refuse a leadership transfer to a peer too
far behind to take office (§6).

Driver contract (implemented in `esker-store`), in order — the normative text is the doc comment on
`Ready` itself, and `docs/raft-spec.md` gives each rule a row:

1. persist `hard_state` and `entries` (fsync) **before** sending any of `messages`;
2. apply `snapshot` before `entries` when both are present;
3. apply `committed_entries` in order, exactly once;
4. answer a `read_state` only once the state machine has applied through its index;
5. then `advance()`;
6. **report what became of a snapshot transfer** it carried out, with `report_snapshot`.

Rule 6 is not an ordering rule like the others, and it is the one that cost a stranded replica. A
leader that has offered a snapshot sends that peer nothing else — the wait ends only when the
follower acknowledges — so a transfer the follower never received is a wait with no end. The core
never saw a byte of it, so only the driver can say. Two backstops sit behind the report because a
report can be impossible rather than merely late: an acknowledgement at or past the pending index
makes the snapshot moot whatever happened, and after `SNAPSHOT_TIMEOUT_TICKS` the leader offers it
again anyway — which is what covers the case nobody owes a report for at all, an offer lost before
any transfer began.

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
**The randomisation above only separates peers if the driver never hands a peer a whole timeout at
once**: a batch that carries `election_tick` ticks in one call steps every peer past its deadline
in the same instant, and the redraw that ends a split vote never happens
([ADR 0101](adr/0101-a-batch-of-ticks-never-carries-a-whole-election.md), where it showed as a
pre-vote livelock with the term climbing to 617). A vote
is granted only to a candidate **this** configuration calls a voter, which is not the same question
as whether the candidate thinks it is one — a node promoted in a configuration the voter has not
applied yet is still a learner here, and granting it would elect a leader the group does not have
([ADR 0085](adr/0085-a-vote-is-not-granted-to-a-learner.md)).

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
- **Apply loop**: each committed entry is decoded into a `WriteBatch` on data CFs plus `apply_index`,
  written atomically; admin entries (split, conf change) are applied under the region lock and bump the
  epoch. **`apply_index` is per region and one batch never spans two regions**, so a restart replays each
  region from its own `apply_index + 1` and entries of one region can never interleave with another's.
- **Where that work runs: a fixed pool, regions pinned by id** *(4d; 4a shipped one thread per region)*.
  `DRIVER_WORKERS` threads, and region `id % workers` is the worker that drives it — for its whole life,
  and across restarts, because PD allocates ids in order and modulo therefore spreads them exactly evenly
  without anything being written down. Pinning, not scheduling: a region's messages reach one worker
  through one channel and are handled in arrival order, so per-region ordering is what it was when the
  region had a thread to itself. A worker holding several regions interleaves *between* them, which
  nothing depends on — two regions share no state, no batch and no apply index. One worker per store is
  the shape this rules out, because then one region's `fsync` blocks every other region's consensus; one
  thread per region is the shape 4a shipped, and it does not reach fifty. A driver error retires that one
  region and the worker keeps serving the rest. **A worker drains a batch until it has carried
  `TICKS_PER_BATCH` ticks — one below `esker_raft::ELECTION_TIMEOUT_MIN_TICKS` — and then drives,
  and only ticks count against it** because a batch of appends or reads is what batching is for
  ([ADR 0101](adr/0101-a-batch-of-ticks-never-carries-a-whole-election.md)).
- **A region has one core per store, and registering claims a place rather than taking one.** The
  register succeeds where the map would refuse, so an orphan is created by the refusal rather than
  by the register; the reservation is what closes that gap
  ([ADR 0099](adr/0099-one-core-per-region-per-store.md)).
- **Split:** triggered by a periodic size check on the **leader** (region > 96 MiB *default*), or by an
  operator through the Admin service (§9). The leader picks a boundary from the region's own
  data ([ADR 0012](adr/0012-split-key-selection.md) — the engine exposes no per-range key sample, so it is
  a bounded sampling scan), asks PD for one id per new region plus one per peer, and proposes
  `Split{split_key, new_region_id, new_peer_ids}` through its own group. On apply, **on every peer**: the
  parent shrinks to `[start, split_key)` and the child takes `[split_key, end)` with fresh peer ids on the
  *same stores*, both halves' `version` bumps, and both `'m'` records go into the **same batch as
  `apply_index`** — so a crash has both or neither. The child's log starts at index 0 and its group elects
  from scratch; its `conf_state` is the split-time membership, which for a log beginning at index 0 is
  exactly what `InitialState::conf_state` means. **"Elects from scratch" is what it used to do**:
  the child now starts led by the store that led the parent, which knows at apply time that it is the
  leader, that the child's membership is the parent's and that the child's log is empty — everything
  an election would have established ([ADR 0094](adr/0094-a-split-childs-leader-is-the-parents-leader.md)).
  Merge is post-v1.
  - **What "the region's data" is, for both the size and the boundary.** A region owns a range of
    the *user* key space, and each user key reaches the engine as `'r' ++ key` or as
    `'x' ++ enc(key) ++ !ts` — so a region is one engine range **per physical namespace**, in every
    family that holds data. The size sums `default` and `write` over both; the boundary scan reads
    exactly the same ranges and yields user keys, a key's versions counting once. `lock` is not
    counted (an in-flight claim is not data) and `raft` is not (this store's log, keyed by region
    id). The mapping is `esker-store/src/keyspace.rs`, shared with the snapshot stream and the
    reclaim. Reading `default` under `'r'` alone is why a SQL table occupied exactly one region
    whatever its size, at every threshold
    ([ADR 0073](adr/0073-a-regions-size-is-the-data-families-it-spans.md)).
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
  - **The store that served the stream reports how it went** (`report_snapshot`, §5 rule 6), from
    the task that walked the region — so every way out of that walk, the receiver hanging up
    included, produces an answer. Without it the leader waits on an acknowledgement the follower
    has no reason to send, and the replica is stranded for the rest of the term.
  - **Every column family the region owns**, `default`, `lock` and `write`, each chunk naming
    its own; `raft` is this store's log and metadata and is never shipped. The keys are engine
    keys, namespace byte and timestamp suffix included, and a region's user-key range maps to one
    engine range per physical namespace — `'r' ++ key` and `'x' ++ enc(key) ++ !ts`. Format
    version 1 walked `default` under `'r'` alone and so dropped every transactional record a
    region held, which is [ADR 0032](adr/0032-a-snapshot-carries-every-column-family.md).
  - **Key-value pairs, not SST files, in v1.** §6 originally described `engine.checkpoint(range)` →
    `ingest()`, and two things stop it: a checkpoint links *whole files* and a file straddles a
    region boundary, so the receiver would get its neighbour's keys; and `Db::ingest` refuses a file
    holding a key the column family already has an entry for (§4.1), so a receive retried after a
    partial one could never ingest again — the partial receive's own keys are what it would
    collide with. The bytes cross a network either way, so what is given up is one write on the
    receiver. `TODO(post-v1)`: sequence-number rewriting plus a range-clipped checkpoint makes the
    link-only transfer possible, and only `esker-store/src/snapshot.rs` changes.
  - **A snapshot is never half-visible**, which is the property everything else is arranged around.
    The receive is four durable steps — announce, stage, ingest, adopt — and nothing serves the
    region until the last. A crash leaves an announcement record naming the region, and the next
    open clears the keys that record's *range* names, so the retry finds the empty range it needs.
  - **A region this store already holds is replaced, not refused.** Refusing it was 4c's
    limitation and phase-4 acceptance showed it is not an edge case: a peer that falls behind its
    leader's compaction boundary can be repaired no other way, and until `snapshot::clear_range`
    it could not be repaired at all (`docs/plans/phase-4.md` §18). The old peer is retired first,
    so nothing is driving the region while its range is emptied and refilled, and the clear
    **verifies** the range is empty rather than assuming it — a refill over survivors would serve
    a mix of two states that looks exactly like correct data.
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
  - **An operator is a repeat by *store*, not by peer id.** PD mints a fresh peer id every time it
    issues one (§7), so a re-derived `AddPeer` naming a store this region already has a peer on is
    the same operator arriving twice and the leader proposes nothing. Asked by peer id it could
    never be recognised as a repeat, and the second change puts **two peers of one region on one
    store** — which `RegionMap::insert` refuses outright, so that peer exists in the configuration
    and nowhere else: unaddressable, never caught up, never promoted, and counted by PD as a
    replica the region no longer needs. What it is checked against is the **core's** membership as
    well as the applied record, because a conf change is in force when its entry is *appended* and
    the record moves only when it applies — and the re-derived operator arrives at the new leader
    inside exactly that window.
  - **`RemovePeer` tears down the raft state and leaves the data.** Removing the keys means point
    deletes over the range (no range tombstones in v1, ADR 0006), and the tombstones that leaves
    are keys in the range — which is exactly the state that stops the range ever receiving a
    snapshot again. The keys stay, no region covers them, and nothing serves them.
  - **A new peer is routable at the append, not at the apply.** A configuration is in force from
    the moment its entry is on disk (§4.1 of the dissertation), so the leader may address the peer
    it adds in the very `Ready` that carries the entry — a full round trip before apply moves the
    `'m'` record. The transport therefore takes `peer → store` from every conf change it persists,
    not only from the region record. Routing by the record alone drops that first message, and if
    the region's log has been compacted the message is an `InstallSnapshot`: the leader's progress
    for that peer goes to `Snapshot`, which is paused until the peer answers a snapshot it never
    received, and the replica is stranded for ever (found by `esker-store/tests/balance.rs`).
- **`TransferLeader` moves leadership, and the store refuses three cases.** PD issues it to spread
  leaders; the store checks what only it can see and drops the operator silently otherwise, because
  a refusal a scheduler cannot act on is noise it would only retry. A target the region does not
  have; a target that is a **learner**, which cannot win an election and so would leave the region
  leaderless until the old leader's timeout; and a target more than `TRANSFER_LAG_ALLOWANCE`
  entries behind the leader's own `matched`, which would campaign on a short log. The third is what
  `RawNode::progress()` exists for (§5).
- **Log compaction:** a peer throws away the head of its log once the apply index has run
  `RAFT_LOG_COMPACT_THRESHOLD` entries past the truncation point, keeping `RAFT_LOG_KEEP_ENTRIES`
  behind it so that a follower one entry behind does not need a snapshot. The record written with
  the truncation carries the term of the entry the log now begins after and **the membership as of
  that index** — not the membership in force, which a conf change above the index has already
  moved.
- **Transport:** one TCP connection per (store, store) pair carrying `RaftTransport::Batch` frames with
  `RaftMessage`s for all regions, batched per tick. Each carries the **sending store's id** as well as the
  sending peer's: a peer id is region-local, so a receiver holding no copy of the region — the case that
  asks for a snapshot, above — could not otherwise turn the sender into an address.
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

One binary; **a Raft group of up to three of them**, each with its own `esker-engine` instance
([ADR 0059](adr/0059-pd-is-a-raft-group.md), `docs/plans/phase-15-pd-ha.md`). A group of one is
what phase 4 shipped and is still the default: it wins with a quorum of itself, needs no ticks and
no transport, and behaves exactly as the single durable PD did.

Every rule below is unchanged by replication. **Only the meaning of "persisted" moved**, from one
`fsync` to a Raft entry this member has applied:

- **What goes in the log.** The four records that nothing can re-derive — the cluster's identity,
  the routing table, the allocator's reserved end and the oracle's mark — become commands applied
  by a deterministic state machine into the same key space, byte for byte. The log lives in the
  `raft` column family of the same database, so an apply writes the record and the apply index in
  **one atomic batch**.
- **What stays out of it.** The in-flight operator set, the balance cooldowns and the settling load
  deltas are memory, leader-only, re-derived from the next round of heartbeats
  ([ADR 0013](adr/0013-repair-operators-are-requests-not-commands.md)). A PD that loses leadership
  is, to the scheduler, a PD that restarted.
- **The leader samples the clock; the state machine never does.** `apply` is a pure function of
  `(applied state, command)`, so every `now_ms` travels *inside* the command. A member that read a
  clock inside `apply` would diverge from its neighbours, and would be a second place in Esker that
  orders on a wall clock (invariant 6).
- **A new leader answers nothing until it has applied a `TakeOffice` entry of its own term.** Raft
  promises a new leader's *log* holds every committed entry and says nothing about `applied`, and
  the oracle is rebuilt out of applied state. Then, and only then, it reloads
  `Allocator::load(allocated_end)` and `Oracle::load(high_water, now_ms)` — the same two
  constructors a restart uses, because a failover is a restart that kept its socket.
- **The mark is the lease, and the reservation is the same lease for ids.** A leader that has lost
  its quorum and not noticed is confined *below* the mark: crossing it needs a commit its proposals
  no longer earn, so the call fails rather than answering. A new leader begins at
  `max(clock, mark)`, which is at or above it. Nothing in that argument needs either clock to be
  right, or the two to agree — which is why PD needs no lease of its own.
- **Only the leader serves; a follower answers `PdNotLeader` with an address.** A separate wire code
  from the region-scoped `NotLeader`, because a client answers that one by repairing its region
  cache and a PD redirect would poison an entry for a region that does not exist. Reads are
  leader-only too — a follower would otherwise answer a routing question out of whatever it had
  applied — with two exceptions that are questions about *this process* rather than about the
  cluster: `Status` and `Members`.
- **Reads do not take a `ReadIndex`.** A deposed leader can answer a routing question one entry
  behind, which is exactly the staleness this section already designs for: a client's cache is a
  hint the store checks against its epoch, so a stale route costs a redirect and never a wrong
  answer (invariant 5). The two answers that *cannot* be stale — an id and a timestamp — are the
  two that go through the log.
- **A group is named once, and then it can grow.** The group's id is `mix64` over the member list
  it was **founded** with, derived one time and written into each member's own state record; a
  member refuses a Raft batch that does not carry it, so two clusters' placement drivers pointed at
  each other by a stale flag cannot form one group and replicate one cluster's routing table over
  the other's. Deriving it *again* on every start is what ADR 0059 did and what
  [ADR 0061](adr/0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md) had to change
  before membership could move at all: the derivation changes when a member is added, so a group
  that recomputed it would partition itself at the moment it grew. A joining member is *told* the
  id; a member whose record and whose `--peers` disagree believes the record, which is
  `esker-raft`'s own rule for membership.
- **Membership changes are single-server, and a member joins as a learner.** `esker pd members add`
  is a **reconciliation** rather than a script — propose a learner, wait for it to catch up,
  promote it — so a `kill -9` anywhere in it is recovered by running the same command again. The
  address rides in the conf change's own `context`, so the voting set and the address book cannot
  disagree about whether a change happened, and the route is learned at *append*, in the same
  `Ready`, because a configuration is in force from the moment its entry is on disk.

  The order is what an operator runs under pressure, and it is forced by arithmetic. Three members
  with one gone is a quorum of 2 with two live; `AddVoter` would take the quorum to 3 the instant
  its entry was appended, and the entry that made it 3 would need 3 to commit — the group stops,
  and undoing needs the quorum it no longer has. A learner is not counted in a quorum, so it
  commits, catches up by snapshot, and only then is promoted. Then the dead member goes. Removing
  first would work and is worse: it leaves a quorum of 2 out of 2 until the replacement lands.
  **Add before remove**, the same sentence [ADR 0013](adr/0013-repair-operators-are-requests-not-commands.md)
  uses about region replicas.

  A removal is refused when it would leave the group without a quorum of members PD has heard from,
  and refused for the last member. Nothing *schedules* one: PD repairs region replicas because it
  can watch a store go quiet, and there is no equivalent observer for its own group.

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
  when the wall clock jumps backwards. This is the only place in the cluster that reads a wall clock,
  and it reads it through an injected `Clock` so that a test can make it misbehave. Its single-process
  stand-in — `esker-sql`'s `MemoryBackend` oracle, which the scoreboard node runs on — is the one
  other reader, and applies the same `max(last, clock)` rule to one process (`Versions::mark` in
  `crates/esker-sql/src/backend/mod.rs`).
- **Liveness:** a store is down when PD has not heard from it for `max_store_down_time`. The age is on
  **PD's clock** — the last-heartbeat stamp is written when the beat arrives and never taken from the
  store's own report, because comparing wall clocks across nodes is what invariant 6 forbids. A store PD
  has no record of is *not* down: unknown is not evidence, and a PD that read it as one would try to
  repair every region in the cluster on its first heartbeat after a restart.
- **Replica repair (4c):** every region with fewer than `target_replicas` **live** replicas gets an
  `AddPeer` onto the emptiest live store that has no peer of that region — emptiest by *effective*
  region count, with the lowest store id breaking the tie, so the same data always chooses the same
  place and a round of repairs spreads instead of piling onto one store. Once the new replica is a
  voter, a peer on a down store gets a `RemovePeer`. **Add before remove**, always: removing first takes
  a three-replica region with one dead peer down to one live replica out of two. "Back at the target" is
  counted in **voters**, because a learner is not in the configuration that votes: dropping the dead
  peer while the replacement is still catching up is the same mistake made one step later, and it is the
  one the phase-4 retest hit once an `AddPeer` had timed out and been re-derived.
- **A death is only one way to be short.** The trigger is the *state*, not the event: a region that
  never finished growing to its target is repaired exactly like one whose store died. The older,
  death-scoped rule — which left a merely under-replicated region alone, on the reasoning that growing
  a healthy cluster was balance's job — was priced by phase 4's acceptance run: region 27 sat at two
  voters with every store alive, and when one of those two died the region could no longer commit even
  its own repair (`docs/bench/phase-4.md` Run 4). Regions are ordered by urgency — **exactly a quorum
  of live voters first**, because that is the one a single further failure ends — for any caller that
  sees more than one at a time; PD's own path decides one region on its own heartbeat and is therefore
  ordered by arrival.
- **Past the quorum boundary, repair stops and an operator starts.** A region with fewer live voters
  than a quorum cannot commit the membership change that would save it, stops electing a leader, and so
  stops beating to PD at all — PD cannot even be *told* to fix it, since operators ride on heartbeats.
  PD never forces a configuration on its own: "down" means silent to PD, not gone, and a forced
  configuration on that evidence is two groups serving one range. The way out is a future
  operator-invoked `esker region unsafe-recover`, lossy by construction and outside the heartbeat path
  ([ADR 0026](adr/0026-the-quorum-loss-boundary.md)).
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
  decided before leader count, because moving a replica takes any leadership of that region with it. A
  replica move is add-then-remove; the replica that goes is the one on the busiest store, and when that
  is the leader's the office is transferred first. **A move already begun always finishes**: while it is
  half done the region sits on two stores and is counted on both, so a stranded move corrupts the numbers
  every later decision uses — which is why at most `max_balance_operators` moves are *started* at once,
  and why neither that cap nor the per-region `balance_cooldown` may pause a move in progress. Repair is
  subject to neither. Balance can be switched off with repair left on.
- **A finished operator's effect is still corrected for until the stores say so themselves.** Stores
  report every `store_heartbeat` interval and PD issues operators between two of them, so a move that
  landed a moment ago is in neither the in-flight set nor the report — and every region deciding in that
  window reads the busy store at its full, unmoved count. Sixteen regions each making that reading is a
  **sweep**, which the spread threshold cannot see: every one of those moves strictly reduces the spread,
  and sixteen in a row still empty a store, which is what the phase-4 retest measured. So a retired
  operator's `LoadDelta` is held until every store it names has reported since, and dropped after
  `max_store_down_time` regardless ([ADR 0023](adr/0023-a-retired-operators-load-outlives-it.md)).
- **Balance never touches a region that is mid-repair**, and a region is mid-repair while it holds a
  peer on a **down store** *or* a plain **`Learner`**. A region can be over its replica target for two
  reasons — a balance move has landed, or repair has put a replacement beside a peer on a down store —
  and only the first is balance's to finish. Told apart by the state: a peer on a down store means the
  second, and then the peer that goes is the dead one and repair alone says when the region can afford
  to lose it. Without this, balance shed a *healthy* replica from a region under repair and repair had
  to put one back on the same store, which is two of the five membership changes a two-change repair
  spent in the retest.

  The learner half is the same rule reaching the state repair passes *through* rather than the state
  it starts from, and it costs more than an extra membership change. Repair and balance both add
  before they remove, and the peer they add is a learner until the **leader** promotes it — on the
  learner's `matched`, because a region heartbeat comes only from a leader and PD can therefore never
  see a learner's progress. A leader with a leadership transfer in progress *refuses proposals*, so a
  `TransferLeader` issued against a region whose learner has not been promoted blocks the very
  `AddVoter` that would finish the repair: the transfer times out, PD re-derives it, and the region can
  stay that way indefinitely. A `ColumnarLearner` is excluded, because ADR 0022 says it is never
  promoted and counting it would freeze such a region out of balance for ever.
- **Operator history.** The last 64 operator events — issued, done, cancelled, timed out — are kept in
  one bounded record on disk, so `esker pd inspect` can say what PD asked a cluster to do after the
  process is gone. A debugging record only: no decision reads it, and losing it costs an explanation
  rather than a repair.
- **Tools:** `esker pd serve --data-dir --listen` runs one; `--id` and `--peers` make it a member
  of a group being founded, and `--join` makes it one joining a group that already exists —
  a member added at run time cannot derive the group's id, so it asks. `esker pd members add
  ID@ADDR` and `remove ID` change the membership, one step per round trip. `esker pd inspect --data-dir` prints the whole state above — including the range
  index beside the records it points at, and the durable half of consensus — and it **creates
  nothing**: opening a placement driver campaigns, and a campaign is a write, so the inspector is a
  read-only view that names no column family and starts no driver. `esker pd members --pd` asks a
  **running** member who is in its group and which one leads, and is answered by a follower too,
  which is the point: it is what an operator reaches for when the leader is what is missing.

## 8. Transactions (`esker-txn`)

Percolator, optimistic, snapshot isolation (the TiKV model). Three CFs:

**Three isolation levels, not one** ([ADR 0057](adr/0057-read-committed-waits-for-the-writer-in-front-of-it.md), [ADR 0062](adr/0062-serializable-is-snapshot-isolation-plus-a-validated-read-set.md)):
`REPEATABLE READ` is the snapshot this section describes, and `READ COMMITTED` — PostgreSQL's
default, and so the one `ActiveRecord` runs in — takes a **new** snapshot per statement and *waits*
for the writer in front of it rather than answering `40001`. The three CFs and the protocol below
are the same either way; what the level changes is which snapshot a statement reads at and what a
conflict does. `SERIALIZABLE` is the third: snapshot isolation **plus a validated read set**, so a
transaction that read a row another transaction then wrote is refused at `COMMIT` rather than
committing on a snapshot that never existed as a serial order. **A unique index entry is locked
like a row at `READ COMMITTED`** ([ADR 0114](adr/0114-a-unique-key-being-written-waits-at-read-committed.md)):
a second writer of the same value on one node waits for the first and re-reads what it left — a
`23505` from its own statement if the first committed, nothing if it rolled back — where it used to
read the key as absent and meet the first writer at commit. At the two levels that keep their
snapshot, and between two nodes, that meeting is still at prewrite, and its loser is told
PostgreSQL's code: `40001` rather than `23505` at `SERIALIZABLE` when it had read the key before
writing it, and at `REPEATABLE READ` as well when `ON CONFLICT`'s arbiter had read it
([ADR 0114](adr/0114-a-unique-key-being-written-waits-at-read-committed.md) §3) — at `COMMIT`, one
statement later than PostgreSQL.


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
primary is committed, rolled back if its TTL expired, else wait/backoff. The TTL **is** extended, since
[ADR 0088](adr/0088-a-row-lock-across-nodes.md): a client renews the lease of every transaction
holding a lock at a **third** of it (`esker-client`'s `renew` module, the cadence ADR 0028 settled
for the schema lease), and stops the moment the transaction ends — commit, rollback, or being
dropped. So a **live** holder keeps its rows however long it holds them, and a **dead** one still
outlives its lock by at most one TTL, which is what the short lease is for. Before that sender
existed the handler had nobody calling it, and a `SELECT … FOR UPDATE` held past three seconds lost
its row to the next session that wanted it.
**A row lock is not one of these yet** ([ADR 0088](adr/0088-a-row-lock-across-nodes.md), accepted
2026-09-09): `SELECT … FOR UPDATE` takes a lock in the `esker-sql` node's own table, so today it
excludes other sessions of the same node and **not** sessions of another node — two nodes given the
crossed sequence that deadlocks one node both commit, measured. The accepted fix is (a'): the row
lock becomes a Percolator lock, acquired by an ordinary prewrite of a `Check` mutation (tag 5) sent
when the statement runs rather than at `COMMIT`, which is why it costs no new tag and no new
method. GC: PD publishes a safepoint
([ADR 0110](adr/0110-who-publishes-the-garbage-collection-safepoint.md)); a rising one makes a store
go and compact (§4.7); a `CompactionFilter` drops versions below it, keeping the newest visible one
— **including when that newest one is a delete**, which is what
[ADR 0111](adr/0111-a-deleted-keys-versions-are-dropped-as-one-segment.md) is about.

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

**This wire can be encrypted, and it still authorises nobody.** `connect_with_tls` and
`Server::with_tls` put a TLS session under the framing, behind the same default-off `tls` feature
the other two surfaces use ([ADR 0055](adr/0055-the-tls-options-across-three-surfaces-measured.md));
`--rpc-tls-cert/-key/-ca` on `esker server` and `esker pd serve` turn it on, and `--rpc-tls-mutual`
adds client certificates in both directions for the links where both ends are ours — store↔store
and PD↔PD. Encryption here is **required, not offered**: there is no in-band upgrade like the
PostgreSQL port's `SSLRequest`, so a server with TLS on refuses a peer that arrives in the clear.
A node given an incomplete set of flags refuses to start rather than serving unencrypted on a port
an operator believes is protected.
What has *not* changed is who may say what. A request carries a region epoch and a cluster id,
neither of which is a credential, and a verified certificate only says the peer holds a key this
cluster's CA vouched for. Mapping that identity to "may register as store 7" or "may vote in
region 4" is application logic `esker-pd` does not have, so a `StoreHeartbeat` or a
`RaftTransport::Batch` from any peer the CA signed is still accepted on its own say-so. mTLS makes
the identity available to check; the check is the work that remains.

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
of `Stream` frames rather than a `Response` (§6) — and `Admin { Split, TransferLeader, Regions }`,
which is the operator's door rather than a client's: `esker-cli region` resolves a region through PD
and then asks its leader directly, so a human can force a split or move a leader without waiting for
a scheduler to decide the same thing. `Regions` is what `region ls` reads, and it answers for the
regions **this store hosts**, leader flag and applied index included. Every KV request carries `{ region_id, epoch, peer }` and every error is a
typed enum with redirect hints (`NotLeader{leader_hint}`, `EpochNotMatch{current_regions}`,
`KeyNotInRegion`, `ServerIsBusy`, `Locked{lock_info}`). A `Pd` request carries the **cluster id** in
place of the region header, since PD's answers are about the routing table rather than about a region,
and `Bootstrap` may send zero because asking is how a caller learns it; PD's own two refusals are
`NotBootstrapped` and `ClusterMismatch{expected, actual}`, and neither is retryable. Unknown methods and fields are errors, not
ignored — forward compatibility is handled by `WIRE_VERSION` negotiation on connect.

**What a `Scan` promises, and what it does not** (#79). One answer is bounded three ways: the
caller's `limit`, the store's own cap on a batch, and a **byte budget the caller cannot see**
(`esker_store::txnkv::MAX_SCAN_BYTES`, four megabytes, so that a response always fits a frame). A
`limit` of zero has always meant *"as many as the server will give"* rather than zero pairs. So a
batch holding fewer pairs than the caller asked for says **nothing at all** about whether the range
is finished, and a client that reads it as "that is all there is" gets a silent subset of the range
with the same type and the same `Ok` as a complete answer. That is not a hypothetical: it cost run
127 attempt 4 five catalog table records that were on disk and readable the whole time.

The contract is therefore:

- **Only an empty batch ends a range.** A client scanning a range pages: it asks, and while the
  answer is non-empty it asks again from the immediate successor of the last key it was given,
  across a region boundary as readily as inside one. `esker_client::Transaction::scan` and
  `RawClient::scan` both do this, and a `limit` of zero means every pair in the range.
- **A store never answers empty while a live key remains.** Both `txnkv::scan` and `rawkv::scan`
  check the byte budget *after* pushing a pair, so one live key always produces one pair. Without
  that half the rule above would end a scan on the first oversized row.
- **What a store's cap counts is pairs it is returning**, never work it did to find them. The
  version records of deleted keys are the case that made this a defect rather than a detail: a
  catalog range with 8,064 dropped tables and 278 live ones exhausted a ceiling of 8,192 *keys*
  while the answer held 278 *pairs*, so every number the caller could see said the answer was
  complete.
- **A scan that cannot finish fails loudly.** Routing that does not advance, a range that outlasts
  `MAX_SCAN_CALLS`, or one that needs more pieces than `MAX_SCAN_REGIONS` is an error and not a
  short answer. `RawClient::scan` used to enumerate its regions up front and simply stop at the
  budget, answering a plan that covered a prefix of the range.
- **A region that split under a scan is still that scan's to finish.** A store's refusal names the
  narrower range it now owns, and the walk carries on from the bound it was actually served rather
  than from the plan it made before sending anything. `Transaction::scan` does this by answering
  the boundary it used; `RawClient::scan` does it by putting the unserved half back on its queue,
  ahead of the pieces below it when the walk is descending.

The alternative considered and not taken is a **resumption cursor** on the wire: the store says
where it stopped and whether more remains, which costs a field and buys back the one empty round
trip per scan that the rule above spends. It is the better design if that round trip ever measures;
it is a wire change and this is not.

Method numbers are `service:method`, so a service's numbers stay contiguous and one can be reserved
before it is written: `0x00` system (`0x0001` Hello), `0x01` RawKv (`0x0101`–`0x0108`, in the order
listed above), `0x03` Pd (`0x0301`–`0x0306`, in the order listed above), `0x04` RaftTransport
(`0x0401` Batch, `0x0402` Snapshot) and `0x05` Admin (`0x0501`–`0x0503`, in the order listed above),
with `0x02` TxnKv reserved. `Hello`'s layout is
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
  *default*), whichever ends first. The budget counts **failures, not attempts**: a refusal that
  moved the region's epoch taught the client something and resets it, because a request that keeps
  being redirected to fresher routing is making progress, and spending a budget on progress is how
  a client gives up on a region that is merely splitting. Only an epoch change resets it — a
  `NotLeader` hint moves no epoch, and chasing leadership around an unchanging region is the loop
  the budget exists to stop. Which errors are retryable is `ProtoError::is_retryable()` —
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
`sst-dump`, `wal-dump`, `manifest-dump`, `region ls`, `region split`, `region transfer-leader`, `bench`.

## 13. The SQL surface, and the serverless hooks still open

**Three of the five below are built and two are hooks**, which the section title used to hide: SST
tiering, TLS and `esker-sql` describe what exists and are maintained against the code; scale-to-zero
and per-tenant quotas are the roadmap this section was originally all of. Each bullet says which it
is in its first sentence.


- **SST tiering — built (phase 6b).** All SST reads go through the `FileSystem` trait; `LocalFileSystem`
  and `fs::tier::TieredFileSystem` are the two implementations. The tiered one routes by file *kind*:
  only `NNNNNN.sst` reaches object storage, and the WAL, the manifest and `CURRENT` stay local, because
  they are the log and invariant 1 is a statement about local durability. **The database directory is the
  cache**: a resident tiered SST is simply the local file, evicting it is deleting that file, and
  refilling it is fetching the object back to the same path — so the engine is never told which of its
  files are resident. A cold read is *ranged*, one `GetObject` per block touched, with the block cache in
  front; a whole-file fetch to answer a point read would be a three-orders-of-magnitude amplification.
  An upload happens **after** the manifest edit that names the file, never before, and an upload that
  fails fails nothing else (ADR 0024). An object is deleted only when no live version names its number
  and it is not a pending output — taken from the version set rather than a directory listing, because an
  evicted file is absent from the listing. The S3 client (`esker-s3`) is in-house: `PutObject`, ranged
  `GetObject`, `ListObjectsV2`, `DeleteObject`, SigV4 over `esker-base`'s SHA-256/HMAC, and an HTTP/1.1
  codec, with **no external dependency at any depth**. A prefix is **claimed**: the first database to
  open one writes a versioned, CRC'd marker naming itself, and any other database is refused at startup
  with both identities in the message, because two databases sharing a prefix overwrite each other's
  `000007.sst` in silence — file numbers restart at one in every database. Identity is a random id kept
  in the database's own directory rather than `(cluster_id, store_id)`, which two benchmarks do not have
  and two misconfigured stores share (ADR 0029). A prefix holding objects and no marker — every pre-6c
  prefix — is refused until `--adopt-sst-store` says otherwise; nothing is ever adopted silently.
- **TLS — built on the PostgreSQL port, deferred on the other two
  ([ADR 0055](adr/0055-the-tls-options-across-three-surfaces-measured.md), accepted 2026-09-04).**
  All three surfaces are **one dependency decision, not three**, and it has been taken: `rustls` with
  `rustls-graviola` as the `CryptoProvider`, behind `esker-sql`'s **`tls` feature, off by default**.
  graviola was chosen on measurement — `rustls-rustcrypto`, the better-known pure-Rust provider,
  fails this repo's own `cargo deny check` today. With the feature off the runtime graph does not
  move by a crate; with it on it grows by nine.
  **The PG port**: `SSLRequest` is answered `S` when the node holds a `--tls-cert`/`--tls-key` pair,
  and the whole session including the client's real startup packet runs inside TLS records; a node
  with no certificate answers `N` and carries on in the clear, and a node given a certificate it
  cannot serve — the flags without the feature — **refuses to start** rather than serving plaintext
  on a port an operator believes is encrypted. `pgwire::tls` is the only module that knows what TLS
  is; the session is driven by a task over a duplex pipe rather than a hand-written poll adapter,
  because the adapter is a third crate or a class of hang.
  **S3 — built, and it cost no further crates.** `esker_s3::tls` is the second implementor the
  transport trait was designed for (ADR 0025 decision 1, which needed no change), and
  `Endpoint::parse` now accepts `https://` under the feature and refuses it without, naming the
  feature. The root store is **not** `webpki-roots`: the host's CA bundle is a file, `SSL_CERT_FILE`
  overrides it and `ESKER_S3_CA_CERT` names one for a self-signed `MinIO`, so the vendored-roots
  crate and its `CDLA-Permissive-2.0` licence line are both avoided and the exception stays at nine
  crates for both surfaces. Its connection pool absorbs post-handshake messages rather than peeking
  at the socket, because a TLS 1.3 server sends tickets and key updates whenever it likes and the
  plain transport's check would throw away a good connection for them — silently, since every
  response stays correct. `esker-s3` costs nothing with the feature off, as it always has.
  **The RPC layer (§9) is what is left.** It carries no TLS and no peer authentication, and ADR 0055
  says why the second is the larger half of that work: mTLS makes a peer identity available, and
  nothing in `esker-pd` yet decides what a given identity is *allowed* to be.
- **Stateless SQL nodes (`esker-sql`) — built (phase 6).** In-house PostgreSQL wire protocol v3 (startup, simple and
  extended query, `psql` compatibility — ~3.5k lines across five modules, no `pgwire` crate) → SQL parser (the one expected
  large dependency exception, `sqlparser`, PostgreSQL dialect, by ADR) → catalog in `'m'` key space →
  planner/executor over `esker-client` transactions, using the `'t'` key layout from §3. Postgres
  compatibility is a surface, not a storage format.
  **A statement reads the catalog once, not once per relation it names**
  ([ADR 0106](adr/0106-what-a-statement-reads-below-the-sql.md)): `catalog::Catalog` is a per-node
  cache of definitions keyed on the summed catalog version, and every reader goes through the
  `catalog::View` a statement opens — names, tables, schemas, views, user types, a table's sequences,
  and the whole `pg_relations::Relations` bundle the `pg_catalog` and `information_schema` views are
  built from. **The version counter is the whole of the invalidation**: a view whose version has moved
  is refused the cache, and a transaction that has *written* the catalog is given a view that neither
  reads nor fills it, so it sees its own uncommitted DDL and publishes none of it. Measured
  ([`docs/bench/statement-reads.md`](bench/statement-reads.md)): a repeated catalog introspection
  went from **60 KV reads to 4**, which are the two catalog views a statement opens and their two
  version counters — the read [ADR 0105](adr/0105-a-catalog-read-never-waits.md) is about, and the
  one that must never wait.
  **A subquery is a plan node** ([ADR 0043](adr/0043-a-subquery-is-a-plan-node-run-once-or-per-row.md)):
  one that names nothing outside itself runs **once**, before the cursor opens — the same pass shape
  that fills a `Node::Columnar` from its fragments — and one that names an outer column runs **once per
  outer row**, with the outer values substituted into a copy of the sub-plan first, so what a cursor
  opens has no correlated reference left in it. `FROM (SELECT …) AS t` becomes a synthetic `TableDef`
  under a reserved relation id, so name resolution, `SELECT *`, `EXPLAIN` and the join machinery are
  unchanged by it; a non-recursive CTE is **inlined at each reference** and is therefore one of those,
  which costs no executor at all and no observable difference — `MATERIALIZED` and `NOT MATERIALIZED`
  return the same rows, measured.
  **A view is a stored `SELECT` expanded where it is read, and a materialized view is a table whose
  rows are recomputed** ([ADR 0064](adr/0064-a-materialized-view-is-a-table-whose-rows-are-recomputed.md)):
  `CREATE MATERIALIZED VIEW` plans its definition once, stores the columns that plan named and the
  rows it produced, and `REFRESH` replaces those rows inside the caller's transaction. It is a table
  underneath, so it has `pg_attribute` rows, takes indexes and rolls back — and the refusals that
  keep it read-only (`42809` on a write, `55000` on a read of one created `WITH NO DATA`) are the
  only thing between a client and a writable one. A view publishes its columns to `pg_attribute`
  too, resolved when it is created rather than at each read. Every one of them is bounded where `Sort` is (`53400`), and none of
  them routes to the columnar engine.
  **The type surface is a list of decisions, not a lattice**, and each one is an ADR because each
  was measured against a real server rather than derived: `name` is a stored type whose tag is
  additive ([ADR 0084](adr/0084-name-is-a-stored-type-and-its-tag-is-additive.md)), `void` is a type
  and the first pseudo-type in the vocabulary
  ([ADR 0092](adr/0092-void-is-a-type-and-it-is-the-first-pseudo-type.md)), and the five geometric
  shapes have their arrays ([ADR 0091](adr/0091-the-five-geometric-shapes-get-their-arrays.md)).
  A **literal** takes the narrowest type that holds it
  ([ADR 0087](adr/0087-an-integer-literal-is-the-narrowest-type-that-holds-it.md)) and a decimal one
  is a `numeric` ([ADR 0089](adr/0089-a-decimal-literal-is-a-numeric.md)); a **folded cast keeps the
  type it named** ([ADR 0086](adr/0086-a-folded-cast-keeps-the-type-it-named.md)), which is what lets
  the printer recover the tree it came from; and `pg_typeof` answers the **declared** type rather
  than reading the datum, because several types share one
  ([ADR 0093](adr/0093-pg_typeof-is-resolved-against-the-declared-type.md)).
  **A stored expression is deparsed by the statement that writes it**, once, rather than by each of
  the six readers that print one
  ([ADR 0090](adr/0090-a-stored-expression-is-deparsed-by-the-statement-that-writes-it.md)) — a
  generated column, a `DEFAULT`, an index key, a `CHECK`, `pg_get_indexdef` and
  `pg_get_expr(indexprs)` all read the same text out of the catalog.
  **`pg_catalog` and `information_schema` are computed relations**
  ([ADR 0044](adr/0044-a-catalog-relation-is-computed-and-its-oid-is-the-record-s-id.md)):
  **thirty-nine** views — thirty-two in `pg_catalog` and seven in `information_schema` — over the same `'m'`-space
  records the planner already reads, materialised per query, with no second store to keep in step.
  The count is `CatalogView::ALL`, which a test walks so that none is added without one. Their oids are the ids those records already carry — a table's, an
  index's, a sequence's — from **one** snapshot read once per statement and bounded like every other
  scan (`53400`), because every statement a schema dump sends is an oid join and two views computing
  one independently is how they silently stop joining. The two relations with no record of their own,
  a primary-key constraint and a `NOT NULL`, derive theirs reversibly from the table and the column.
  A `pg_catalog` function is resolved where its argument allows: `'x'::regclass` before the plan is
  built, `pg_get_indexdef(d.indexrelid)` per row against a snapshot the cursor holds. Every write is
  `42501` but one — `pg_constraint.convalidated`, below — and a type this node has no value for is provided where the client reads it as text
  (`pg_index.indkey`, an `int2vector` there and text here, printed the same and subscripted from
  zero). `pg_constraint.conkey` was the refusal beside it and is a `smallint[]` now, which is what
  it is on a real server.
  **A `DO` block and a trigger function are PL/pgSQL, and the subset is a census**
  ([ADR 0113](adr/0113-plpgsql-is-the-subset-the-suite-sends.md)): `crate::plpgsql` reads a body
  whole — declarations, `IF`, `RAISE` of a literal, `SELECT … INTO`, assignment,
  `FOR <record> IN <query> LOOP`, `EXECUTE`, `RETURN` and any SQL statement — refuses every other
  construct by name before any of it runs, and answers PostgreSQL's own sentence for a body
  PostgreSQL refuses. `exec::plpgsql` runs it **inside the statement that reached it**: every SQL
  statement goes through `run_recording` with that statement's transaction and savepoint, a
  variable reaches SQL as a typed literal put into the lowered tree rather than as text, and a name
  that is both a variable and a column is `42702`. Nesting is bounded (`54001`). **A row trigger
  fires from `exec::trigger`**: every enabled `BEFORE ROW` trigger in name order, after defaults and
  before generated columns and checks, `RETURN NULL` taking the row out of the statement; the
  `AFTER ROW` triggers once the statement has written its rows; a foreign key's actions firing the
  child's. A statement-level trigger, arguments, a partitioned table and `ON CONFLICT` into a table
  with a trigger are refused by name. The one write to a system catalog lives beside it: `UPDATE pg_catalog.pg_constraint SET convalidated = …` writes the
  `NOT VALID` flag a foreign key or a `CHECK` already stores (`exec::catalog_write`), because
  `check_all_foreign_keys_valid!` cannot work without it, and every other catalog write is `42501`.
  **The catalog record is a versioned on-disk format** like every other byte this system writes
  (invariant 2): `catalog::record::CATALOG_FORMAT_VERSION` is **36**, a record carries it in its
  first byte, and every field added since version 2 is read behind a `reader.version >= N` guard so
  an older record still decodes. **A record's *contents* are what that number versions, and a
  second one versions the key layout**: `CATALOG_LAYOUT_VERSION` is a database-level marker at a
  reserved key, and a database written before index names became schema-scoped is refused with one
  sentence rather than misread ([ADR 0080](adr/0080-an-index-name-record-is-scoped-to-its-schema.md)). Goldens in `catalog::tests` pin the bytes. A field is appended —
  at the end of the record, or beside the item it belongs to when the reader already walks that
  list — and a version is claimed by the lane that takes it, out loud, because two lanes have
  collided on the number twice.
  **A node's authority to write is a lease from PD, and its cadence is a deadline rather than a
  delay** ([ADR 0028](adr/0028-the-schema-lease.md)). The refresher renews at a third of the lease
  — derived from PD's number, never configured, so a lost round trip still leaves two attempts —
  and sleeps until `round_start + period`, so what a renewal costs comes out of the wait instead of
  being added to it. The columnar report keeps its **own thread and its own cadence**: its PD half
  carries a deadline but its backend half is a cluster read whose duration is unbounded, and while
  the two shared a loop a slow report made the next renewal late. Measured on a real cluster:
  `renew_ms=0`, `report_ms=3879` against a `period_ms=1666` on a 5 s lease, putting the next
  renewal 5,545 ms after the last — the node's first `INSERT` came back `25006` while `SELECT 1`
  kept working, which is why an expired write lease presents as a client problem. The two halves
  have different failure semantics, which is the reason they are apart: a lost report is repaired
  by the next one, because `columnar_wishes` is a full assertion and not a delta (ADR 0022
  decision 5), and a lost renewal stops this node writing. Every recorded lease is measured, the
  startup grant included — a renewal landing more than half a lease after the previous one logs a
  warning with both durations. Half rather than the whole, because at the whole the client already
  has the error.
  **The DDL surface is wider than the planner's**, and three decisions in it are worth following
  from here: a dropped column keeps its slot because a row is decoded by position
  ([ADR 0051](adr/0051-a-dropped-column-keeps-its-slot.md)); `DO` is two recognised templates and
  not a PL/pgSQL engine ([ADR 0058](adr/0058-a-do-block-is-two-templates-not-a-language.md)); and a
  `USING` clause on `ALTER COLUMN … TYPE` was a **licence** while there was no per-row evaluator
  to run one with ([ADR 0060](adr/0060-a-using-clause-is-a-licence-not-an-expression.md)), and is
  an **expression** now: lowered like any other and evaluated per row against the row as it was
  before the change (`tests/alter_column_type_using.rs`), with a plain cast still costing what the
  licence did.
  **A row lock is a node's own, until ADR 0088 is built**
  ([ADR 0088](adr/0088-a-row-lock-across-nodes.md), accepted 2026-09-09). `SELECT … FOR UPDATE`,
  `NOWAIT`, `SKIP LOCKED`, `lock_timeout` and the `40P01` a cycle earns are all real and all
  measured against PostgreSQL 19 — **within one node**. Across two nodes the lock is invisible:
  measured on a real cluster, two sessions on two nodes given the crossed sequence that costs one
  node a `40P01` and one victim both wrote and both committed, and `pg_locks` on the first node was
  empty. Stateless is what makes that possible — the lock lives in the process, and there are many
  processes — so the fix puts the lock where the row is.
- **Scale-to-zero:** because SQL nodes are stateless and SSTs can live in object storage, an idle tenant
  costs only its Raft metadata; PD may later hibernate cold regions (ADR).
- **Multi-tenancy:** tenant id is the first field of every SQL key; RawKV/TxnKV users may adopt the same
  convention. Per-tenant quotas are a PD concern, out of v1 scope.

## 14. Defaults (one place)

| Knob | Default |
|---|---|
| WAL block | 32 KiB (fixed) |
| memtable | 64 MiB, max 4 immutables |
| SST data block / restart interval | 4 KiB local, 16 KiB tiered / 16 |
| bloom | 10 bits/key, prefix-extracted for versioned CFs |
| L0 trigger / slowdown / stop | 4 / 8 / 12 files |
| L1 base / multiplier / levels | 64 MiB / 10 / 7 |
| compaction output file size | 8 MiB |
| block cache | 256 MiB, 8 shards |
| open SST readers | 256, evicted least-recently-used |
| SST tier local budget | 4 GiB (`TierOptions::local_budget`); `None` never evicts. Only *uploaded* files are ever candidates |
| SST tier upload batch | 8 distinct files per maintenance pass — which is also the retry backoff (ADR 0024) |
| SST tier idle tick | 5 s; the uploader also wakes whenever an SST becomes durable |
| manifest roll size | 64 MiB |
| compaction threads | 2 |
| region split size | 96 MiB |
| raft tick / election / heartbeat | 100 ms / 10–20 ticks / 2 ticks |
| max inflight raft msgs | 256 |
| store / region heartbeat | 10 s / 60 s — the region interval is also the latency of a PD operator, which has no other way to reach a store |
| raft log compact threshold / tail kept | 4096 / 1024 entries |
| snapshot offer timeout | 100 ticks (10 s) — one store-heartbeat interval; the driver's report is the fast path |
| snapshot chunk / stream depth | 1 MiB / 4 chunks |
| driver workers per store | 4 threads; region `id % workers` picks one |
| leadership-transfer lag allowance | 64 entries behind the leader's `matched` |
| PD operator timeout (store side) | 5 s |
| PD id allocation batch | 1,000 ids per persist |
| PD TSO save interval | 3 s ahead of what is handed out |
| `max_store_down_time` | 30 s |
| `operator_timeout` | 300 s, measured from the last observed progress |
| `target_replicas` | 3 |
| `balance_cooldown` | 300 s per region, after an operator retires |
| `max_balance_operators` | 4 moves started at once (finishing a move is never capped) |
| leader / region spread threshold | 2 (a constant, not a knob — see ADR 0018) |
| PD operator history | 64 events |
| txn lock TTL | 3 s — the `TxnKv::Heartbeat` that would extend it has no sender (§8), so a dead holder's lock lapses in at most 3 s |
| transport (`TransportConfig`) | §9 has the table — seven knobs, listed there because each one only means something next to the rule it bounds |
| store WAL sync mode | `Never` — the engine adds no `fsync` of its own, so each request's `sync` flag decides (§4.2, and `CLAUDE.md` invariant 1's opt-out) |

## 15. Open questions (turn into ADRs as they are decided)

Joint consensus vs single-server changes only — live for the **store's** groups, where a scheduler
issues the changes and might one day want to move two peers together · separate Raft log store ·
async commit / 1PC · leader leases vs ReadIndex only · secondary-index encoding for composite keys ·
how much Postgres surface for the first SQL milestone.

**The catalog's region is on the path of every statement, and nothing about its read path is
decided.** The catalog lives under `'m'` (§3) and every SQL key under `'x'`, so the catalog sorts
below all data and stays in the **left-most region for the life of the cluster** — while
`Catalog::view` reads `catalog_version` from the store **once per transaction**. One region is
therefore read by every statement on every node, and a moment when it has no leader is a moment the
whole node is refusing. Three shapes to choose between when this is decided: a **cached** version
with an invalidation the store pushes, a **lease read** that a follower may answer, or a **split
exemption** that keeps the catalog in a region nothing else can make busy. Measured evidence for
why it matters is `docs/plans/debts-v1.1.md` #34, and the three shapes are written out with what
each costs in [ADR 0102](adr/0102-the-catalogs-read-path.md) — **a draft for a milestone
conversation, not a decision**, so this question stays open and now has its options on paper.

*Answered by being refuted:* whether a client should wait longer for a region that is **between
leaders** — a count of retries rather than the caller's deadline. It should not, and nothing in the
client changes: at these region counts a region that loses its leader gets one back inside the
deadline the caller already gave, so the pre-registered criterion answered itself
([ADR 0100](adr/0100-a-region-between-leaders-waits-on-the-callers-deadline.md)). Recorded because
a measurement that refutes its own proposal is the cheapest kind and the easiest to lose.

*Landed since this list was written:* the `"char"` one-byte type (ADR 0095), `oid` as its own type
rather than `bigint` (ADR 0097) and `regproc` (ADR 0098) — b4's three families, all three in the
tree. `regclass[]` (2210) joined them with the same five-place shape, and with it the correction to
ADR 0098's second rule: `min`/`max` over a `regtype`, a `regproc` or a `regclass` all decay to an
`oid`, value included (`crates/esker-sql/tests/reg_class.rs`).

*Settled since this list was written:* **PD HA timing** — three placement drivers replicated with
`esker-raft`, phase 15 ([ADR 0059](adr/0059-pd-is-a-raft-group.md), §7 above). **Dynamic PD
membership** — added and removed at run time through single-server conf changes, with the group id
minted once instead of derived
([ADR 0061](adr/0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)).

## 16. Columnar (`esker-columnar`)

A second copy of a table laid out by column, for the queries a row layout answers badly:
`SELECT count(*), sum(amount) FROM ledger WHERE day > ...` reads every byte of every column to use
two of them. [ADR 0022](adr/0022-columnar-learner-replica.md) decides *that* Esker gets one, where
it comes from and what it costs; [ADR 0027](adr/0027-columnar-file-format.md) decides what the
bytes look like. This section is the shape, not the argument.

The crate shares nothing with `esker-engine` but the `FileSystem` trait (§4) and the primitives in
`esker-base`, which is what lets a columnar file live wherever an SST can, object storage included.

### 16.1 The file

```
file   := stripe* ++ footer ++ trailer
stripe := chunk*                          one chunk per column, in schema order
chunk  := payload ++ codec:u8 ++ crc32c(offset ++ payload ++ codec)
payload := encoding:u8 ++ rows:varint ++ null_count:varint ++ [null mask] ++ values
```

A **stripe** is a row group: the unit of pruning and of decoding. A **chunk** is one column of one
stripe. Values are stored densely for the rows that are not NULL, so a NULL costs one bit in the
mask and no slot among the values; the mask sits *above* the value encoding, so no encoding has to
steal a value from its own domain — and this domain contains `i64::MIN` and the empty string.

Encodings, chosen by encoding both ways and keeping the smaller rather than by a tuned threshold:
frame-of-reference or delta for `int8` and `timestamptz`, dictionary or bit-packed lengths plus
bytes for `text` and `bytea`, bit-packed or run-length for `bool` and the null mask, plain for
`double`. Then LZ4 over the payload when it saves more than an eighth — the engine's rule (§4.5).
The type set is whatever `esker_sql::value::ColumnType` carries, with the row side's own tag
bytes — a number that has risen with every type unit rather than a fixed six. A `numeric`
rides the byte run as its text, so its statistics bounds are that text's bounds and **not**
the number's: `"10" < "9"` as bytes. Nothing prunes yet, and the first pruner must decode
both bounds and compare with `numeric`'s own ordering or skip the type (ADR 0045).

The **trailer** is 32 fixed bytes at the very end: the footer's offset, length and CRC, a CRC over
itself, a format version, and the magic `ESKERCOL`. It is written last, so **its magic is the
commit point**: everything a crash left half-written lacks it and is reported as *unsealed* rather
than as corruption, which is a different operational answer — an unsealed file is deleted, a
corrupt one is an alarm. The file is written to a temporary and renamed into place (invariant 3).

A chunk's checksum covers its **offset**. A checksum proves a block is intact, not that it is the
block that was asked for: a chunk copied over another one carries its own valid checksum and
answers with another stripe's rows. That is what a misdirected write looks like, and the milestone-2
fuzz produced one before the offset was folded in (ADR 0027, format version 2).

The **footer** carries the schema, every chunk's position, and per-chunk statistics — min, max and
null count — so that deciding what to read costs no I/O beyond opening the file. Bounds are
computed in **`pg_cmp` order**, this system's ordering rather than IEEE's, which is why `NaN` is
the maximum of a chunk that holds one: `WHERE x > 5` matches a `NaN` row, and a bound that
excluded it would prune away a stripe the query wants.

### 16.2 The fragment

Push-down rides the axis `TxnKvReq::Scan` already establishes (§9): a request that carries *work*
rather than a range, and returns what the work produced rather than what it read.

```
fragment := version:u8 ++ body ++ crc32c:u32
body     := table ++ key range ++ projection ++ [filter] ++ output
output   := rows(limit) | aggregates(group_by, count/count(col)/sum/min/max)
```

The projection is the only place a table column index appears; the filter, the grouping and the
aggregates all name **projection slots**. So an expression cannot reach a column the fragment did
not ask for, and "decode only what was projected" is a property of the format rather than a
discipline the evaluator keeps. A fragment whose only aggregate is `count(*)` decodes no chunk at
all.

**A fragment this build cannot evaluate is refused, and none of it is done.** An unknown version,
expression node, comparison operator, aggregate kind or type tag; a slot outside the projection; a
`sum` over a type with no addition; a key range, which a columnar file cannot restrict to because
it records none. Honouring the half it understood would silently drop a filter, which returns
extra rows rather than an error — the defect class `esker_sql::plan`'s "reject, do not ignore"
rule exists to make impossible. Refusal and corruption are decided in that order and are different
answers: the checksum runs first, so intact bytes carrying an unknown tag are a build that does
not implement them, not a damaged message, and the caller falls back to a row scan.

Aggregate semantics are PostgreSQL's, defined here because the row executor has none yet:
`count(*)` counts rows and `count(col)` skips NULLs, `sum`/`min`/`max` over nothing are NULL and
not zero, extremes order by `pg_cmp`, NULL forms one `GROUP BY` group of its own. `sum` widens the way a real server widens: `int2` and `int4` to `bigint`, `int8` and `numeric` to
**`numeric`** — so an `int8` sum cannot overflow here either — and every exact type averages to
`numeric` at PostgreSQL's own division scale, which aims at sixteen *significant* digits rather
than sixteen fractional ones. A `float8` stays a `float8` for both.

A fragment's aggregates are folded into **one accumulator per group across the whole file**, in row
order, so its answer does not depend on where stripe boundaries fell — floating-point addition is
not associative. Combining partials *between* files does change the answer, which is the two-level
aggregate's problem and not this one's.

### 16.3 What holds it up

The defence ADR 0022 asks for by name — a differential test — is a second interpreter that answers
the same fragment over the rows the file was *written from*, never opening the file, sharing only
`pg_cmp` because that is the specification. Every generated fragment is answered by both, and by
the evaluator again with pruning switched off, and compared as a `Result` so that an error is an
answer too. Pruning may only ever remove work.

Not built here: the tiering rewrite (ADR 0022 milestone 3's other half), MPP exchange
(milestone 5), compaction, and any wire service — the fragment's bytes are defined in
`esker-columnar` and `esker-proto` carries them when there is something to carry them between.

### 16.4 Choosing between the two

The learner feed is phase 8 and planner routing is
[ADR 0040](adr/0040-the-engine-a-query-runs-on.md), so a query over a table with
`ALTER TABLE t SET (columnar_replicas = 1)` now runs on the copy when it should.

The rule is ADR 0022 Decision 2's, in its order: a point read or a bounded range stays on rows
always, because a columnar file records no key range and cannot restrict to one; anything the
columnar side cannot answer stays on rows, of which the only rule about a *wrong* answer rather
than a slow one is that a transaction which has written cannot be answered by a learner that has
not seen the write; and otherwise the **ratio** decides — `projected / stored`, at most a half,
which is bytes read rather than rows and moves on its own when a table grows a column.
`SET esker.engine = 'row' | 'columnar' | 'auto'` overrides the *estimate* and never the correctness
rules.

Exactly one plan shape is substituted — `Aggregate { [Filter] { SeqScan } }` — by a node producing
the same row the aggregate produced, one fragment per region, finished on the SQL node. A rows-
output fragment is not routed: a fragment answers in one framed message, so it would materialise a
whole region where the row path streams a page.

Every routed plan **carries the row plan it falls back to**, so a refusal — `NotColumnar`,
`TooFarBehind`, `Unsupported`, or a store that could not be reached — is answered in the same
transaction at the same snapshot. Silent to the client, because the answer is the one the snapshot
always had; visible in `EXPLAIN`, which names the engine, the reason, the fragment count and, under
`ANALYZE`, what the scan cost.

What holds it up is a differential on a real cluster: every query run twice at one instant, once
routed and once under `esker.engine = 'row'`, with a concurrent writer and with the learner's store
stopped — and each comparison asserting that the columns *did* answer, because a query that fell
back agrees with the row engine for free.
