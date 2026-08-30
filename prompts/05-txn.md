# Phase 5 — Distributed transactions (Percolator)

Implement `esker-txn` per `docs/DESIGN.md` §8: optimistic transactions with snapshot isolation over the
`lock`/`write`/`default` column families, timestamps from PD's TSO, a client-side buffered write set,
2PC with a primary key as the commit point, lock resolution by readers, and MVCC garbage collection through
the engine's `CompactionFilter`. Read the Percolator paper (Peng & Dabek, OSDI 2010) §2 and keep the
mapping "paper column → Esker CF → key/value bytes" in `docs/txn-spec.md`.

## Deliverables

1. Encodings and CF options: `lock` CF unversioned; `write` and `default` CFs with the 8-byte
   prefix extractor; `short_value` inlined in `write` when ≤ 255 bytes; all value formats hand-rolled and
   golden-tested.
2. Server side (`esker-store` handlers for `TxnKv`): `Get`/`Scan` at a read ts (check locks in
   `[0, ts]`, then newest `write` ≤ ts, then `default`), `Prewrite` (write-conflict check on `write`,
   lock check, atomic `lock`+`default` batch through Raft), `Commit` (atomic `write` + delete `lock`),
   `Rollback`, `ResolveLock` (given primary status), `Heartbeat` (TTL extension), `GcSafepoint` +
   compaction filter that keeps the newest version ≤ safepoint and drops older ones and rollback markers.
3. Client side (`esker-client::TxnClient`): `begin()` → buffered `put/delete/get/scan` with
   read-your-writes → `commit()` doing prewrite (primary first, then secondaries in parallel per region),
   commit primary, async commit of secondaries; retry on region errors; lock resolution with
   backoff; `rollback()`.
4. Range deletions: replace the phase-1 `DeleteRange` limitation with proper range tombstones in the
   engine (ADR), because GC and `DROP TABLE` in phase 6 need them.

## Tests

- **Bank test:** N accounts, M concurrent clients doing random transfers for 60 s under the simulator's
  fault plan (leader kills, partitions, crashes between prewrite and commit); the sum of balances read at
  any single snapshot ts is invariant, and every committed transfer is visible at later ts.
- **Write-skew / lost-update:** classic anomaly tests; SI must prevent lost updates and dirty reads;
  document (in `docs/txn-spec.md`) that write skew is allowed under SI and how SSI could be added later.
- **Crash between prewrite and commit** at every step boundary in the simulator; readers must resolve
  the lock the right way (roll forward iff the primary's `write` record exists).
- **GC:** write 1,000 versions of one key, set a safepoint, compact, assert the visible read is unchanged
  and the SST holds one version.
- Linearizability of single-key transactional histories through the Porcupine-style checker.

## Acceptance

Bank test passes across 1,000 seeds; anomaly tests pass; GC verified; bench `txn-put` and `txn-get`
numbers recorded in `docs/bench/phase-5.md` against the RawKV numbers (the ratio is the cost of 2PC +
MVCC; explain it); DESIGN.md §8 and `docs/txn-spec.md` match the code.
