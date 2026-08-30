# 0006 — `RawKV DeleteRange` without range tombstones

Date: 2026-08-30 · Status: accepted · Phase: 2

## Context

`prompts/02-single-node-server.md` lists `DeleteRange` among the `RawKv` methods the store must
serve, and notes that "the v1 limitation from DESIGN.md §4.7 applies". That limitation reads:

> Range deletions are implemented as range tombstones in v2; v1 rejects `DeleteRange` across more
> than one SST boundary with an error (documented limitation, removed in phase 5).

The engine as built in phase 1 does not do that. `DeleteRange` is a stored entry kind — it appears
in the `WriteBatch` format, in the WAL, in the memtable and in compaction — but every read path
treats it exactly like a point `Delete` at the range's `begin` key:

```rust
// memtable.rs, and the same shape in db/read.rs and db/iter.rs
Some((_, _, EntryKind::Delete | EntryKind::DeleteRange)) => Some(Lookup::Deleted),
```

The phase-1 plan records this deliberately (§8): *"`DeleteRange` is a stored entry kind with the
v1 limitation of DESIGN §4.7: the memtable answers for the key at `begin` and no other. The format
is frozen, so making it real is not a format change."*

So the engine does not reject an unsupported range — it **accepts every range and deletes one
key**. That is a fine internal state of affairs while the only caller is the engine's own tests.
It stops being fine the moment a network client can call it, because the store would answer "the
range is deleted" to a request that removed one key out of a thousand. Silent data retention is
worse than a refusal, and worse still than a crash: nothing in the system would ever notice.

Three options.

**(a) Refuse `DeleteRange` outright** until the engine has range tombstones. Honest, and it makes
the phase's method list incomplete: a client cannot delete a range at all, and the API that "the
multi-node phases must not need to change" would gain a working method in phase 5.

**(b) Pass the request to the engine's `WriteBatch::delete_range`.** One entry, atomic, cheap —
and wrong, in the specific way that is hardest to find later. It is the option that requires
nobody to write any code. (Since this ADR was accepted, the engine refuses it outright, so (b) is
no longer available to anyone: `Db::write` returns `Error::Unsupported`. See the consequences.)

**(c) Implement the range delete in the store**, as a scan of the range followed by point deletes
in one `WriteBatch`. Correct, atomic, and bounded by how many keys the range holds.

## Decision

**(c), with a hard bound.** The store scans `[start, end)`, collects the stored keys, writes one
`WriteBatch` of point `Delete`s, and returns the count. A range holding more than
`Limits::max_delete_range_keys` keys (10,000 by default) is refused with
`ProtoError::Unsupported`, naming the limit and saying to delete in smaller ranges, **before
anything is written** — so a refused `DeleteRange` deletes nothing.

The engine's `DeleteRange` entry kind is not used by this path at all.

Two properties this buys, and one it does not:

* **Atomic.** The deletes are one `WriteBatch`, so the range disappears in one step and a crash
  leaves either all of it or none of it. That is what a caller expects from a single request.
* **Bounded.** The scan, the batch and the answer are all bounded by the same number, so a
  `DeleteRange` over the whole key space is a typed refusal rather than an out-of-memory kill.
* **Not isolated.** A key written between the scan and the write survives the delete. There is no
  cheap fix on a single node without a lock over all writes, which `CLAUDE.md` and the phase brief
  both rule out, and the real fix is the one phase 3 brings: every write becomes a Raft proposal,
  and the apply loop is a serialisation point that a range delete can occupy. Documented on the
  method, not silently accepted.

## Consequences

* A `DeleteRange` costs a scan of the range. Deleting a million keys is a hundred requests of ten
  thousand rather than one request, which is a visible cost in the API rather than a hidden one in
  the engine.
* `docs/DESIGN.md` §4.7 described a check that was never written: it said v1 rejects a wide
  `DeleteRange` with an error, and in fact v1 accepted it and under-deleted. This was reported to
  the coordinator as an engine finding rather than fixed here, because `esker-engine` is phase-1
  code — and then fixed on a one-time grant to touch it: **`Db::write` now refuses any batch
  containing a `DeleteRange`** with `Error::Unsupported`, before the batch is logged. The entry
  kind stays in the frozen format. So the workaround below is no longer a choice between a correct
  path and a silently wrong one; it is the only path, and the engine says so.
* When range tombstones land in phase 5, this store path becomes one entry again and the bound can
  go. The wire format does not change: `DeleteRange { start, end, sync }` and
  `DeleteRange { deleted }` are the same messages either way, and `deleted` stops being a count the
  store had to compute.
* The bound is a `Limits` field rather than a constant, so a test can set it to four and prove the
  refusal happens before the write.
