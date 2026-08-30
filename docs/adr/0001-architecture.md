# 0001 — LSM storage, Multi-Raft replication, range sharding

Date: 2026-08-30 · Status: accepted · Phase: 0

## Context

Esker has to be a transactional, horizontally scalable, ordered key-value store that a SQL
layer can later sit on without the layers below changing. That target constrains the shape
more than it might appear:

* **Ordered keys with cheap range scans.** A SQL table is a key range and an index is another
  key range; a scan of either has to be a sequential read, not a scatter of point lookups.
  Anything hash-partitioned is disqualified before performance is even discussed.
* **MVCC with the version in the key suffix**, so a snapshot read is "the first key under this
  prefix" and garbage collection is a compaction filter rather than a background scanner.
* **Crash safety at every layer.** `kill -9` at any instant must not lose an acknowledged
  write.
* **Linear scale-out**, which means the unit of replication has to be a piece of the key space
  rather than the whole database.
* And a constraint that is not technical: this is a project to **learn these systems by
  building them**, so "use the mature implementation" is a real option to weigh and usually the
  wrong one here.

## Options considered

### Storage engine

1. **Log-structured merge tree, written from scratch** — WAL, memtable, SSTs, leveled
   compaction, column families.
2. **Embed RocksDB.** Mature, fast, and exactly the design we would be copying.
3. **B-tree / page store.** Better read amplification, worse write amplification, much harder
   to make crash-safe without a page-level redo log, and a poor fit for object-storage tiering
   because pages are mutated in place.
4. **Object-storage-first, no local engine.** Write batches straight to S3-shaped storage.

### Replication

1. **Multi-Raft**, one Raft group per key range.
2. **Single Raft group for the whole cluster.** Simple, and a hard ceiling on throughput: one
   leader orders every write in the system.
3. **`openraft` or `raft-rs`** instead of writing the state machine.
4. **No consensus**: primary/backup with asynchronous replication, or a shared-storage design
   where durability comes from the object store.

### Sharding

1. **Range sharding** with a placement driver that splits and moves ranges.
2. **Hash sharding** with a fixed or consistent-hashed partition count.

## Decision

**LSM + Multi-Raft + range sharding**, all written in-house, with a placement driver holding
the routing table and the timestamp oracle.

The LSM falls out of the requirements. Writes become sequential I/O, which is what gives
predictable write throughput; SSTs are immutable, which is what makes snapshots a hard-link
away and object-storage tiering possible later without changing the write path; and a
compaction filter is the natural place for MVCC garbage collection. Its costs are real — read
amplification, space amplification, and write stalls when compaction falls behind — and the
design answers each of them explicitly (bloom filters and a block cache, leveled compaction,
and a stall policy that is *visible in metrics* rather than mysterious).

Range sharding follows from ordered scans: a hash-partitioned range scan touches every
partition, which turns the one operation SQL depends on most into a fan-out. Range sharding
brings hot-spotting on sequential keys, which is a scheduling problem the placement driver can
work on, whereas hash sharding's cost is architectural and permanent.

Multi-Raft follows from range sharding plus the crash-safety requirement. Replicating each
range with its own group makes throughput scale with the number of ranges instead of with one
leader, and Raft gives a linearizable, well-understood, model-checkable story for what happens
when a node dies mid-write. A single group would be simpler and would cap the system at one
machine's ordering throughput. Primary/backup without consensus would require an external
arbiter to avoid split-brain and would trade away exactly the guarantee this project is about.

We write all three ourselves. `openraft` and `raft-rs` are good and are banned here on purpose:
the point of the exercise is the state machine, its `Ready` contract, and the simulator that
proves the driver honours it. That prohibition is a project constraint (`CLAUDE.md`,
"Toolchain and conventions"), not a technical judgement about those crates.

Object-storage-first was rejected for v1 and kept as a **roadmap hook** rather than a
foundation. Its latency profile does not suit a WAL or a Raft log, and it needs the immutable
SST plus `FileSystem` trait that this design produces anyway. `docs/DESIGN.md` §13 keeps every
SST read behind that trait so tiering can be added later without disturbing the layers below.

## Consequences

* Four things must be built before anything is usable end to end: an engine, a Raft core, a
  store that drives them, and a placement driver. Phases 1 through 4 exist for that reason and
  are gates, not suggestions.
* The Raft core must stay a pure state machine with no I/O (`CLAUDE.md` invariant 4). That is
  what makes it simulatable and model-checkable, and it is the invariant most likely to be
  eroded by convenience.
* Every request carries a region epoch, and every layer above the engine has to handle
  `EpochNotMatch` and `NotLeader` redirects. Clients cache routing and must treat the cache as
  a hint.
* Write stalls are a design surface, not a bug to be discovered later: L0 file count, pending
  compaction bytes and stall state are metrics from phase 1.
* Hot ranges are a real failure mode. Splitting and leader balance are placement-driver work
  (phase 4b); until then a sequential-key workload will hot-spot one store.
* Reversing this would mean rewriting the project. What can be revisited cheaply is inside the
  layers: the compaction strategy, the Raft log store, and whether reads use leases or
  `ReadIndex`.
