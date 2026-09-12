# 0111 — A deleted key's versions are dropped as one segment

Status: **Proposed**, 2026-09-11. Opened by the coordinator after #70's (d) landed and the climb it
was meant to flatten did not reach zero. **No code is written against this**; what follows is the
material for a ruling.

## Context: what #70 left behind, measured

[ADR 0110](0110-who-publishes-the-garbage-collection-safepoint.md) publishes a safepoint, #62 made a
compaction drop what is below it, and #70's `collect::Sweeper` made a rising safepoint actually go
and collect. Twelve identical rounds of DDL at the default write buffer, in process, three stores:

| | read cost per round | SSTs standing |
|---|---|---|
| collecting | **+288.9** (95% interval ±1.1) | +1.04 / round |
| control (same build, collection off) | **+4198.2** (±127.5) | +0.00 (nothing ever flushed) |

A fourteenth of the climb, and the memtable stopped growing. What is left is **+96 entries and
exactly one SST per round**, with every file in L6 and L0–L5 empty — `levels {6: 1}`, `{6: 2}`, …
`{6: 12}` over the twelve rounds.

**Two symptoms, one cause.**

`MvccCollector::keep_as_newest` (`crates/esker-store/src/gc.rs`) keeps the newest version at or
below the safepoint, because that is what a read at the safepoint would return. It does not look at
`record.kind`, so a `Kind::Delete` is kept exactly as a `Put` would be. Twelve dropped tables leave
ninety-six such records a round, each saying "this key is gone" and each immortal.

The file count is the same fact seen from the other side. `CREATE TABLE` takes a **fresh table id**
every time, so each round's survivors sit at a strictly higher, disjoint key range. The sweep's
output overlaps nothing already in L6, lands as a new file, and `Picker::worst_level`
(`crates/esker-engine/src/compaction/picker.rs`) excludes the last level — it has nowhere to compact
it into — so the bottom level never merges with itself. A read consults all twelve. This is the
direct source of run 127h's `corr(seconds, SSTs standing) = +0.58`.

## Why the current contract cannot express the fix

`CompactionFilter::filter(&self, level, user_key, value) -> FilterDecision` decides **one entry at a
time**, and `FilterDecision` is `Keep` or `Remove`. A filter that wanted to say "this key is gone,
take all of it" has no way to say so, and no way to know it is safe.

The engine's own three drop rules (`compaction/job.rs`) never help here, and the reason is worth
stating precisely: for MVCC, **every version is a different engine user key.** `esker_txn::key::write`
builds `'x' ++ enc(user_key) ++ enc_ts(commit_ts)`, so `K@100` and `K@200` are two unrelated keys as
far as the engine is concerned. Rule 1 (`previous_seqno <= floor`, "a newer version is visible to
everyone") compares sequence numbers *within one engine user key* and so never fires across MVCC
versions. Rule 2 drops engine tombstones, and an MVCC delete is not one — it is an engine `Put`
whose *value* is a `WriteRecord` with `kind = Delete`. Rule 3 is the filter, one entry at a time.

## Why a point query is not enough

The obvious move is to let the collector drop the delete when `Picker::is_bottom_level_for_key` says
nothing below holds the key. **That is unsound, and the encoding is why.**

The timestamp suffix is **complemented** — `esker_keys::codec::enc_ts` is `(!ts).to_be_bytes()`, and
`crates/esker-txn/src/key.rs`'s module doc says why — so newer versions sort *first*. Take key `K`
with a delete at `ts = 200` and a put at `ts = 100`:

```
'x' ++ enc(K) ++ !200      <- the delete, sorts first
'x' ++ enc(K) ++ !100      <- the older put, sorts AFTER it
```

`is_bottom_level_for_key(version, level, k)` asks whether any file below overlaps `[k, k]`. A file at
a lower level holding only `'x' ++ enc(K) ++ !100` has a smallest key **greater** than the delete's
key, so it does not overlap `[k, k]` — and the predicate answers **true** while an older version is
sitting right underneath. Drop the delete on that answer and a read at `ts = 250` finds the put at
`100` and returns a value for a key that was deleted. **A point query is the wrong shape**: the
question is about the key's whole version span, and the span extends in the direction the point
query does not look.

The span itself is already expressible: `esker_txn::key::version_range(user_key)` returns
`(versioned(user_key, u64::MAX), successor(prefix(user_key)))` — every version of one key and
nothing else. It exists and is tested today.

## Why a segment is decidable in the input stream

A compaction reads its inputs in internal-key order (`CompactionOutput::add`'s contract: "keys
arrive in internal-key order"). Every version of one logical key shares the prefix
`'x' ++ enc(user_key)` and differs only in the eight-byte suffix, so **all of them are contiguous**,
and because the suffix is complemented they arrive **newest first**. A run of one logical key is
therefore a run the reader can recognise as it goes: it begins when the prefix changes and ends when
it changes again.

`MvccCollector` already relies on exactly this, which is the best evidence it holds:
`keep_as_newest`'s comment says "entries arrive newest first within a key", and the whole of its
`seen` state is one `(key, newest_ts)` pair carried across calls.

The job also already tracks a run — `current_user` / `new_key` in `CompactionJob::run` — but of the
**engine** user key, which for MVCC is one version. The grouping this needs is coarser by exactly
one suffix, and the engine may not compute it itself: **invariant 7** says key semantics live in
`esker-keys` and above, and the engine is byte-opaque. Whatever the shape of the decision, the
grouping has to be told to the engine, not inferred by it.

## Decision to be made

**The contract changes from "judge each record independently" to "judge a user key's contiguous
segment."** Drop **all** versions of a key, the delete included, if and only if:

1. **The newest record in the segment is a `Delete`.** Anything else means the key exists at the
   safepoint, and its newest version is what a read returns.
2. **`is_bottom_level_for_key` holds over the segment's whole range** — no level below the output
   holds *any* version of this key. Asked with `key::version_range(user_key)`, not with one key.
3. **No live reader can still see any version in the segment** — no engine snapshot's `seqno` and no
   active MVCC read's `ts` sits above any of them.

Otherwise the segment is kept as it is today.

### "Newest" means newest **version**, and this is not a detail (#78)

Added 2026-09-11, after the same word cost five catalog table records in run 127 attempt 4.

`Kind::Lock` is a record and not a version. It is what a committed `Op::Check` leaves — a
SERIALIZABLE transaction's validated read set (ADR 0062, ADR 0067) and the row lock a
`SELECT … FOR UPDATE` takes (ADR 0088) — and the read side steps past it looking for a version
(`esker_txn::percolator::newest_version_at`). `Kind::Rollback` is the same. So the newest *record*
of a segment and its newest *version* are different things, and **condition (1) is about the
version**: `Kind::is_a_version` is the predicate, exactly as `MvccCollector::filter` now uses it.

Reading it the other way is what #78 was. `filter` asked "is this the newest record at or below the
safepoint" and a `Lock` answered yes, which kept the lock and dropped the `Put` under it — a key
that a transaction had only *read* came back absent, and a catalog name record was left pointing at
a table record that was no longer there.

For this ADR the same confusion costs the other direction, which is merely a missed collection: a
segment `Lock@30 · Delete@20 · Put@10` has a newest record that is not a `Delete`, so condition (1)
is false and the segment is kept although every version in it could go.

**It also costs the streaming shortcut below.** "The decision is available at the segment's first
entry" holds for the newest *record*; the newest *version* may be one or more records further in. A
design that decides at the first entry must step past the non-versions at the head of the segment
first — which is bounded (a key collects at most one lock record per validating transaction) but is
not zero, and is the thing to get right when this is implemented.

### What each condition refuses, stated as a counterfactual

- **Without (1):** a key whose newest version is a `Put` loses its current value. This is the
  difference between collecting and deleting.
- **Without (2):** the resurrection above — a lower level still holds `K@100`, the delete at `K@200`
  is dropped, and a read at `250` answers with a value for a deleted key. An acknowledged delete is
  lost, which is invariant 1's territory from the read side.
- **Without (3):** a reader holding a snapshot below the delete is entitled to see what was there.
  Dropping the segment answers it with nothing where it should see the old value.

### (3) is mostly already true, in two places

This is worth saying because it means condition (3) is a *check*, not new machinery.

- The **engine** half is `CompactionJob`'s `floor` — "the oldest live snapshot, or the newest
  visible sequence number when there are none" — and the existing rules already gate on
  `seqno <= floor`. A segment decision needs the same test over every entry in the segment, which is
  the maximum of their sequence numbers against the same floor.
- The **MVCC** half is [ADR 0110](0110-who-publishes-the-garbage-collection-safepoint.md) decisions
  1 and 5: the safepoint is at or below the oldest active read, and a read below the safepoint is
  **refused** rather than answered. So no live MVCC reader is entitled to a version below the
  safepoint, and `MvccCollector` already tests `commit_ts > safepoint → Keep` per record.

The new work in (3) is to apply both over a segment instead of an entry.

### How it composes with `MvccCollector`

The segment rule **narrows**; it never overrides a `Keep`. The order is:

1. Per record, as today: outside the safepoint, or a `Rollback` whose `start_ts` is above the
   safepoint, or an undecodable record → `Keep`, and the segment is kept whole. A filter that cannot
   read a record must not decide that a *key* is collectable.
2. `keep_as_newest` still decides which single record survives when the segment is kept.
3. Only where every record in the segment would be dropped or kept-as-newest, and the three
   conditions hold, does the segment go entirely.

So the fail-closed property r1 named — `effective_safepoint` returning `None` means keep — is
preserved: a table with `retention forever` keeps its segment, delete and all.

### What does not change

- **Invariant 1** (log before state, fsync before ack) is untouched: this is a compaction-time
  decision about what to write out, and the delete it forgets was acknowledged and is being honoured
  by having *nothing* left to read, not by a record.
- **Invariant 2** (checksums) and **invariant 3** (immutable files, atomic pointers) are untouched.
- **No on-disk or wire format changes.** The `WriteRecord` encoding, the key layout, the SST format
  and `crates/esker-proto/tests/golden/messages.hex` are all unaffected: this changes which entries a
  compaction writes, which is what a compaction is for.
- **Invariant 7** (the engine is byte-opaque) is the constraint the design must respect, not
  something it changes: the engine must be *told* the grouping.

### The decision is available at the segment's *first* entry, which is why it need not buffer

The complemented suffix earns its keep twice. It is what makes a point query unsound, above — and it
is also what makes the segment decidable **without holding the segment in memory**: the newest
version sorts first, so condition (1) ("the newest record is a `Delete`") is answered by the entry
the job is already looking at when the segment opens. Conditions (2) and (3) are about the key, not
about a particular version. So a streaming loop can decide at the segment's first entry and then
drop the rest of the run as it meets them, with one flag carried across iterations — the same shape
`MvccCollector::seen` already has.

**One claim in that needs checking before it is relied on.** Condition (3)'s engine half wants
`max(seqno) <= floor` over the segment, and deciding up front means testing only the first entry's.
That is sound **if** the version with the largest `commit_ts` also has the largest sequence number,
which should hold for the `write` column family — Percolator cannot commit a key at `ts = 200` before
it has committed it at `ts = 100`, because the earlier transaction's lock is in the way — so commit
order is write order and the two agree. It is stated here rather than assumed because it is a claim
about `esker-txn`'s ordering being visible in `esker-engine`'s sequence numbers, and if it is ever
false the decision has to move to the end of the segment and buffer after all.

### The shape of the change, not yet chosen

Three ways to tell the engine, recorded so the ruling can pick one:

- **(a) The filter reports the grouping.** A method like `fn group<'k>(&self, user_key: &'k [u8]) ->
  Option<&'k [u8]>` returning the part of the engine key that identifies the logical key. The job
  compares those bytes to find the segment and asks `is_bottom` over the segment's range. The engine
  compares bytes it was handed and learns nothing about what they mean.
- **(b) A deferring decision.** A third `FilterDecision` that says "hold this segment", with the job
  buffering until the segment ends. It needs no claim about sequence numbers agreeing with commit
  timestamps, because it sees the whole segment before deciding — but it makes a streaming loop hold
  a run in memory, and the number of versions of one key is small in practice and not bounded in
  principle.
- **(c) The collector does it above the engine.** A separate sweep that reads and rewrites, outside
  the compaction path. Costs a second pass over the data and a second set of correctness rules.

(a) is the smallest and keeps one set of rules; it is written first for that reason and not as a
recommendation the ruling is bound by.

## Cost

The decision adds, per segment, one range `overlapping` query per level below the output — the same
query `is_bottom_level_for_key` already makes per entry, over a range instead of a point, and
**fewer times**: once per logical key rather than once per version.

What it buys, on #70's twelve-round measurement: the ninety-six immortal records a round go, and
with them the file a round — the sweep's output for a round of dropped tables becomes empty rather
than a new L6 file.

## Acceptance

The workload exists: `crates/esker-sql/tests/collection_keeps_up.rs`. It is **green**, because #70
had to land against something it could hold — it asserts the two-arm ratio, and its doc names this
residue as the reason the flat-slope form is not the assertion. Landing this ADR is what lets that
test go back to the shape #70 was specified in.

1. `levels` stops gaining a file a round — today `{6: 1}` … `{6: 12}` over twelve rounds.
2. The read-cost slope's 95% interval contains zero, which is #70's acceptance in the shape it was
   originally specified in, and which the two-arm ratio currently stands in for.
3. **A resurrection test.** A key deleted above the safepoint with an older version in a lower level
   that this compaction does not touch: the delete must survive, and a read after the compaction must
   still say the key is gone. This is the one that fails if condition (2) is ever weakened, and it has
   to be written as a *store*-level test with a real multi-level database, because it is about what a
   partial compaction can see.

## Recorded, not decided

**The bottom level never merges with itself.** `Picker::worst_level` excludes the last level because
there is nowhere to compact it into, which is correct as far as it goes, and it means L6 accumulates
one file per sweep whenever a sweep's output does not overlap what is already there. This ADR
removes the *supply* of such files for the deleted-key case, and it does not answer the general
question: **should the bottom level have a self-merge trigger, on its own file count?**

Arguments both ways, so that whoever picks this up starts with them:

- **For:** the file count is what a read pays, and run 127h measured it directly —
  `corr(seconds, SSTs standing) = +0.58` against `+0.65` for the entries inside them. L0 is already
  scored by file count for exactly this reason (`Picker::score`); the bottom level is the one place
  where that reasoning is dropped. Any workload whose keys advance monotonically — an append-only
  table, a sequence-keyed index, an id allocated per object — produces non-overlapping bottom-level
  files for ever, with or without this ADR.
- **Against:** merging bottom-level files that do not overlap buys nothing but a lower file count;
  the data is already as merged as it can be, and the rewrite is pure write amplification. The right
  trigger may be "too many files in a key range" rather than "too many files", and choosing it needs
  a measurement nobody has taken.

This is a separate decision with a separate acceptance and is deliberately not made here.
