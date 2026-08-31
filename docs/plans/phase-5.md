# Phase 5 plan — Percolator: the front half

Status: **in progress** — this file covers the lane `wy-p5-txn` only. Written before implementation;
§8 records progress and §9 what changed. Spec: `prompts/05-txn.md`. Constitution: `CLAUDE.md`
(invariants 1, 5, 6 and 9 are the ones this phase leans on). Design: `docs/DESIGN.md` §3, §8, §9,
§10, §4.7. Byte-level companion: `docs/txn-spec.md`.

Phase 4 is still open in `esker-store` and `esker-pd`. Phase 5 therefore starts from its *far* end:
the parts that have no store in them — the record encodings, the protocol library, the wire codecs
and the client — get built first, against fakes, so that when phase 4 closes the store handler is a
`decode → call → write batch through Raft` shim over code that is already tested.

## 1. Scope, and what is deliberately not here

| | In this lane | Deferred |
|---|---|---|
| Encodings (deliverable 1) | `esker-txn`: the `lock`/`write`/`default` record formats, keys, seeks, goldens | the CF *options* (prefix extractor, bloom) — an `esker-store` bootstrap concern |
| Protocol (deliverable 2) | `esker-txn::percolator`: every decision, as pure functions over a snapshot trait | the `esker-store` `TxnKv` handlers that call them, and the GC `CompactionFilter` |
| Wire (§9) | `esker-proto`'s `TxnKv` request/response codecs — the reserved `0x02` service | the server dispatch |
| Client (deliverable 3) | `esker-client::TxnClient`, whole | — |
| Range deletes (deliverable 4) | `esker-engine` range tombstones, read/flush/compaction and the lifted refusal | — |
| Tests | the protocol matrix, codec goldens + proptests, every client retry and resolution path, the engine's model/crash battery | the **bank test**, the crash-between-prewrite-and-commit simulator run, the GC test, `docs/bench/phase-5.md` — all of which need a store that serves `TxnKv` |

The split is not a convenience. Everything above needs exactly two things from the store — that a
`WriteBatch` lands atomically, and that a snapshot answers five questions — and both are already
built. Writing the store handler now would mean writing it against a `RegionMeta` that phase 4c is
still moving.

**File ownership.** `crates/esker-txn/**`, `crates/esker-client/src/**`, `docs/txn-spec.md`, this
file, and — for §7 only — `crates/esker-engine/**`. In `crates/esker-proto/**` this lane adds the
`TxnKv` method numbers and their codecs and touches nothing else. `crates/esker-store/**` and
`crates/esker-pd/**` belong to the phase-4 lanes and are not edited here.

## 2. File list

```
docs/plans/phase-5.md                    this file                                       (unit 1)
docs/txn-spec.md                         paper column → CF → bytes, and the SI guarantees (unit 1)
docs/adr/NNNN-txn-record-encodings.md    why these bytes, and why the kind byte versions  (unit 2)
crates/esker-txn/src/lib.rs              the crate root: invariants, re-exports           (unit 2)
crates/esker-txn/src/key.rs              'x'-space keys: lock/write/default, the seek      (unit 2)
crates/esker-txn/src/codec.rs            LockRecord and WriteRecord encode/decode         (unit 2)
crates/esker-txn/src/error.rs            TxnError                                          (unit 2)
crates/esker-txn/tests/golden.rs         frozen record and key bytes                      (unit 2)
crates/esker-txn/tests/proptest_codec.rs round trip, canonicity, no panic on garbage       (unit 2)
crates/esker-txn/src/snapshot.rs         the TxnSnapshot trait + the in-memory fake        (unit 3)
crates/esker-txn/src/percolator.rs       check_prewrite / commit / rollback / resolve      (unit 3)
crates/esker-txn/src/mutation.rs         Cf, Mutation, Mutations — WriteBatch-shaped        (unit 3)
crates/esker-txn/tests/protocol.rs       the decision matrix                              (unit 3)
docs/adr/NNNN-txnkv-on-the-wire.md       the service, and why lock_info stays opaque      (unit 4)
crates/esker-proto/src/txn.rs            TxnKvReq/TxnKvResp + codecs                      (unit 4)
crates/esker-proto/src/messages.rs       the eight method numbers of service 0x02         (unit 4)
crates/esker-proto/tests/golden/*        frozen wire bytes                                (unit 4)
crates/esker-client/src/txn.rs           TxnClient, Transaction, the buffered write set   (unit 5)
crates/esker-client/src/raw.rs           the call loop, generalised over the two services (unit 5)
crates/esker-client/tests/txn.rs         every retry and resolution path, injected clocks  (unit 5)
docs/adr/NNNN-range-tombstones.md        the design, against ADR 0006 and §4.7            (unit 6)
crates/esker-engine/**                   range tombstones through read, flush, compaction (unit 6)
```

## 3. Public API sketch

```rust
// esker-txn
pub struct LockRecord  { pub kind: Kind, pub start_ts: u64, pub ttl_ms: u64,
                         pub primary: Bytes, pub short_value: Option<Bytes> }
pub struct WriteRecord { pub kind: Kind, pub start_ts: u64, pub short_value: Option<Bytes> }
pub enum   Kind        { Put, Delete, Rollback, Lock }

pub mod key {                       // every function returns a *engine* key, 'x'-prefixed
    pub fn lock(user_key: &[u8]) -> Vec<u8>;                    // 'x' ++ k
    pub fn write(user_key: &[u8], commit_ts: u64) -> Vec<u8>;   // 'x' ++ k ++ !commit_ts
    pub fn value(user_key: &[u8], start_ts: u64) -> Vec<u8>;    // 'x' ++ k ++ !start_ts
    pub fn seek_write(user_key: &[u8], ts: u64) -> Vec<u8>;     // where a read at ts starts
}

pub trait TxnSnapshot {             // five questions, all fallible, all the store can answer
    fn get_lock(&self, key: &[u8]) -> Result<Option<LockRecord>>;
    fn seek_write(&self, key: &[u8], ts: u64) -> Result<Option<(u64, WriteRecord)>>;
    fn get_write_at(&self, key: &[u8], commit_ts: u64) -> Result<Option<WriteRecord>>;
    fn get_write_newer_than(&self, key: &[u8], ts: u64) -> Result<Option<(u64, WriteRecord)>>;
    fn get_value(&self, key: &[u8], start_ts: u64) -> Result<Option<Bytes>>;
}

pub fn read(snap: &impl TxnSnapshot, key: &[u8], ts: u64) -> Result<ReadOutcome>;
pub fn check_prewrite(snap: &impl TxnSnapshot, req: &Prewrite) -> Result<PrewriteDecision>;
pub fn commit_primary(snap: &impl TxnSnapshot, ..) -> Result<PrimaryCommit>;   // → token
pub fn commit_secondary(snap: &impl TxnSnapshot, .., &PrimaryCommitted) -> Result<Mutations>;
pub fn rollback(snap: &impl TxnSnapshot, key: &[u8], start_ts: u64) -> Result<Mutations>;
pub fn resolve(lock: &LockRecord, primary: PrimaryState, now_ts: u64) -> Resolution;
```

```rust
// esker-client
pub struct TxnClient { /* an Arc<RawClient>-shaped core plus a TimestampOracle */ }
impl TxnClient {
    pub fn begin(&self) -> Result<Transaction>;
}
pub struct Transaction { /* start_ts, a BTreeMap write buffer, the resolved primary */ }
impl Transaction {
    pub fn get(&self, key: &[u8]) -> Result<Option<Bytes>>;        // read-your-writes
    pub fn scan(&self, start: &[u8], end: &[u8], limit: u32) -> Result<Vec<(Bytes, Bytes)>>;
    pub fn put(&mut self, key: &[u8], value: &[u8]);               // buffered, no I/O
    pub fn delete(&mut self, key: &[u8]);                          // buffered, no I/O
    pub fn commit(self) -> Result<Option<u64>>;                    // Some(commit_ts), or None if empty
    pub fn rollback(self) -> Result<()>;
}
```

## 4. Test list

| Kind | Where | What it pins |
|---|---|---|
| golden | `esker-txn/tests/golden.rs` | every record and key encoding, byte for byte |
| proptest | `esker-txn/tests/proptest_codec.rs` | round trip; canonical encoding; **no panic on arbitrary bytes** (invariant 9) |
| unit | `esker-txn/src/codec.rs` | each malformed input class: bad kind, truncation, trailing bytes, a rollback lock, a short value over the cutoff |
| unit | `esker-txn/src/key.rs` | the seek boundary at `ts == commit_ts`, and that a seek never leaves the key's prefix |
| unit/matrix | `esker-txn/tests/protocol.rs` | write-write conflict; lock conflict; **each alone**; idempotent re-prewrite; rollback marker; roll-forward vs roll-back by primary state; TTL expiry either side of the boundary; commit without a lock; rollback after commit |
| unit | `esker-proto/tests/messages.rs` | round trip of every `TxnKv` message; unknown method is an error |
| golden | `esker-proto/tests/golden/` | the wire bytes of every `TxnKv` message |
| unit | `esker-client/tests/txn.rs` | prewrite order (primary strictly first); secondaries grouped per region; `Locked` → resolve → retry; TTL wait with a jumping clock; `AmbiguousResult` on a prewrite resolved by primary state; retries bounded; rollback |
| model | `esker-engine/tests/model.rs` | `DeleteRange` in the random-op model against the `BTreeMap` reference |
| golden | `esker-engine/tests/golden/` | the new SST range-deletion block |
| crash | `esker-engine/tests/crash_*.rs` | still green with range tombstones in the WAL |
| **deferred** | — | bank test, crash-between-prewrite-and-commit, GC, `txn-put`/`txn-get` bench |

## 5. Risks

1. **The seek direction.** `enc_ts` is the complement, so *newer sorts first* and "the newest
   version at or below `ts`" is a forward seek to `'x' ++ k ++ !ts`. Getting the direction wrong
   reads the oldest version instead of the newest and every test that uses one version passes. The
   boundary `ts == commit_ts` is tested explicitly, in both `key.rs` and the protocol matrix.
2. **Prewrite has two checks, not one.** The `write` CF for a commit newer than `start_ts` *and*
   the `lock` CF for any lock. Dropping either is a silent isolation break, so the matrix has a case
   for each **alone** as well as together.
3. **Primary-first is the commit point.** A secondary committed before its primary makes a
   transaction that no resolver can classify. The API makes it unrepresentable: `commit_secondary`
   demands a `PrimaryCommitted` token, and the only way to get one is to consume the primary's plan
   after applying it.
4. **Rollback markers vs GC.** A marker at `commit_ts == start_ts` must outlive the safepoint until
   the safepoint passes `start_ts`; otherwise a late prewrite from a transaction everyone agreed was
   dead succeeds. The encoding carries what the filter needs; the filter itself is deferred, and
   `docs/txn-spec.md` §7 states the rule so the deferred half cannot get it wrong quietly.
5. **`esker-proto` has another writer.** The phase-4 store lane is editing `messages.rs`. Unit 4
   waits for that file to be clean and adds the `TxnKv` service in one commit; the messages
   themselves live in a new `src/txn.rs`, so the shared file takes an eight-line change and nothing
   else.
6. **Unit 6 is the big one.** Range tombstones touch the memtable, both iterators, `get`, flush and
   compaction, and they interact with snapshot seqnos. It is scheduled last and stops where it
   stops; a half-built range tombstone that the read path does not honour is exactly the failure
   `docs/DESIGN.md` §4.7 refuses, so the refusal in `Db::write` is lifted in the *same commit* that
   makes reads honour them, and not before.

## 6. What this lane will not do

- No pessimistic locks, no async commit, no 1PC, no `min_commit_ts` — `docs/DESIGN.md` §15 keeps
  them open, and Percolator as published has none of them.
- No SSI. Write skew is legal under snapshot isolation; `docs/txn-spec.md` §6 says so and says what
  adding SSI would cost, rather than leaving a reader to discover it from a failing test.
- No `esker-store` handler, no compaction filter, no bank test (§1).
- No change to `ProtoError::Locked`'s shape: `lock_info` stays opaque bytes that `esker-txn`
  encodes and decodes, because `esker-proto` must not learn Percolator's layout (its own TODO says
  so, and the alternative is a protocol crate that changes whenever a lock gains a field).

## 7. Range tombstones (unit 6), sketched

Written out here because the ADR follows this shape and because it is the unit most likely to be cut
short. `docs/DESIGN.md` §15 lists "range tombstones design" as an open question; the ADR closes it.

- **WAL**: entry kind 2 already exists and is frozen (§4.3). Reused, not extended.
- **Memtable**: a per-CF ordered list of `[begin, end)` ranges with the seqno that deleted them,
  beside the skiplist rather than in it — a range is not a key and giving it one would put it in
  scan results.
- **SST**: a new block, listed in the footer beside the filter block, holding the ranges a flush or
  compaction carried. Its own golden.
- **Reads**: `get` and both iterators consult the ranges covering the key at their snapshot seqno
  before accepting a value; a tombstone with a higher seqno hides it.
- **Compaction**: a covered version is dropped outright at the bottom level, and carried forward
  above it, on the same rule that governs point deletes.
- **`Db::write`**: the `Error::Unsupported` refusal is deleted in the commit that makes reads honour
  the entry, and `esker-store`'s scan-plus-point-deletes workaround (ADR 0006) is left in place for
  the store lane to remove — it is their file.

## 8. Progress

- [x] unit 1 — plan and `docs/txn-spec.md`
- [x] unit 2 — record and key encodings, goldens, proptests (77 tests, ADR 0015)
- [x] unit 3 — the Percolator decision library and its matrix (39 protocol cases)
- [x] unit 4 — `TxnKv` on the wire (ADR 0016, 141 tests in `esker-proto`)
- [x] unit 5 — `TxnClient` (23 cases, and the `Router` both clients now share)
- [x] unit 6 — engine range tombstones (ADR 0017, decision 6 ruled option (ii)): the block, the
  memtable list, `get`, both iterators, flush, the discharge compaction, and the lifted refusal

## 9. What changed, and why

**The key layout is not §3's.** Appending a fixed version suffix to a raw user key only keeps one
key's versions contiguous when keys are prefix-free, and `TxnKV` keys are not: raw, `"a"@0` sorts
after `"ab"@MAX`, so a seek for `a`'s newest version lands inside `ab`'s — and the prefix check that
should catch it passes, because `"a"` *is* a prefix of `"ab"`. `esker-keys`' own property test
excludes the case with a `prop_assume!`. `esker-txn::key` group-encodes first; `docs/txn-spec.md` §2
and ADR 0015 work the bytes through, and `docs/DESIGN.md` §3 and §8 were corrected in the same
commit.

**The commit point is a type, not a comment.** `commit_secondary` demands a `PrimaryCommitted`
token, and the only source of one is `PrimaryCommit::applied()`, which consumes the plan the caller
has just made durable. §5's risk 3 asked for "type-state or runtime assert"; this is the former, and
it costs nothing.

**`TxnKv` answers with a status, not only with errors.** ADR 0016 records the line: a lock, a
`NotLeader` or a `ServerIsBusy` is a refusal to *serve* and belongs in the error channel, where the
client's retry machinery already handles it; "you conflicted", "you were rolled back", "you already
committed", "your lock is gone" are determinations about the transaction, which no retry changes.
The second group travels in the response as a `TxnStatus`. Writing the client is what found it —
unit 4 shipped `Prewrite` with an empty body, and there was nowhere to put a conflict.

**`RawClient`'s call loop became `Router`.** Both clients want the same five steps, and a second
copy would be a second place for `docs/DESIGN.md` §10 to drift. `StoreTransport::call` now answers a
whole `Response` rather than a `RawKvResp`, because unwrapping one service at the transport would
have meant a second loop for the other. Nothing above the trait changed shape.

**Two things the goldens found.** `esker-proto`'s `damaged_bodies_never_panic` was fuzzing
`which in 0usize..12` against a list that had grown to 31, so nineteen messages — every `Pd` one
included — had never been damaged; it indexes against the list's real length now. And the coverage
sweep that demands a golden request *and* response per method is what made the eight `TxnKv` pairs
mandatory rather than a judgement call.

**Unit 6 landed in two commits, and the split was the point.** ADR 0017 decision 5 says the refusal
in `Db::write` is lifted in the *same* commit that teaches the read paths to honour tombstones — a
database that accepts a range delete it does not honour is worse than one that refuses it, the
refusal being loud and the acceptance silent. So the format landed first with the refusal intact, and
the refusal went in the commit that made reads honour it.

Between them, decision 6 was ruled: **discharge at L0, never propagate**. Widening a file's key
bounds to span its tombstones is safe at L0 and silently wrong below it — `search_levels` finds one
file per level with `partition_point(|f| f.largest < target)`, correct only while a level partitions
the key space — so nothing below L0 ever holds one. Implementing the ruling turned up three things
the design did not predict:

- a **trivial move** carries a tombstone to L1 without writing a byte, so a compaction whose inputs
  carry one is never trivial;
- a discharge that lands its outputs at a deeper level must also take every file *between* that
  overlaps what it is writing, or a file left at an intermediate level shadows the output with an
  older version of a key that has nothing to do with the tombstone;
- a tombstone above the compaction floor cannot be consumed at all, because a snapshot older than the
  delete still has to see what was deleted — so the discharge waits, which is what "scheduled" means
  when the schedule has to.

**One edit outside the lane.** Adding `Request::TxnKv` made `esker-store`'s dispatch `match`
non-exhaustive and the workspace stopped compiling. The arm added there is six lines answering
`Unsupported` with the method's name — the store half of phase 5 is deferred (§1), and a client that
reaches for it should meet a refusal that says so rather than a default or silence. It was swept
into the store lane's commit `33e95e9` before this lane could stage it on its own.
