# 0069 — A dropped database is reclaimed by range, not key by key

* Status: accepted for the store half; the wire message it needs is **not** in this change
* Date: 2026-09-04
* Follows the shape of [ADR 0034](0034-a-removed-peer-is-swept-and-its-range-reclaimed.md).

## Context

`DROP DATABASE` today is one Percolator transaction that walks every key the tenant owns and
deletes it individually:

```rust
for (start, end) in record::tenant_ranges(tenant) {   // 257 ranges
    for (key, _) in txn.scan(&start, &end, u32::MAX)? {
        txn.delete(&key);
    }
}
```

Three costs, and the first two are fatal rather than slow:

* **`u32::MAX` with values.** `Txn::scan` answers `Vec<(Bytes, Bytes)>`, so a database of *n* bytes
  is *n* bytes in the coordinator's memory before a single key is deleted.
* **One transaction.** Every key becomes a prewrite lock and then a commit record. A Percolator
  transaction whose write set is the whole database will not commit, and the failure arrives after
  the scan has already been paid for.
* **It grows the data before it shrinks it.** Each delete is an MVCC version, so a dropped database
  is briefly *larger* on disk, and the bytes only leave when GC passes the safepoint.

The 257 is not incidental either: the row and index data is one range (`'t' ++ tenant ++ …`, the
tenant first so it is contiguous), and the catalog metadata is 256 more, one per kind byte.

## What a reclaim has to be

The properties are ADR 0034's, and they are what `DROP DATABASE` has none of:

* **bounded work per tick** — no single step may be proportional to the database;
* **idempotent** — a step repeated after a crash must be a no-op, not a second delete;
* **crash-safe at every instant** — `kill -9` between any two operations leaves a state the next
  start can continue from, and never one that serves half a database.

## Options

1. **Chunked logical delete, in `esker-sql`.** Delete in bounded batches across many transactions,
   re-driven by the schema-job machinery that already exists for `CREATE INDEX CONCURRENTLY`. Needs
   no format change, no wire change, and nothing from this crate. Still `O(keys)`: every key gets a
   tombstone, and the space comes back only after GC.
2. **Physical range reclaim, in `esker-store`.** Clear the tenant's user-key range across
   `default`, `lock` and `write` with `delete_range`, region by region. `O(regions)` writes rather
   than `O(keys)`, and the space comes back at the next compaction rather than at the next GC pass.
   Needs a replicated command and a wire request to trigger it.
3. **Both halves, split by what they are for.** The catalog drop stays a small transaction; the
   bytes are reclaimed afterwards by a background range clear.

## Decision

**Option 3, with option 2 as the mechanism.**

**1. The catalog drop stays transactional and stays small.** It deletes the database record and the
tenant's *metadata* keys — bounded, because those are catalog entries and not rows — and that
commit is what makes the database gone. Atomicity belongs here: a half-dropped catalog is a
database that half exists.

**2. The rows are not deleted. Their key range is reclaimed.** Once the catalog drop has committed,
nothing can route into `'t' ++ tenant ++ …`: `database_id` answers `None`, so no session resolves a
relation there and no plan can name one. The range is unreachable, and unreachable is what makes a
physical clear safe — this is the same argument `TiKV`'s `unsafe_destroy_range` rests on, and it is
worth naming as an argument rather than an intuition.

**3. The clear waits for the safepoint, and that is the one thing that makes it correct.** A
transaction that took its snapshot *before* the drop committed can still legally read those rows.
So the reclaim may not run until the drop's commit timestamp is below the safepoint PD publishes
(`crate::gc`) — the same floor the MVCC collector already respects, reached by the same
`TxnKv::GcSafepoint` a store already answers. Below it, no reader exists that could observe the
range, and the clear is invisible rather than merely unlikely to be noticed.

**4. Bounded per tick, by a cursor rather than by a timer.** The reclaim clears one bounded chunk
of the range per pass and persists where it got to. A chunk is one synced `WriteBatch` of
`delete_range` over every column family and both physical namespaces, which is atomic by
construction: after `kill -9` the batch is either in the WAL or it is not, and there is no
intermediate state to recover from. Re-running from a persisted cursor re-deletes an empty range,
which is a no-op — so idempotence is a property of the operation and not of a guard.

**5. The order is the same as ADR 0034's, for the same reason.** The record that says "this range
is being reclaimed" is written and synced *before* any byte is deleted. A crash between them leaves
keys under no record — unreachable, never served — which is the recoverable direction. The other
order leaves a record claiming a range is gone while its keys are still there, and something that
later routes into it would serve a mix.

## What this change does not contain, and why

The trigger has to cross the wire, and `esker-proto` is sequenced by the coordinator. What is
needed is **one store-directed request**, a sibling of `TxnKvReq::GcSafepoint`:

```text
TxnKvReq::ReclaimRange { start: Bytes, end: Bytes, below_ts: u64 }
```

* `start`/`end` — the **user-key** range, which the store already knows how to map into both
  physical namespaces (`snapshot::physical_ranges`);
* `below_ts` — the drop's commit timestamp. The store refuses until its own safepoint is at or
  above it, which makes the safety condition a property of the request rather than of the caller's
  timing.

It answers with how far it got, so the caller resumes rather than restarts. Until that message
exists, this ADR's store half is reachable only in-process, and `DROP DATABASE` keeps its current
behaviour — which is correct, and slow, and bounded only by the size of the database.

## Consequences

* A dropped database costs `O(regions)` writes instead of `O(keys)`, and its space returns at the
  next compaction rather than after a GC pass over every tombstone.
* The drop is no longer atomic with the reclaim, and that is deliberate: they answer different
  questions. "Is the database gone" is answered by the catalog, synchronously. "Are the bytes gone"
  is answered later, and no client waits for it.
* A crash between the two leaves an unreachable range with keys in it. It is retried from the
  persisted record; until then it costs disk and nothing else, which is exactly the state ADR 0034
  calls the recoverable direction.
* The safepoint gate means a drop does not free space immediately on a cluster with a long
  retention window. That is the same trade the MVCC collector already makes and it is visible in
  the same place.
* `esker-sql` keeps option 1 available and unchanged. If the wire message is not sequenced, the
  chunked logical delete is the fallback and needs nothing from this crate.
