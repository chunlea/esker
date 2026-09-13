# ADR 0114 — how §2 (a) and §3 (ii) would be built

**A plan, not a build.** Written 2026-09-13 while [ADR 0114](../adr/0114-a-unique-key-being-written-waits-at-read-committed.md)
and both of its questions (`esker-coord/QUESTION-s1.md`) wait for the user. Nothing below may be built
before they are ruled: §2 (a) is a wire and Raft-log format change (`CLAUDE.md`, "ask before doing"),
and §3 (ii) is one of three answers to a semantic question.

**§3 is built** (unit I, 2026-09-13). The coordinator ruled question 2 — (ii), with step 5's arbiter
rule — under the mandate to close the gaps, and the user may overrule it. It was built as §3 below
describes, under two names this plan did not have: the arbiter's list is `arbitrated`, marked by
`mark_arbitrated`, and step 3's test is `read_before_writing`. §2 (a) still waits.

**Every file, function, line and number below was read in the tree at `b36e5d4f`.** Main's `fc8333c8`,
merged into this branch as `aad1dca3`, moved cited lines only in `exec/mod.rs` and `exec/dml.rs`; those
are given as they stand at `aad1dca3`, and its other hunks fall after every line cited here.

**Five points were first left unverified**, and unit H settled them on 2026-09-13: four by reading the
code, one by running this node beside PostgreSQL 19. Two changed the design — when §2's eager lock is
stamped and when the stamp goes (step 4), and §3's arbiter rule (step 5) — and are marked *(unit H)*.

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
  `if self.isolation().waits()`** (`crates/esker-sql/src/exec/mod.rs` line 1035). So `statement_ts` is
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
   * `Transaction::lock` (line 742) **stamps the key before `pin_primary`** — right after its early
     `Taken` for a key already buffered or locked (lines 744–746) — with `self.stamp(&key)` (line 1051),
     which records `statement_ts` when there is one. *(unit H)* Before `pin_primary`, and not merely
     before the `prewrite_once` at the end (line 758): with nothing buffered, the key being locked **is**
     the primary, and its lock goes out from inside `pin_primary` (line 870). That is Rails' shape — a
     `FOR UPDATE` that is the transaction's first write — and stamped any later it would still be tag 5,
     with #91's test still red.
   * **The stamp stays only if the call answers `Taken`.** *(unit H)* On `Held` or on an error, `lock`
     removes the stamp it added. A lock that was not taken was validated at nothing, and a leftover stamp
     would pin every later attempt at that key through `stamp`'s `or_insert`, because nothing else clears
     it: `restart_statement` (line 1017) drops only the stamps of keys the statement wrote. Later attempts
     are routine — `esker-sql`'s `wait_for_the_lock` retries a `Held` lock (`exec/mod.rs` lines 457–481),
     and a transaction can lock the row again after `ROLLBACK TO SAVEPOINT`.
   * **`release` drops the stamp of each key it gives back** *(unit H)* — beside `self.locked.remove`
     (lines 816 and 833) — unless the key is still in the buffer, where the stamp is its write's. A
     statement that locks a released row again must validate at its own snapshot.
   * A stamp that stays is earliest-wins, as `stamp` already is, so `commit` re-sends a lock with the
     timestamp it was actually validated at.
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
| `esker-client` | `tests/txn.rs`: `begin_statement(ts)`, `lock(k)` with nothing buffered, `commit` | both `TxnPrewrite` calls carry `Check { k, read_ts: Some(ts) }` — the lock's from `pin_primary`, the commit's from `prewrite` (line 1567) |
| `esker-client` | `tests/txn.rs`: `put(a)`, then `begin_statement(ts)` and `lock(k)`, with `a < k` | `k` goes out from `prewrite_once` as `Check { k, read_ts: Some(ts) }` |
| `esker-client` | `tests/txn.rs`: `lock(k)` with no statement timestamp | `Check { k, read_ts: None }` — tag 5, today's bytes |
| `esker-client` | `tests/txn.rs`: `begin_statement(t1)`; the prewrite of `k` answers `TxnStatus::Locked` by an older, live transaction, which is not fatal (`is_fatal`, `esker-proto` `txn.rs` line 196), so `lock` is `Held` with no further call (`wound_or_wait`, line 946); `restart_statement(t2)`; `lock(k)` | the second `Check` carries `Some(t2)` *(unit H)* |
| `esker-client` | `tests/txn.rs`: `begin_statement(t1)`, `lock(k)`, `release(&[k])` (a scripted `TxnReleaseLock`), `begin_statement(t2)`, `lock(k)` | the second `Check` carries `Some(t2)` *(unit H)* |
| `esker-sql` | `concurrent_unique_insert.rs`: remove `#[ignore]` from `a_for_update_of_a_row_committed_after_the_transaction_began_takes_the_lock` and `relations_test_s_find_or_create_by_duel_commits_both_sessions` | PostgreSQL's e1 and e2, and the Rails duel |
| `esker-sql` | new REPEATABLE READ twins, same file | e3 stays `40001`, e4 finds no row |

**The client harness** *(unit H)* is `esker_client::testing::FakeTransport`, which *"records every call
it was given, so a test can assert on the requests as well as on the result"*
(`crates/esker-client/src/testing.rs`, module doc). `tests/txn.rs` already makes this kind of claim with
it: `a_key_written_twice_keeps_the_earlier_statements_read_timestamp` (line 1692) sets a statement
timestamp and asserts on the prewrite's `read_ts`, over `client` (line 99), `script_a_clean_commit`
(line 123) and `nth_txn` (line 138). Two things are new to it. **No file that uses `FakeTransport` takes
an eager lock yet.** And `pin_primary` registers the lease renewal (lines 885–886), whose thread sleeps a
wall-clock third of `LOCK_TTL_MS` = 3 000 ms (`renew.rs` lines 119–130) and then heartbeats through the
same router (line 170) — which the fake answers, unscripted, with `fake transport: no rule matched`
(`testing.rs` line 306). So these tests pick their calls by method, from `calls()` (line 339) filtered on
`Method::TxnPrewrite`, and not by index.

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

Step 4's two stamp rules have counterfactuals of their own *(unit H)*: keep the stamp on `Held`, and the
`Held` test's second `Check` carries `Some(t1)`; skip `release`'s, and the release test's does.

### Files

* `esker-proto`: `src/txn.rs`, `tests/messages.rs`, `tests/golden/messages.hex`.
* `esker-store`: `src/txn_command.rs`, `src/txnkv.rs`, one new or extended test, and the three tests
  above that construct `TxnMutation::Check`.
* `esker-client`: `src/txn.rs` (`lock`, `release`, `mutations_for`), `src/wire.rs` (`txn_payload_size`), tests in `tests/txn.rs`.
* `esker-txn`: none.
* `esker-sql`: tests only.
* Docs: ADR 0114's status; `docs/DESIGN.md` §8, which names the eager lock's "`Check` mutation (tag 5)"
  (line 839); debt #91 moves to §2.

### Risks

* A new client talking to an old store fails every READ COMMITTED `FOR UPDATE` with `08006`, and a new
  leader costs its old followers their copies of the region (*Old peers*). Stores go first — all of them.
* One more entry in the client's `read_ts` map per eager lock, until the lock is released or the
  transaction ends.
* A stamp that outlived a lock not taken would make every later attempt at that row `40001` — silently,
  and only after a wait or a savepoint rollback — which is why step 4 keeps a stamp only on `Taken` and
  `release` drops it *(unit H)*.
* The read-to-lock window in step 5 still refuses.

---

## §3 (ii) — `40001` at SERIALIZABLE when the lost key had been read by an earlier statement (built)

Today a unique conflict at SERIALIZABLE is always renamed `23505` at `COMMIT`. PostgreSQL answers
`40001` when the transaction had read the key and `23505` when it had not (ADR 0114 cases 06 and 09
against 07). The answer (ii) needs one bit per unique key, *"was this read before it was written"*,
and the one moment that bit can be taken. *(unit H)* PostgreSQL gives `ON CONFLICT` the same `40001` at
REPEATABLE READ too, for a reason of its own; step 5 covers it, and that goes beyond the question as it
was put to the user.

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
  `in_a_transaction` calls with `isolation() == Isolation::Serializable` (`exec/mod.rs` lines 1041 and
  1081).
* A savepoint copies the read set with `Txn::read_set` / `Txn::restore_read_set` (lines 371 and 379;
  `pub struct ReadSet { keys, ranges }`, line 441). `savepoint::Recording` forwards all three
  (`crates/esker-sql/src/exec/savepoint.rs` lines 337, 341, 374).
* **The `Txn` trait has four implementors**: `StoreTxn` (`store.rs` line 271), `MemoryTxn` (`mod.rs`
  line 848), `Recording` (`savepoint.rs` line 268), and a test's `GatedTxn` (`tests/redrive.rs`
  line 354) — the trait's own doc says four, and unit I found the one this line had missed.

**Where the checks go out:**

* `StoreTxn::commit` (`store.rs` line 542) hands `read_keys` and `read_ranges` to
  `esker_client::Transaction::checking` (line 559).
* `checking` (`crates/esker-client/src/txn.rs` line 711) **drops a key already in the write buffer** —
  *"its write lock covers the same interval"*. So a key that was read and then inserted is validated by
  its own `Put`'s prewrite, and a lost race comes back as `TxnConflict` naming that key.

**Where it becomes `23505`:**

* `Executor::explain_conflict` (`exec/mod.rs` line 2432) renames a `SerializationFailure` whose key is
  one of `Written::unique_keys` (`Written` line 4310; `Unique { key, constraint, detail }` line 4335).
* **The block's `commit` (line 4885) runs `end_of_block` (line 1807) first**, and that resets
  `transaction_isolation` to the session default (lines 1820–1824). By the time `explain_conflict`
  runs, the level can no longer be asked.

**The probes that fill `unique_keys`** — `exec::dml::write_row` (line 1035):

* the primary key: `if txn.get(&key)?.is_some()` (line 1059), push at line 1078;
* each by-value unique entry: `if txn.get(&entry.key)?.is_some()` (line 1154), push at line 1160, after
  ADR 0114 §1's lock;
* `ON CONFLICT`'s `conflicting_row` (line 1612) reads the arbiter's entry — its `txn.get`s at lines 1637
  and 1660 — before `write_row` does.

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

1. **A required `Txn` method** — no default, implemented by all four:
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
5. **The arbiter rule** *(unit H)*. PostgreSQL's case 13 is not a read-set answer. Its `40001` comes from
   `ExecCheckTupleVisible` — the arbiter found a row this transaction's snapshot cannot see — and it is
   the same at REPEATABLE READ and SERIALIZABLE, with or without an earlier read (cases 13–16 below).
   Step 3 reaches it at SERIALIZABLE only because `conflicting_row`'s `get` records the key before
   `write_row` asks `has_read`; REPEATABLE READ keeps no read set, so it would stay `23505`. So:
   * `conflicting_row` (`exec/dml.rs` line 1612) takes `probed: &mut Vec<Vec<u8>>` and pushes each key it
     reads: the primary key's (line 1637) and each by-value arbiter entry's (line 1660);
   * the `INSERT` loop, per row, notes `written.unique_keys.len()` before `write_row` (line 742) and, at
     REPEATABLE READ and SERIALIZABLE (`!executor.isolation().waits()`), sets `read_first` on each
     `Unique` that call pushed whose `key` is in `probed`.

   `write_row`'s signature does not change, nor do its six other call sites. READ COMMITTED is left out:
   there `write_row` waits for the holder (ADR 0114 §1), and a lock taken after a wait restarts the
   statement (`wait_for_the_lock`, `exec/mod.rs` lines 438–441). Case 11 was not re-measured in unit H.

### What it answers, against ADR 0114's capture

| case | PostgreSQL 19 | today | after (ii) |
|---|---|---|---|
| 09 — SERIALIZABLE, read first, holder committed before the `INSERT` | `40001` at the `INSERT` | `23505` at `COMMIT` | `40001` at `COMMIT` |
| 06 — SERIALIZABLE, read first, holder live | waits, `40001` at the `INSERT` | `23505` at `COMMIT` | `40001` at `COMMIT` |
| 07 — SERIALIZABLE, never read | waits, `23505` at the `INSERT` | `23505` at `COMMIT` | `23505` at `COMMIT` |
| 13 — SERIALIZABLE `ON CONFLICT DO NOTHING`, read first | waits, `40001` at the `INSERT` | `INSERT 0 1` at once, `23505` at `COMMIT` | `40001` at `COMMIT` (step 3: `conflicting_row` read the arbiter) |
| 14 — the same, never read | waits, `40001` at the `INSERT` | `INSERT 0 1` at once, `23505` at `COMMIT` | `40001` at `COMMIT` (steps 3 and 5) |
| 15 — REPEATABLE READ `ON CONFLICT DO NOTHING`, read first | waits, `40001` at the `INSERT` | `INSERT 0 1` at once, `23505` at `COMMIT` | `40001` at `COMMIT` by step 5; `23505` without it |
| 16 — the same, never read | waits, `40001` at the `INSERT` | `INSERT 0 1` at once, `23505` at `COMMIT` | `40001` at `COMMIT` by step 5; `23505` without it |
| 04, 10 — REPEATABLE READ, plain `INSERT` | `23505` | `23505` at `COMMIT` | unchanged — `read_first` is false |

With step 5 the code moves to PostgreSQL's in every row above; without it, rows 15 and 16 keep `23505`.
The statement does not move: refusing at the `INSERT` would need a predicate lock in the store, and
PostgreSQL's wait before it would need REPEATABLE READ and SERIALIZABLE to wait (*What neither half does*).

**Rows 13–16, measured** *(unit H)*. PostgreSQL 19beta1, `esker-coord/s1-oracle-2026-09-13/h/` (13 is
`d/`'s case again, as the control): in all four, B's `INSERT … ON CONFLICT (nick) DO NOTHING` waited about
1.3 s, until A's `COMMIT`, and was refused `40001 could not serialize access due to concurrent update`
from `ExecCheckTupleVisible`; one `bob` remained. This node at `aad1dca3`, by one probe run once and never
committed (`h/node-probe.rs`, output `h/node-probe.out`): the same sequence on real stores (`tests/cluster`,
three stores) and on `MemoryBackend`, with B's first read as `count(*)`, as `find_by`, or none. In every
cell and on both, B's `INSERT` answered `INSERT 0 1` at once, A's `COMMIT` succeeded, and B's `COMMIT` was
`23505 duplicate key value violates unique constraint "index_subscribers_on_nick"`.

### Tests

* Real stores, `crates/esker-sql/tests/concurrent_unique_insert.rs`:
  * remove `#[ignore]` from `serializable_refuses_a_unique_key_committed_after_it_was_read_with_40001` (case 09);
  * add case 07 → `23505`;
  * add case 13 → `40001`;
  * add cases 15 and 16, REPEATABLE READ `ON CONFLICT DO NOTHING` → `40001` *(unit H)*.
* In process (`MemoryBackend`): the same shapes, the SERIALIZABLE ones in
  `crates/esker-sql/tests/serializable.rs`. The probe found `MemoryBackend` answering rows 13–16 exactly as
  real stores do today, so each is red before the change.
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

For step 5 *(unit H)*: skip the marking in the `INSERT` loop. Cases 15 and 16 answer `23505` again, while
13 and 14 stay `40001` because step 3 still reaches them — the control that shows the arbiter set, and
not the level, decides at REPEATABLE READ.

### Files

`esker-sql` only:

* `src/backend/mod.rs` — the trait method, and `MemoryTxn`'s implementation;
* `src/backend/store.rs` — `StoreTxn`'s;
* `src/exec/savepoint.rs` — `Recording`'s forward;
* `src/exec/mod.rs` — `Unique::read_first` and `explain_conflict`;
* `src/exec/dml.rs` — `write_row`'s two probes, and `conflicting_row` and the `INSERT` loop (step 5);
* the tests above.

No format, no wire.

### Risks

* A statement that reads a unique key and inserts it in the same statement — `INSERT … SELECT … WHERE
  nick = …` over the same table — counts as "read first". PostgreSQL's predicate lock would count it too.
* Nothing new to keep in memory: the read set already exists.
* Not captured *(unit H)*: `ON CONFLICT DO UPDATE`, and a lost key on a unique index that is not the
  arbiter. Step 5 marks only what `conflicting_row` read, so the second stays `23505`.

---

## What neither half does

* Build anything before the rulings.
* Make REPEATABLE READ or SERIALIZABLE **wait** for a live holder of a unique value, as PostgreSQL
  does (ADR 0114, "What stays declared").
* Move the `40001` from `COMMIT` to the `INSERT`, at SERIALIZABLE or at REPEATABLE READ.
* Close §2's read-to-lock window (step 5).
