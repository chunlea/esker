# A SQL table that occupies more than one region

Lane `split`, branch `split-fix`, cut from `main` at `07e2f128`. Closes the first item of
`docs/plans/phase-16-mpp.md` §10 "What would change this verdict": *a SQL table that splits*.

## 1. The finding

A SQL table always occupies exactly **one** region, whatever its size. Range sharding — the
system's whole scale-out story (`CLAUDE.md`: "Linear scale-out by splitting key ranges into
regions") — does not trigger for SQL data at any threshold.

The measurement is in `docs/plans/phase-16-mpp.md` §10: a region holding 200,000 rows reports
`~0 bytes` to PD; a 4 MiB threshold over ~20 MB of table produced one region, identical to the
run that asked for one with a 512 MiB threshold. **The threshold is not the variable — the
measured keyspace is.**

Two functions in `crates/esker-store/src/split.rs` look at one namespace in one column family:

* `approximate_size` (:174) calls `db.approximate_size(cf::DEFAULT, ['r'++start, 's'))`;
* `choose_split_key` (:115) scans the same range and `strip_prefix(&[prefix::RAW])`s every key.

A SQL row is the *user key* of a transaction, so it reaches the engine as
`'x' ++ enc('t' ++ tenant ++ table ++ 'r' ++ row_id) ++ !ts` — the `'x'` namespace, and the
Percolator families. Neither function can see a byte of it.

### The part that is easy to get wrong, and that decides the fix

Widening the range to `['r', 'y')` in `default` — the obvious "fix" — **still reports ~0 for a
SQL table**. `esker_txn::codec::SHORT_VALUE_MAX_LEN` is 255, and a value at or under it is
**inlined into the `write` record**; `default` gets no entry at all
(`crates/esker-txn/src/percolator.rs`, `Op::short_value`/`long_value`). Ordinary SQL rows are
short. So for the workload that produced the finding, essentially **every byte of the table is in
the `write` column family**, and any fix that does not read `write` leaves the finding in place
while looking like it closed it.

## 2. What `approximate_size` should measure

A region owns `[start_key, end_key)` of the **user** key space. `esker-store/src/snapshot.rs`
already knows how that maps down — `physical_ranges_of` (:454), the mapping
[ADR 0032](../adr/0032-a-snapshot-carries-every-column-family.md) established after format
version 1 shipped a snapshot that walked `default` under `'r'` alone and dropped every
transactional record a region held. **That is this bug, one layer over.** The same region shipped
its transactional records correctly and reported none of them.

So: one definition of the mapping, used by both. `physical_ranges`, `physical_ranges_of` and
`PHYSICAL_NAMESPACES` move out of `snapshot.rs` into a new `crates/esker-store/src/keyspace.rs`
and are called from `split.rs` as well. No behaviour change in `snapshot.rs`; it imports what it
used to define.

**Which column families count as a region's size:** `default` and `write`. Not `lock` — a lock is
one in-flight transaction's state, cleared at commit or rollback, so counting it makes a region's
size a function of concurrency and lets a burst of prewrites trigger a split of data that has not
been written. Not `raft` — that is this store's log, keyed by region id and not by user key, and
it is what `SNAPSHOT_CFS` deliberately excludes for the same reason.

**Both physical namespaces in both families**, rather than a table of which family may hold which
namespace. `write` holds only `'x'` today; a scan of the `'r'` range in it costs one seek that
fails its bound immediately, and it cannot go stale. `snapshot.rs:281` states this reasoning for
the snapshot stream and this follows it.

So the size is the sum of four `Db::approximate_size` calls: `{default, write} × {'r' range, 'x'
range}`. Each is the same hint it was, with the same three over-counting directions
`Db::approximate_size` names; all this changes is that the hint now covers the data.

## 3. The boundary-search fix

`choose_split_key` must sample **user keys**, because the split key is a user key: it is checked
by `is_legal_boundary` against `region.start_key`/`end_key`, it goes into the `Split` command, and
every peer applies it against its own copy of the range.

The scan reads **exactly the (family, range) pairs the size counts** — the same four. That is the
property worth stating, because the failure it rules out is the one a partial fix produces: a
region whose size says "split" and whose boundary search says "there is nothing here", which the
split checker records as `refused` and does not look at again until the region has grown by
another whole threshold (`server.rs:1445`).

Each source yields ascending user keys:

* `'r'` range: user key = the key with `'r'` stripped, as today.
* `'x'` range: user key = `esker_txn::key::split(key).0`. A user key has one entry per version, so
  a source **advances to the next distinct user key** — otherwise the sample is weighted by how
  often a row was updated rather than by how many rows there are.

The four sources are merged by taking the smallest current user key and advancing every source
that is on it, so a key held in more than one family or namespace is one key. The merged stream
then feeds the *existing* halving-stride sampler unchanged: [ADR 0012](../adr/0012-split-key-selection.md)'s
three rules (a key that exists, strictly inside `(start, end)`, never the first sample) are about
the sample, not about where it came from, and none of them changes.

Byte order is preserved by the merge: the group encoding is memcomparable, so within a source the
decoded user keys ascend, and across sources a bytewise comparison of decoded user keys is the
same order the region's bounds are in.

## 4. Invariants

* **1/3 (crash safety, immutable files).** Nothing here writes. Both functions are reads over a
  `DbIterator`, and the split path they feed is unchanged: the boundary still goes through the
  Raft log, still applies in the batch that carries `apply_index`.
* **2 (checksums).** No format touched. No new bytes on disk or on the wire.
* **5 (region epoch).** Untouched: the epoch bump is `apply.rs`'s, and both halves still bump.
* **7 (engine and Raft byte-opaque).** The measurement learns *which namespaces and families a
  region spans* and nothing else. The `'r'` mapping is `esker_keys::prefix::raw_key`, the `'x'`
  mapping is `esker_txn::key::prefix`/`split`. No key layout is parsed in `esker-store`, and
  **no new accessor in `esker-keys` is needed** — everything this fix needs already exists there
  and in `esker-txn`, which is what `snapshot.rs` proves by already doing it.
* **9 (no panic on disk data).** A key in the `'x'` range that does not decode is a
  `ProtoError::internal`, matching the existing refusal for a key outside `'r'`.

## 5. Tests

Red first, at the store layer. **Not through SQL** — the defect is in the store's measurement.

1. `split.rs` unit: **a region of committed transactional rows is not zero bytes.** Write ~1 MB of
   `write`-CF records (`esker_txn::key::write` + a `WriteRecord` with an inline short value — what
   a committed SQL row *is* on disk) and assert `approximate_size` reports within a factor of the
   bytes written. Red today: `~0`.
2. `split.rs` unit: **a boundary is found in transactional data.** `choose_split_key` over the same
   region returns a legal boundary that is one of the written user keys. Red today: `None`.
3. `split.rs` unit: **the versions of one key are one key.** A region of few keys and many versions
   each yields no more samples than it has keys — the boundary of 10 keys × 100 versions lands
   near the 5th key, not near the 500th version. (Guards the dedup; without it the sampler is
   weighted by update frequency.)
4. `split.rs` unit: **a long value counts too.** A value over `SHORT_VALUE_MAX_LEN` lands in
   `default` under `'x'`; the size sees it and the boundary search sees its key.
5. `split.rs` unit: **a lock is not size.** A prewritten, uncommitted key adds `lock` bytes and
   those do not move the size. (Pins decision 1 of the ADR, so reversing it is a test change.)
6. `split.rs` unit: **the raw namespace still behaves exactly as it did.** The existing five tests
   stay, unchanged, and are the control.
7. `tests/split.rs` integration: **a region of transactional rows actually splits.** Drive
   `Store::serve_txn` (prewrite + commit, as `tests/txnkv.rs` does) past `TINY_SPLIT_SIZE`, then
   assert region count > 1, the halves tile `["", "")` exactly, **each half is non-empty** by
   `snapshot::key_counts`, and every committed key is still readable through whichever half owns
   it. Red today: it stays at one region until the deadline.
8. `tests/split.rs` integration: the parallel of 7 written in the `'r'` namespace already exists
   (`a_region_that_grows_past_the_threshold_splits`) and is the control.

Trap being avoided (brief §4): the assertions are the split *happening* and the size *changing*,
never that a function returned `Ok`.

## 6. Risks

* **A region now splits where it never did.** That is the point, but it means clusters carrying
  transactional data will start splitting on restart with this build, at 96 MiB. The split path
  itself is phase-4 code with its own tests; what is new is that it now receives work. Test 7 is
  the evidence it survives it.
* **A larger scan.** Up to four range scans instead of one, on a blocking thread, once per split
  attempt. Three of them are an immediate bound failure for a region that holds only one
  namespace. ADR 0012 already prices a split at one full scan.
* **`write` bytes are versions, not rows.** A region's size includes superseded versions until GC
  drops them, so a heavily-updated table splits earlier than its live row count suggests. That is
  the same direction `Db::approximate_size` already over-counts in, and it is the safe one.
* **Moving code out of `snapshot.rs`** could conflict with another lane. `crates/esker-store/**`
  is this lane's, and the move is three items and their imports.

## 7. What this will NOT do

* **Not touch `esker-keys` or `esker-proto`.** Nothing is needed there; §4 says why.
* **Not change the split threshold, the split protocol, the `Split` command, or the epoch rules.**
* **Not add a byte-weighted boundary.** ADR 0012 chose key-count and its consequence (uneven
  halves by bytes, corrected by the next size check) is unchanged.
* **Not measure the `'t'` or `'m'` namespaces as physical ranges.** They are namespaces of the
  *user* key space and reach the engine inside `'x'`; `snapshot.rs`'s `PHYSICAL_NAMESPACES`
  comment already says so and its guard test enforces it.
* **Not touch `esker-sql`.** A SQL table splitting is the *consequence* to be measured later; the
  defect and its test are the store's.
* **Not build the parallel fragment dispatch** phase-16 §10 item 3 asks for. Different lane.

## 8. ADR

[ADR 0073](../adr/0073-a-regions-size-is-the-data-families-it-spans.md) — which column families
and which namespaces count toward a region's size, and why `lock` does not. Claimed at `main`
`07e2f128`, where the highest is 0072.

## 9. Units

| # | What | Commit |
|---|---|---|
| 0 | This plan | |
| 1 | Red: the size and the boundary of a transactional region | |
| 2 | Fix: `keyspace.rs`, and both functions read every family the region spans | |
| 3 | ADR 0073 + DESIGN §6 | |
| 4 | Red→green: a region of transactional rows actually splits | |
