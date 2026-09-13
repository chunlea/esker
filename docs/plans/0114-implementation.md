# ADR 0114 — how §2 (a) and §3 (ii) would be built

**A plan, not a build.** Written 2026-09-13 while [ADR 0114](../adr/0114-a-unique-key-being-written-waits-at-read-committed.md)
and both of its questions (`esker-coord/QUESTION-s1.md`) wait for the user. Nothing below may be built
before they are ruled: §2 (a) is a wire and Raft-log format change (`CLAUDE.md`, "ask before doing"),
and §3 (ii) is one of three answers to a semantic question.

**Every file, function, line and number below was read in the tree at `b36e5d4f`.** Anything that was
not is marked **(unverified)**, and is the first thing to read when the unit starts.

---

## §2 (a) — a `Check` that carries the statement's read timestamp

The defect is debt #91: at READ COMMITTED, `SELECT … FOR UPDATE`'s eager lock is a `Check` mutation
validated at the transaction's `start_ts`, so a row another transaction committed after `BEGIN` is
always `40001`. The fix gives the eager lock's `Check` the statement's read timestamp.

### What exists today

**The wire** — `crates/esker-proto/src/txn.rs`:

* `pub enum TxnMutation` (line 251): `Put { key, value, read_ts: Option<u64> }`,
  `Delete { key, read_ts: Option<u64> }`, `Check { key }`, `CheckRange { start, end }`.
* `TxnMutation::tag` (lines 322–331): `Put { read_ts: None }` = **1**, `Delete { read_ts: None }` =
  **2**, `Put` with a timestamp = **3**, `Delete` with one = **4**, `Check` = **5**, `CheckRange` = **6**.
* `TxnMutation::encode` (line 333): `u8 tag`, then — `Put`: `bytes key, bytes value[, varint read_ts]`;
  `Delete`: `bytes key[, varint read_ts]`; `Check`: `bytes key`; `CheckRange`: `bytes start, bytes end`.
  Bytes are varint-length-prefixed and integers LEB128 (the header of `tests/golden/messages.hex`).
* `TxnMutation::decode` (line 361): an unknown tag is
  `Err(DecodeError::invalid("mutation.tag", format!("{tag} is not a mutation kind")))` (line 388).
  The unit test `an_unknown_mutation_tag_is_an_error` uses tag **9**.

**The log** — `crates/esker-store/src/txn_command.rs`:

* `pub enum TxnWrite` (line 56) mirrors the wire: `Put`/`Delete` with `read_ts: Option<u64>`,
  `Check { key }`, `CheckRange { start, end }`. `TxnWrite::to_wire` turns one back into a
  `TxnMutation`; `TxnCommand::from_request` turns a `TxnKvReq::Prewrite` into a
  `TxnCommand::Prewrite` (`TxnMutation::Check { key } => TxnWrite::Check { key }`, line 235).
* `TxnCommand::encode_to` (line 309): `u8 VERB_PREWRITE (1), varint start_ts, bytes primary,
  varint ttl_ms, varint count`, then per write a `u8` kind and its fields — kinds **1–6**, with the
  same numbers and layouts as the wire tags.
* `TxnCommand::decode_from` (line 421): an unknown kind is
  `Err(ProtoError::corrupt("txn command", format!("{other} is not a write kind")))`.
* There is **no byte golden for the log**. Its tests are round trips — `every_command_round_trips`,
  and `a_malformed_command_is_an_error`, which uses verb 9 and write kind 9.

**The decision** — made at apply, on every peer:

* `crates/esker-store/src/apply.rs` lines 609–617: `writes.iter().map(TxnWrite::to_wire)`, then
  `crate::txnkv::prewrite(db, batch, start_ts, primary, ttl_ms, &mutations)`.
* `crates/esker-store/src/txnkv.rs::prewrite` (line 559) builds an `esker_txn::Prewrite` per key.
  Its `read_ts` is `read_ts.unwrap_or(start_ts)` for `Put`/`Delete` and **`start_ts` for
  `Check`/`CheckRange`** (lines 617–623). Its `op` is `Op::Check` for both checks (line 629).
* `esker_txn::check_prewrite` (`crates/esker-txn/src/percolator.rs` line 404), step 2:
  `newest_write_after(key, request.read_ts)` answers `Conflict` (line 429). `Op::Check` commits as
  `Kind::Lock` (line 63). **`esker-txn` needs no change**: it already validates each key against the
  request's own `read_ts`.

**The client** — `crates/esker-client/src/txn.rs`:

* `statement_ts: Option<u64>` (line 524), set by `begin_statement` (line 999), `restart_statement`
  (line 1017) and `reading_at` (line 701). `read_ts: BTreeMap<Bytes, u64>` (line 521) is stamped by
  `stamp` (line 1051) — earliest stamp wins — for **writes only**.
* `Transaction::lock` (line 742) → `pin_primary` (line 860) or `prewrite_once` (line 892) →
  `mutations_for` (line 1626), where a key that is not in the write buffer becomes
  `TxnMutation::Check { key }` (line 1644).
* `Transaction::commit` (line 1416) prewrites **every eagerly locked key again** as a secondary
  (`self.locked` is chained into `secondaries`), through the same `mutations_for`.
* `crates/esker-client/src/wire.rs::txn_payload_size` (line 101) matches `TxnMutation::Check { key }`.

**The SQL layer**:

* `StoreTxn::lock` (`crates/esker-sql/src/backend/store.rs` line 322) sends a `Reach::Cluster`
  lock to `Transaction::lock`.
* `Executor::in_a_transaction` calls `Txn::begin_statement` **only inside
  `if self.isolation().waits()`** (`crates/esker-sql/src/exec/mod.rs` line 1029). So `statement_ts` is
  set at READ COMMITTED and never at REPEATABLE READ or SERIALIZABLE — which is what keeps PostgreSQL's
  e3 (`40001` at REPEATABLE READ) true after the change.

### The change, step by step

1. **Wire — `TxnMutation::Check { key, read_ts: Option<u64> }`.** The shape `Put` and `Delete` took
   for ADR 0057 §4. **Tag 5 stays `read_ts: None`** and keeps its bytes. **Tag 7 is `read_ts: Some`**,
   laid out `u8 7, bytes key, varint read_ts`:
   * `tag`: `Self::Check { read_ts: None, .. } => 5`, `Self::Check { .. } => 7`;
   * `encode`: after the key, `if let Some(read_ts) = read_ts { out.put_varint(*read_ts) }`;
   * `decode`: `7 => Ok(Self::Check { key: take(input, "mutation.key")?, read_ts: Some(input.get_varint("mutation.read_ts")?) })`.

   `CheckRange` stays tag 6 at `start_ts`: a range is only ever a SERIALIZABLE read set.
   *Not taken:* a separate `CheckAt` variant. Tags 3 and 4 set the precedent of one variant with an
   optional timestamp, and a second variant would need its own arm in every match.
   *While there:* the doc comments on `TxnMutation::Put::read_ts` ("`None` on the wire today…") and
   `TxnWrite::Put::read_ts` ("`None` in the log today…") predate the ruling that put tags 3 and 4 on
   the wire, and are stale.
2. **Log — `TxnWrite::Check { key, read_ts: Option<u64> }`, kind 7**, laid out
   `u8 7, bytes key, varint read_ts`. Arms in `encode_to` and `decode_from`
   (`7 => TxnWrite::Check { key: bytes(input, "txn.write.key")?, read_ts: Some(varint(input, "txn.write.read_ts")?) }`),
   and `from_request` and `to_wire` carry `read_ts` through. Kinds 1–6 do not move.
3. **Store — `txnkv.rs` lines 617–623**: `TxnMutation::Check { read_ts, .. } => read_ts.unwrap_or(start_ts)`,
   and `CheckRange` stays `start_ts`. `op` does not change.
4. **Client — `txn.rs`**:
   * `Transaction::lock`: when `self.statement_ts` is `Some(ts)`, record the key before prewriting it —
     `self.read_ts.entry(key.clone()).or_insert(ts)`. That is `stamp`'s earliest-wins rule, so a
     statement re-run after a wait keeps the timestamp the lock was actually validated at.
   * `mutations_for`, the not-buffered arm:
     `None => TxnMutation::Check { key: key.clone(), read_ts: self.read_ts.get(key).copied() }`.
     A read-set check never has an entry — SERIALIZABLE sets no statement timestamp — so it stays
     tag 5. An eager lock taken at READ COMMITTED is tag 7, **both when it is taken and when `commit`
     prewrites it again**.
   * `wire.rs::txn_payload_size`: `Check { key, .. }`, plus one field for the timestamp.
5. **SQL — no product change is needed.** The statement timestamp already reaches the client through
   `begin_statement`. One window remains and is not in this plan: a commit that lands between the
   statement's snapshot and the lock still refuses. Restarting the statement there, as
   `changed_since_statement` does for writes, is the follow-on, to be measured first.

### Old peers

* **Wire — an old store refuses the request and keeps the connection, and the client surfaces it.**
  `TxnKvReq`'s decoder reads a `Prewrite`'s mutations one at a time (`crates/esker-proto/src/txn.rs`
  line 683), and tag 7 is `DecodeError::invalid("mutation.tag", "7 is not a mutation kind")` (line 388).
  `ConnectionState::on_request` (`crates/esker-proto/src/transport/server.rs` lines 355–365) makes it
  `ProtoError::InvalidRequest` (`impl From<DecodeError> for ProtoError`, `error.rs` line 650), answers an
  `Error` frame for that request id and returns `FrameAction::Continue`: *"this is a caller error, not
  corruption, and the connection survives it."* `InvalidRequest` is not `is_retryable` (`error.rs`
  lines 446–455), so `esker_client::retry::classify` surfaces it (`retry.rs` line 216) and the router's
  `terminal` hands it back as `Error::Store`, with no second attempt (`router.rs` line 665). Its
  `outcome()` is `NotApplied` (`error.rs` line 389), so `Error::changed_nothing` holds. `esker-sql`'s
  `translate` files `ClientError::Store(_)` as `SqlError::StoreUnavailable`
  (`crates/esker-sql/src/backend/store.rs` line 656), which is `08006`. A READ COMMITTED `FOR UPDATE`
  that reaches an old store is `08006`, having locked nothing and misread nothing.
* **Log — an old follower drops the region, and drops it again after every restart.** Raft appends a
  kind-7 entry without reading it, and it fails at apply. `PeerCore::apply`
  (`crates/esker-store/src/peer.rs` line 695) decodes with `Command::decode` (line 708) — `TAG_TXN` is
  `TxnCommand::decode_from` (`apply.rs` line 268), which answers
  `ProtoError::corrupt("txn command", "7 is not a write kind")` — and returns before the batch that
  carries `apply_index` is staged and written (lines 760–772). Its doc gives the reason: *"A payload
  that cannot be **decoded** cannot be applied at all, and skipping it would leave this peer's state
  machine differing from every other's, so it stops the driver."* `PeerCore::drive` passes the error up
  (line 481), and the store's worker logs `the Raft driver failed for a region`, removes that region,
  fails its outstanding proposals and goes on serving its other regions
  (`crates/esker-store/src/driver.rs` lines 421–430). `apply_index` has not moved, so a restart replays
  the same entry and stops at it again, and a columnar copy resuming over it refuses the same way
  (`crates/esker-store/src/columnar/region.rs` line 705). **Nothing diverges; availability is what is
  lost** — the old replicas of each region such an entry reaches, and the region itself once they are a
  majority.
* The deployment rule is the one `VERB_RELEASE_LOCK`'s doc already states (`txn_command.rs` lines
  47–52): every store understands the addition before any client sends it. The two paths above are what
  make it a rule: a client that goes first costs `08006` on every such lock, and a leader that goes first
  costs its old followers their copies of the region.

### Goldens

* **`crates/esker-proto/tests/golden/messages.hex`** — one row after `txn-prewrite-checks` (line 124):

  ```text
  request txn-prewrite-check-at 0302010203042a0170b8170107016332
  ```

  `0302` Prewrite 0x0203 · `01 02 03 04` the header { 1, {2, 3}, 4 } · `2a` start_ts 42 ·
  `01 70` primary `p` · `b8 17` ttl_ms 3000 · `01` one mutation · **`07` tag 7 · `01 63` key `c` ·
  `32` read_ts 50**.

  With it, a third row in `golden_txn_prewrite_requests` (`crates/esker-proto/tests/messages.rs`
  line 553), whose doc already says it is *"the list that grows when one is added"*:
  `TxnMutation::Check { key: "c", read_ts: Some(50) }`. The two existing rows must stay byte-identical;
  their `Check { key }` literal gains `read_ts: None`. `the_goldens_cover_every_method_and_every_error_code`
  (line 1648) is unaffected, because no method is added.
  *Worth adding in the same change:* the golden has **no row for tags 3 and 4** — the TxnKv section
  holds only `txn-prewrite` and `txn-prewrite-checks` — so a `txn-prewrite-read-ts` row would pin them.
* **Log**: `every_command()` in `txn_command.rs`'s tests gains a `TxnWrite::Check` with
  `read_ts: Some(_)`, so `every_command_round_trips` covers kind 7. `a_malformed_command_is_an_error`
  keeps kind 9.

### Tests

| crate | test | asserts |
|---|---|---|
| `esker-proto` | the golden row, through `golden_txn_prewrite_requests` | tag 7's bytes, both ways |
| `esker-store` | `txn_command.rs` round trip | kind 7 round-trips |
| `esker-store` | new, beside `tests/a_lock_is_not_a_version.rs` (which already prewrites `TxnMutation::Check`) | commit `k` at 20; at start_ts 10, `Check { k, read_ts: Some(25) }` is `Ok` and leaves a lock; `read_ts: None` is `Conflict { 20 }` as today; `read_ts: Some(15)` is `Conflict { 20 }` |
| `esker-client` | an eager lock after `begin_statement(ts)` sends `read_ts: Some(ts)`, `commit` re-sends the same, and with no statement timestamp it is `None` | harness **(unverified)**: `tests/txn.rs` or `tests/store_model.rs` |
| `esker-sql` | `concurrent_unique_insert.rs`: remove `#[ignore]` from `a_for_update_of_a_row_committed_after_the_transaction_began_takes_the_lock` and `relations_test_s_find_or_create_by_duel_commits_both_sessions` | PostgreSQL's e1 and e2, and the Rails duel |
| `esker-sql` | new REPEATABLE READ twins, same file | e3 stays `40001`, e4 finds no row |

Regression guards to run: `store_locking` (among them `write_skew_is_refused_against_real_stores` and
`a_deadlock_inside_a_savepoint_is_recoverable_against_real_stores`), `cross_node_deadlock`,
`row_locking`, `serializable`, and the ignored `lock_cost` once, to see what the extra varint costs.
Tests that construct `TxnMutation::Check` and gain `read_ts: None`:
`crates/esker-store/tests/a_collection_under_many_keys.rs` (line 254),
`a_lock_is_not_a_version.rs` (line 160), `a_deleted_key_goes_whole.rs` (line 198),
`crates/esker-proto/tests/messages.rs` (line 590).

### The minimal counterfactual

Take out **step 3 alone** — `TxnMutation::Check { .. } => start_ts` again at `txnkv.rs` line 623 — and
keep the wire, the log and the client. The client still sends tag 7, and the store still validates at
`start_ts`: `a_for_update_of_a_row_committed_after_the_transaction_began_takes_the_lock` is red again
with `a commit at … beat this transaction at …`, and the store test's `read_ts: Some(25)` case answers
`Conflict`.

### Files

* `esker-proto`: `src/txn.rs`, `tests/messages.rs`, `tests/golden/messages.hex`.
* `esker-store`: `src/txn_command.rs`, `src/txnkv.rs`, one new or extended test, and the three tests
  above that construct `TxnMutation::Check`.
* `esker-client`: `src/txn.rs` (`lock`, `mutations_for`), `src/wire.rs` (`txn_payload_size`), a test.
* `esker-txn`: none.
* `esker-sql`: tests only.
* Docs: ADR 0114's status; `docs/DESIGN.md` §8, which names the eager lock's "`Check` mutation (tag 5)"
  (line 839); debt #91 moves to §2.

### Risks

* A new client talking to an old store fails every READ COMMITTED `FOR UPDATE` with `08006`, and a new
  leader costs its old followers their copies of the region (*Old peers*). Stores go first — all of them.
* One more entry in the client's `read_ts` map per eager lock, for the life of the transaction.
* The read-to-lock window in step 5 still refuses.

---

## §3 (ii) — `40001` at SERIALIZABLE when the lost key had been read by an earlier statement

Today a unique conflict at SERIALIZABLE is always renamed `23505` at `COMMIT`. PostgreSQL answers
`40001` when the transaction had read the key and `23505` when it had not (ADR 0114 cases 06 and 09
against 07). The answer (ii) needs one bit per unique key, *"was this read before it was written"*,
and the one moment that bit can be taken.

### What exists today

**The read set:**

* `StoreTxn` (`crates/esker-sql/src/backend/store.rs`) has three fields: `validating: bool`
  (line 196), `read_keys: RefCell<BTreeSet<Vec<u8>>>` (line 199) and
  `read_ranges: RefCell<Vec<(Vec<u8>, Vec<u8>)>>` (line 200).
* `record_key` (line 238) and `record_range` (line 248) do nothing unless `validating`, and skip the
  catalog's `META` prefix.
* **`StoreTxn::get` records every point read** (`self.record_key(key)`, line 450), and that includes
  `write_row`'s own uniqueness probes.
* `MemoryTxn` (`crates/esker-sql/src/backend/mod.rs`) has the same fields (`validating` line 741,
  `read_keys` line 746) and `record_key` (~line 800), and validates its keys at commit
  (`if self.validating { for key in self.read_keys.borrow().iter()`, line 1131).
* Recording is switched on by `Txn::validate_reads` (trait, `backend/mod.rs` line 280), which
  `in_a_transaction` calls with `isolation() == Isolation::Serializable` (`exec/mod.rs` lines 1035 and
  1075).
* A savepoint copies the read set with `Txn::read_set` / `Txn::restore_read_set` (lines 371 and 379;
  `pub struct ReadSet { keys, ranges }`, line 441). `savepoint::Recording` forwards all three
  (`crates/esker-sql/src/exec/savepoint.rs` lines 337, 341, 374).
* **The `Txn` trait has three implementors**: `StoreTxn` (`store.rs` line 271), `MemoryTxn` (`mod.rs`
  line 848), `Recording` (`savepoint.rs` line 268).

**Where the checks go out:**

* `StoreTxn::commit` (`store.rs` line 542) hands `read_keys` and `read_ranges` to
  `esker_client::Transaction::checking` (line 559).
* `checking` (`crates/esker-client/src/txn.rs` line 711) **drops a key already in the write buffer** —
  *"its write lock covers the same interval"*. So a key that was read and then inserted is validated by
  its own `Put`'s prewrite, and a lost race comes back as `TxnConflict` naming that key.

**Where it becomes `23505`:**

* `Executor::explain_conflict` (`exec/mod.rs` line 2434) renames a `SerializationFailure` whose key is
  one of `Written::unique_keys` (`Written` line 4312; `Unique { key, constraint, detail }` line 4337).
* **The block's `commit` (line 4887) runs `end_of_block` (line 1809) first**, and that resets
  `transaction_isolation` to the session default (lines 1822–1826). By the time `explain_conflict`
  runs, the level can no longer be asked.

**The probes that fill `unique_keys`** — `exec::dml::write_row` (line 1035):

* the primary key: `if txn.get(&key)?.is_some()` (line 1059), push at line 1078;
* each by-value unique entry: `if txn.get(&entry.key)?.is_some()` (line 1154), push at line 1160, after
  ADR 0114 §1's lock;
* `ON CONFLICT`'s `conflicting_row` (line 1607) reads the arbiter's entry before `write_row` does.

**How `find_by` reads the key:**

* `WHERE nick = $1 LIMIT 1` over a unique index plans as `Node::IndexLookup` (`exec/query.rs`
  line 2979) and runs as a point access (`exec/cursor.rs` line 524, `Kind::Point`; the arm at line 741).
* The arm calls `point(self.txn, self.tenant, node, Some(&namer))` (line 746). On `Node::IndexLookup`,
  `point` (line 1208) builds `row::index_key(tenant, *table_id, *index_id, key, None)` (line 1234),
  reads it with `txn.get(&index_key)` (line 1235), and then reads the row with `txn.get(&key)`
  (line 1240).
* Both `get`s record their key. `StoreTxn::get` does it first thing (`backend/store.rs` lines 449–450);
  `MemoryTxn::get` does it after its buffer, which holds this transaction's own writes and is not a read
  (`backend/mod.rs` lines 870–887). `record_key` (`store.rs` line 238, `mod.rs` line 801) keeps a key
  only while `validating`, and never a catalog key.
* **The entry key it records is the key `write_row` probes.** `exec::index::entry` (`exec/index.rs`
  line 39) gives a by-value entry no suffix — `row::index_key(tenant, table.id, index.id, &values, None)`
  (lines 61–65) — which is the call `point` makes, with the same `None`, so one value is one key. An
  earlier `find_by` of `bob` therefore leaves exactly `entry.key` in `read_keys`, and
  `has_read(&entry.key)` sees it.

### The change

1. **A required `Txn` method** — no default, implemented by all three:
   `fn has_read(&self, key: &[u8]) -> bool`, true when `key` is in the read set's keys or inside one of
   its ranges (`start <= key < end`). `Recording` forwards to its inner transaction. A default here
   would be the silent opt-out the trait's own docs refuse for `lock`, `locks` and
   `get_without_waiting`.
2. **`Unique` gains `read_first: bool`.**
3. **`write_row` takes the bit before each probe**, not after:
   `let read_first = executor.isolation() == Isolation::Serializable && txn.has_read(&key);`
   immediately before line 1059 (with `&key`) and line 1154 (with `&entry.key`), stored in the pushed
   `Unique`. Asking first is the whole distinction. The probe records the key a moment later, so an
   `INSERT`'s own probe can never count as its earlier read, and a `find_by` in an earlier statement
   — or `conflicting_row` in the same one — always does. The level is captured here, at the `INSERT`,
   so `end_of_block`'s reset before `explain_conflict` does not matter.
4. **`explain_conflict`**: a lost key whose `Unique` has `read_first` stays `SerializationFailure`
   (`40001`); any other is renamed `23505` as today. The same filter applies in the no-key second look.

### What it answers, against ADR 0114's capture

| case | PostgreSQL 19 | today | after (ii) |
|---|---|---|---|
| 09 — SERIALIZABLE, read first, holder committed before the `INSERT` | `40001` at the `INSERT` | `23505` at `COMMIT` | `40001` at `COMMIT` |
| 06 — SERIALIZABLE, read first, holder live | waits, `40001` at the `INSERT` | `23505` at `COMMIT` | `40001` at `COMMIT` |
| 07 — SERIALIZABLE, never read | waits, `23505` at the `INSERT` | `23505` at `COMMIT` | `23505` at `COMMIT` |
| 13 — SERIALIZABLE `ON CONFLICT DO NOTHING` | waits, `40001` | **(unverified)** | `40001` at `COMMIT` (`conflicting_row` read the arbiter) |
| REPEATABLE READ, either way | `23505` | `23505` at `COMMIT` | unchanged — `read_first` is false |

The code moves to PostgreSQL's in all four SERIALIZABLE rows. The statement does not: moving the
refusal to the `INSERT` would need a predicate lock in the store, and that is outside (ii).

### Tests

* Real stores, `crates/esker-sql/tests/concurrent_unique_insert.rs`:
  * remove `#[ignore]` from `serializable_refuses_a_unique_key_committed_after_it_was_read_with_40001` (case 09);
  * add case 07 → `23505`;
  * add case 13 → `40001`.
* In process, `crates/esker-sql/tests/serializable.rs` (`MemoryBackend`): the same three shapes.
* `has_read` on `StoreTxn` and on `MemoryTxn`: key membership, range containment, and a catalog key that
  is never recorded.
* A savepoint test that `Recording` forwards `has_read`: a read made before `SAVEPOINT` is still seen
  inside it, and `ROLLBACK TO` a savepoint taken before the read no longer sees it.
* Guards:
  * `insert.rs::a_concurrent_duplicate_is_reported_as_a_duplicate` and
    `real_backend.rs::a_lost_race_on_a_unique_index_is_a_duplicate_key` — both REPEATABLE READ, both stay
    `23505`;
  * `transaction_timeouts.rs`, whose rewritten row stays `40001`;
  * `serializable.rs`;
  * `store_locking.rs::write_skew_is_refused_against_real_stores`.

### The minimal counterfactual

Set `read_first` to `false` unconditionally in `write_row`. Case 09's test answers `23505` again, and
case 07's stays green — it is the control that shows the bit, and not something else, is deciding.

### Files

`esker-sql` only:

* `src/backend/mod.rs` — the trait method, and `MemoryTxn`'s implementation;
* `src/backend/store.rs` — `StoreTxn`'s;
* `src/exec/savepoint.rs` — `Recording`'s forward;
* `src/exec/mod.rs` — `Unique::read_first` and `explain_conflict`;
* `src/exec/dml.rs` — `write_row`'s two probes;
* the tests above.

No format, no wire.

### Risks

* A statement that reads a unique key and inserts it in the same statement — `INSERT … SELECT … WHERE
  nick = …` over the same table — counts as "read first". PostgreSQL's predicate lock would count it too.
* Nothing new to keep in memory: the read set already exists.

---

## What neither half does

* Build anything before the rulings.
* Make REPEATABLE READ or SERIALIZABLE **wait** for a live holder of a unique value, as PostgreSQL
  does (ADR 0114, "What stays declared").
* Move SERIALIZABLE's `40001` from `COMMIT` to the `INSERT`.
* Close §2's read-to-lock window (step 5).
