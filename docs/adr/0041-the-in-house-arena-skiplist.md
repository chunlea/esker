# 0041 — The in-house arena skiplist

Status: **proposed, design only.** No code is written under this ADR without a coordinator's GO.
It replaces the one bought piece of concurrent code named in `CLAUDE.md`'s dependency policy and in
`docs/DESIGN.md` §4.4, and closes the `TODO(post-v1)` at `crates/esker-engine/src/memtable.rs:273`.

## Context

`crossbeam-skiplist` is the deliberate exception in a project whose whole point is building these
parts itself: *"the one piece of concurrent unsafe code we buy rather than write."* It was the
right call at the time — a lock-free ordered map with concurrent readers during a write is the hard
part, and getting it wrong is a data race rather than a failing test.

Three things have changed since.

**1. The cursor is `O(log n)` per step, and the reason is a lifetime.** `MemTableIter` cannot hold
a `crossbeam_skiplist::map::Entry`, because that entry borrows the map: a cursor holding both the
`Arc<MemTable>` and a reference into it is self-referential, which in safe Rust means threading a
lifetime through every caller above — `Db::iter`, `MergeCursor`, compaction — or a crate we are not
allowed to add. So the cursor remembers its position **as a key** and re-finds it on every step,
copying the entry it lands on. `memtable.rs:265` says exactly this. A merge iterator over four
memtables and several levels calls `next()` once per entry per child, so a scan of a full memtable
is `O(n log n)` with a `Vec` allocation per step where it should be `O(n)` with none.

**2. The comparator costs an `Arc` clone per insert.** `crossbeam-skiplist` orders by the key
type's `Ord`, and the order the engine needs is chosen per column family at runtime. So every key
carries its own `Arc<InternalKeyComparator>` handle (`memtable.rs:19`). An arena skiplist holds the
comparator once, on the table.

**3. The engine now knows what its concurrency actually is**, which it did not in phase 1. See the
next section — it is much narrower than a general lock-free map, and that is the whole argument.

## The concurrency this memtable actually needs

Not "a lock-free ordered map". Reading `db/write.rs` and `db/flush.rs`:

* **Writers are serialised.** Every insert happens inside `write_group`, which the group-commit
  leader runs alone (`docs/DESIGN.md` §4.2). One thread inserts into a given active memtable at a
  time, always. There is no writer-writer concurrency to design for.
* **Writers never remove, and never mutate an existing node.** `memtable.rs` says so in its
  header: nothing is ever removed; a delete is a tombstone at a higher sequence number, so the map
  is **append-only** for its whole life. A flush retires a table wholesale and builds an SST from
  it; it does not delete keys from it.
* **Readers are concurrent with the writer and with each other, and are unsynchronised.** A `get`
  or a cursor may run at any point during an insert.
* **A reader may outlive the table's retirement.** `MemTable::iter` takes an `Arc<Self>` and keeps
  it, so a scan carries on across a memtable switch. Reclamation is therefore `Arc`'s job and not
  the skiplist's.

**That last pair is what removes the hard part.** `crossbeam-epoch` exists to answer "when may a
node freed by one thread be reclaimed, given readers that may still be inside it". A structure that
never frees a node until the whole arena is dropped, and whose arena is dropped by the last `Arc`,
never asks that question. No epochs, no hazard pointers, no deferred destruction — the reclamation
problem is solved by ownership, one level up, and it is already solved there today.

So the structure is: **a single-writer, multi-reader, append-only, arena-backed skiplist**, which
is precisely LevelDB's, and LevelDB's is about 350 lines.

## Options

1. **Keep `crossbeam-skiplist`.** Correct, and the cursor stays `O(log n)` per step with a copy,
   because the lifetime that forces it is a property of that crate's API and not something a
   caller can work around. Three crates stay in the budget.
2. **Wrap the whole memtable in an `RwLock<BTreeMap>`.** All safe, no dependency, and it puts every
   reader behind a lock the writer takes on the write path. Refused: it would make a scan block
   group commit, which is the one thing §4.2 says must not happen.
3. **An in-house arena skiplist, single-writer.** What this proposes.
4. **An in-house *lock-free* skiplist, multi-writer.** More `unsafe`, more to prove, and it buys
   nothing the engine can use — writers are serialised by group commit and would stay serialised.
   Refused as scope.

## Decision (proposed)

**Option 3.** `esker_engine::memtable::skiplist`, behind the existing `MemTable` surface, so
nothing above `memtable.rs` changes except that `MemTableIter` gets faster.

### The arena

A `Vec<u8>` grown in blocks, never moved and never freed until the table is dropped. Nodes are
allocated by bumping an offset; a node is a header (key length, value length, height) followed by
the key bytes, the value bytes, and `height` forward pointers stored as `u32` **offsets into the
arena**, not raw pointers.

Offsets rather than pointers is the load-bearing choice and it is what `db/table_cache.rs` and
`cache/lru.rs` already do for the same reason. A `u32` offset stays valid when the arena's backing
`Vec` is reallocated, so growth needs no fix-up pass and no stable-address guarantee; a raw pointer
would need the arena to be a list of never-reallocated blocks and would make every growth a place
to get lifetime wrong.

### The `unsafe` surface, in full

The design's aim is that **the unsafe surface is three functions**, each a handful of lines, each
with a `// SAFETY:` naming the invariant it relies on and a test that exercises it:

| Site | What it does | The invariant | Its test |
|---|---|---|---|
| `Arena::grow` | Extends the backing buffer | Only the writer calls it, and only while holding the table's writer token; readers never observe a partially written node because a node's forward pointers are published last | A proptest that grows the arena under a concurrent reader loop, checking every read is either the old or the new state and never a torn one |
| `Node::next(level)` | Reads a forward offset | The offset is either `NIL` or a node fully written before it was published | The linearizability-style test below |
| `Node::set_next(level, offset)` | Publishes a forward offset | Called only by the writer, with `Ordering::Release`; every reader loads with `Ordering::Acquire` | A test that reverses the ordering to `Relaxed` and shows the checker fires under `--test-threads` pressure |

Everything else — the search, the height draw, the key comparison, the cursor — is safe code over
those three.

**The publication rule is the whole safety argument, stated once here so a reviewer can check the
code against one sentence:** a node is fully written — header, key, value, and all of its forward
pointers — *before* any existing node's forward pointer is made to point at it, and that final
store is a `Release` matched by an `Acquire` load in every reader. A reader therefore sees either
no node or a complete one, and never a half-built one.

Height is drawn from a seeded PCG32 (`esker_base::rng`), **per table**, so a memtable's shape is a
function of its seed and a failing test replays. `crossbeam-skiplist` uses thread-local entropy and
does not.

### What does not change

`MemTable`'s public surface: `add`, `add_range`, `get`, `iter`, `approximate_size`, `len`,
`range_tombstones`. Range tombstones stay beside the map in their own `Mutex<Vec<_>>` (ADR 0017) —
they are a short list appended to rarely and read once per iterator, and putting them in the
skiplist would be the mistake ADR 0017 exists to prevent.

The **cursor** changes shape, and that is the point: it holds `Arc<MemTable>` plus a `u32` node
offset, so `next` and `prev` are a pointer hop. `prev` needs the same treatment LevelDB gives it —
a skiplist has no back pointers, so `prev` is "seek to the largest key strictly less than this
one", `O(log n)` and unchanged. Forward iteration, which is what a scan and a compaction do, goes
from `O(log n)` with an allocation to `O(1)` with none.

## The test plan, with the tools already in the workspace

Nothing here needs a new dependency.

1. **Unit tests** for the arena: allocation alignment, growth across a reallocation, offset
   round-trip, `NIL` never aliasing a real node.
2. **A `proptest` differential against `BTreeMap`.** Random `add`/`get`/`seek`/`next`/`prev`
   sequences over a small key space, compared entry for entry with the map — the shape
   `tests/model.rs` already uses for the whole `Db`, applied to one structure.
3. **A single-writer/many-reader concurrency test** in `tests/concurrency.rs`, which already exists
   and already runs threads against a `Db`: one writer inserting a known sequence, N readers
   scanning throughout, each reader's output required to be a subsequence of the final state in
   key order with no duplicates and no torn keys. That is the assertion that catches a publication
   bug, and it is the one that must be shown **red** against a deliberately `Relaxed` store before
   the real ordering is trusted.
4. **A `stateright` model** of the publication protocol itself — writer states (allocating,
   writing, publishing) against a reader's load — exhaustive over the interleavings of one insert.
   Small enough to enumerate, and it is the piece a proptest cannot promise to have covered.
5. **Miri** over the unit and proptest suites. It is a toolchain component rather than a
   dependency, so it costs nothing in the budget, and it is the only thing that will catch a
   provenance mistake in the offset arithmetic. **This is a gate on the code landing**, not a
   nice-to-have: `CLAUDE.md` invariant 8 asks for a test that exercises every `unsafe` block, and
   for pointer arithmetic "exercises" means Miri.

## The bench plan

`esker-cli bench` already has what is needed after phase 11 U2: `--write-buffer-size` decides how
much of a workload stays in the memtable.

* `fillseq` and `fillrandom` with a large `--write-buffer-size`, so the run is memtable-dominated:
  the insert path, and where the per-insert `Arc` clone shows up.
* `readseq` with the same, so the whole scan is the memtable cursor: this is the number the
  `O(log n)`-per-step cursor costs, and the one with the most headroom behind it.
* `scanrange --batch-size 20`, for the short-scan shape.
* `overwrite`, because a key written many times is many nodes in an append-only structure and the
  arena's growth behaviour under that is worth a number.

Recorded in `docs/bench/` beside the phase that lands it, before and after, on the same machine —
and, per the lesson in `docs/plans/phase-11-engine.md` §10, **interleaved**, not one arm then the
other.

## Consequences

**The crate budget gains three.** `cargo tree -p esker-engine -e normal` shows the whole subtree:

```
crossbeam-skiplist v0.1.3
├── crossbeam-epoch v0.9.20
│   └── crossbeam-utils v0.8.22
└── crossbeam-utils v0.8.22
```

Three crates, and nothing else in the workspace depends on any of them, so all three leave the
runtime graph. `deny.toml` says lowering the budget is welcome and raising it needs an ADR; the
number in it should come down by three in the same change, and `esker-cli/tests/dep_budget.rs` is
what will say whether that is right.

**The runtime allowlist loses its only concurrency dependency.** After this, every piece of
concurrent code in the engine is code in this repository with a test in this repository.

**It is `unsafe`, and `CLAUDE.md` invariant 8 applies in full.** Three blocks, three `// SAFETY:`
comments, three tests, and Miri over all of it. If any of those cannot be delivered the change does
not land — a skiplist that is fast and unproven is worse than a skiplist that is slow and bought.

**The risk that matters is not the algorithm.** A skiplist is a well-understood structure and
LevelDB's is public. The risk is the memory ordering, and it fails silently: a missing `Release`
produces a test suite that passes on x86 for a year and corrupts a memtable on ARM under load.
Item 3's red-first requirement is the mitigation and is not negotiable.

**What this does not do.** It does not make writers concurrent, it does not change any on-disk
format, and it does not touch anything above `memtable.rs`. If a later phase wants multi-writer
memtables, that is a different ADR and it starts by changing group commit.

## Status of this ADR

Design only. `docs/plans/phase-11-engine.md` §6 records that the lane stopped here deliberately;
the decision to write the code belongs to the coordinator, and the thing to weigh is that the gain
is a faster scan and three fewer crates, against `unsafe` in the one place where getting it wrong
is silent.
