# 0017 — Range tombstones

Status: accepted (phase 5). Supersedes the v1 refusal in `docs/DESIGN.md` §4.7 and retires the
workaround in [ADR 0006](0006-rawkv-delete-range.md). See `crates/esker-engine/src/range_del.rs`,
`docs/DESIGN.md` §15 ("range tombstones design" — this closes it), `prompts/05-txn.md`
deliverable 4.

## Context

`DeleteRange` has been a *format* since phase 1 and not a feature. `EntryKind::DeleteRange = 2`
exists in the `WriteBatch` and WAL layouts so that making it real would not be a format change, and
`Db::write` refuses any batch containing one — because no read path honours it, and the memtable,
`get` and both iterators would treat it as a point `Delete` at the range's `begin`. Storing it would
delete one key while telling the caller a range was gone (§4.7). `esker-store` has served
`RawKv DeleteRange` as a bounded scan plus point deletes in one atomic batch ever since (ADR 0006).

Phase 5 needs the real thing twice over. MVCC garbage collection removes whole `[key, ts)` spans, and
`DROP TABLE` in phase 6 removes a table's entire key range — both of which are one administrative act
over a range that may hold millions of keys, and neither of which can afford to enumerate them.

## The shape of the problem

A point delete works because it sits *exactly where the key it hides sits*: the sorted run brings the
two together and the read path compares sequence numbers. A range delete cannot, because it hides
keys that do not exist yet and keys it has never seen. So it has to live beside the run, and every
read has to ask a second question.

That second question is the whole cost, and it is why this is not a small change: `get`, both
iterators, flush and compaction each have a place where "the newest entry for this key" is decided,
and every one of them now needs "…and is it covered?" as well.

## Decision 1: tombstones live beside the run, per source

A **memtable** keeps a list of `(begin, end, seqno)` beside its skiplist. A **table** keeps them in a
block of its own. Neither ever puts one in the sorted run.

The rule, in one line: *a key found at sequence number `s` is hidden from a read at snapshot `t`
when some tombstone covers it with `s < tombstone.seqno <= t`.*

Both bounds do work. `tombstone.seqno > s` is what makes a write *after* a range delete survive it —
without it, one `delete_range(a, z)` would swallow every later write to that range for ever.
`tombstone.seqno <= t` is the ordinary snapshot rule, unchanged.

**Options.** (a) Expand the range into point deletes at write time — which is ADR 0006's workaround
moved down a layer, and it is O(keys) for an act that should be O(1). (b) Keep tombstones in the
sorted run at `begin` — the v1 behaviour, which is wrong. (c) Beside the run.

**(c)**, which is what RocksDB does and for the same reason.

## Decision 2: the block is found through the properties, not the footer

`crates/esker-engine/src/sst/footer.rs` fixes the footer at **48 bytes, forever**: six LEB128
varints for three block handles, a format version and a nine-byte magic, with five bytes of padding
in the worst case. A fourth handle needs two more varints and eight varints do not fit in 35 bytes.
The footer's size is the thing a reader seeks by, so it cannot grow.

So the range-deletion block is a real block — written through the same framing, with the same
trailer, checksum and optional compression as every other — and its handle is recorded in the
**properties block** as `esker.range_del.offset`, `esker.range_del.size` and
`esker.range_del.count`.

**Options.** (a) Put the tombstones themselves in the properties block, as one property value. Cheap
to implement and it makes the properties block, which is read whole at open, grow without bound with
a workload's delete count. (b) Bump the format version and grow the footer — forbidden by its own
contract. (c) A handle in the properties, pointing at a block.

**(c).** The properties block already documents exactly this extension route: "An unknown name is
ignored and a missing one keeps its default. That is what lets a later format version add a property
without making every older reader fail." A table with no tombstones — nearly all of them — carries
three extra properties whose values are zero, and reads no extra block. This is what a metaindex
block would be for, and the properties block is already that.

**Consequences.** A reader must ask the properties whether there is a block before it can honour a
tombstone, which it already does for the filter. And `format_version` does not change, so every
table written before this ADR is still readable and simply has no tombstones.

## Decision 3: a table's key bounds are widened to cover its tombstones

A tombstone over `[m, p)` in a table whose keys run `a`..`c` has to be findable when someone reads
`n`. `Version::overlapping` picks files by their `smallest_key`/`largest_key`, so a flush or a
compaction that writes tombstones widens those bounds to span them.

Without it the failure is silent and total: the read never opens the file that says the key is
deleted, and returns the value from a lower level.

## Decision 4: an empty or inverted range is refused, not ignored

`Db::write` rejects a `DeleteRange` whose `end` is not strictly above its `begin`, as
`InvalidArgument`, before anything is logged.

RocksDB treats it as a no-op. This engine does not, on the rule that runs through the rest of it: a
caller who computed `[k, k)` meant something, and "nothing happened" and "everything from k was
deleted" are far enough apart that guessing between them is worse than refusing. There is no
convention that an empty `end` means the end of the key space — the engine is byte-opaque, and an
empty `end` sorts *below* everything. A caller wanting "to the end of this namespace" passes the
namespace's successor, which is what `esker-store` already computes.

## Decision 5: the refusal is lifted in the same commit that makes reads honour tombstones

`Db::write`'s `Error::Unsupported` is deleted in the commit that teaches `get`, both iterators,
flush and compaction to apply tombstones — not before, and not after.

Not after, because the feature is worthless until then. **Not before**, because a database that
accepts a `DeleteRange` and does not honour it is the exact failure §4.7 refuses, and it would be
worse than the refusal: the refusal is loud and the acceptance is silent, and by the time anyone
noticed, the logs would contain entries the reader disagrees with.

## Decision 6 (open): how a tombstone reaches below L0

**Not settled. The format above does not depend on it, which is why this ADR lands ahead of it.**

Decision 3 widens a table's key bounds to span its tombstones, and that is safe at L0, whose files
overlap by design and are all consulted. It is **not** safe below L0 as written, and the reason is
worth recording because it is not obvious:

`DbInner::search_levels` finds the one file per level that can hold a key with
`partition_point(|file| file.largest < target)`. That is correct only because a level's files
partition the key space, so `largest` is non-decreasing across the level. Widening one file's bounds
to span a tombstone can push its `largest` past the next file's `smallest`, and then the binary
search can return the earlier file for a key the later one holds — a read that misses the key
entirely, silently.

Three ways out, in increasing order of how much they change:

**(i) Clip tombstones to output boundaries at compaction.** Each output file carries the part of each
tombstone that falls in `[its first key, the next output's first key)`. `largest` then never passes
the next file's `smallest` by more than the single boundary key, so the partition invariant survives
and `search_levels` needs only to scan forward from the partition point while the file still covers
the key — at most two files. This is what RocksDB does. It needs the compaction's input selection to
already cover the tombstones' full range, or a tombstone that reaches beyond the compaction loses the
keys it should have covered outside it.

**(ii) Discharge tombstones at L0 and never propagate them.** A compaction whose inputs carry a
tombstone must also take every lower-level file overlapping its range; it then drops the covered keys
and drops the tombstone with them. Nothing below L0 ever holds one, `search_levels` is untouched, and
reads consult only the memtable, the immutables and L0. The cost is that a `DeleteRange` forces one
compaction over the deleted range — which for `DROP TABLE` and a GC sweep is work that has to happen
anyway, and happens once.

**(iii) A per-level tombstone index on the `Version`.** File bounds stay untouched and the read path
asks the version rather than a file. It needs `FileMeta` to record whether a file has tombstones,
which is a manifest format change, and it moves state into the version set that has to be rebuilt on
open.

(ii) is the smaller change and is enough for both of phase 5's users; (i) is the general one. Whoever
implements the read half should pick before writing any of it, because the choice decides whether
compaction or the picker is the piece that moves.

## Consequences

- `EntryKind::DeleteRange` and the `WriteBatch`/WAL layouts are unchanged, which is what freezing
  them in phase 1 bought.
- `esker-store`'s `RawKv DeleteRange` workaround (ADR 0006) can become one `WriteBatch` entry. That
  file belongs to the phase-4 lane and the change is theirs to make; ADR 0006 stays as the record of
  why it existed.
- A compaction at the bottom level drops entries covered by a tombstone outright, and drops the
  tombstone with them once nothing below it survives — the same rule that governs point deletes.
- Until decision 6 is settled and the read paths honour tombstones, `Db::write` keeps refusing
  `DeleteRange` and `esker-store` keeps ADR 0006's workaround. Decision 5 is why: a database that
  accepts a range delete it does not honour is worse than one that refuses it.
- The GC compaction filter phase 5's store half needs is now expressible: a safepoint sweep is a
  range delete per key prefix rather than a scan.
