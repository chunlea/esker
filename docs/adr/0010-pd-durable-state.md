# ADR 0010 — the placement driver's durable state, and the two writes that are ordered against an answer

Status: accepted (phase 4a)
Date: 2026-08-30
Context: `docs/DESIGN.md` §3, §7; `prompts/04-multiraft-pd.md`; `docs/plans/phase-4-pd.md`;
`CLAUDE.md` invariants 1, 2, 6 and 9

## Context

PD in 4a is one process holding four kinds of state: the cluster's identity, the routing table, an
id allocator and the timestamp oracle. `docs/DESIGN.md` §7 says the state lives in its own
`esker-engine` instance and nothing more, which leaves three decisions that a future reader could
reasonably reverse:

1. **What the key space looks like**, and in particular how `GetRegion(key)` — "which region's range
   contains this key" — becomes a lookup rather than a scan.
2. **What a record's bytes are**, given that the engine already checksums everything it stores.
3. **Where the durability boundary sits** for the allocator and the oracle. Both hand out values
   that must never repeat, and both amortise their persistence over a batch, so "persist" and "hand
   out" are two events whose order is the entire correctness argument.

The third is the one that matters. A repeated region id makes two regions indistinguishable; a
repeated timestamp breaks Percolator's snapshot isolation in phase 5 (`docs/DESIGN.md` §8) in a way
that appears once, under load, as a lost update — and no local test in phase 5 will reproduce it.

## Decision

**PD keeps its state under the `'m'` prefix of §3 in its own database, with a region indexed twice;
every record is `version:u8 ++ fields` with no checksum of its own; and both the allocator's batch
end and the oracle's high-water mark are fsynced *before* the value they cover leaves the process.**

```text
'm' 'c'                        cluster record
'm' 'a'                        allocator: the end of the reserved id batch
'm' 'k' ++ tag:u8 ++ end_key   range index: which region ends here (tag 1 bounded, 2 = +∞)
'm' 'r' ++ region_id:u64 BE    region record
'm' 's' ++ store_id:u64 BE     store record
'm' 't'                        the oracle's high-water mark, in physical milliseconds
```

- A region is written under **both** `'m' 'r'` and `'m' 'k'` in one `WriteBatch`.
- `AllocId` persists `allocated_end` and only then returns ids at or below it; a restart resumes at
  `allocated_end + 1`.
- `Tso` persists `mark = physical + 3 s` whenever `physical >= mark`, and only then composes a
  timestamp; every timestamp handed out therefore has `physical < mark`, and a restart resumes at
  `max(clock, mark)`.

## Rationale

**Two keys per region, not one.** A heartbeat arrives naming a region id and must find its record in
one seek, so the primary key is the id. A lookup arrives with a *key* and needs the first region
whose end is past it, which is a different order entirely — so the index is keyed by end key. Keying
regions only by end key would make every heartbeat a scan; keying them only by id would make every
lookup one. Both keys in one batch is what keeps them from disagreeing: an index entry naming a
region that is not there would route a client into a hole.

**The tag byte, because `b""` means two different things.** An empty `end_key` means +∞ in region
metadata (`docs/plans/phase-4.md` §5) and `b""` sorts *below* every byte string. An index keyed by
the raw end key would therefore sort the last region — the one covering everything above its start —
first, and every lookup past the second-to-last region would fall off the end of the index and find
nothing. Tagging bounded ends `1` and the unbounded end `2` puts +∞ where it belongs. The client's
region cache hit the same wall from the other side and solved it by keying on `start_key` and
walking backwards (phase-4 plan §10.2); either fix is fine, and *no* fix is a routing table that is
wrong for ever at exactly one key range.

**Seek to `key ++ 0x00`, because an end key is exclusive.** A region ending exactly at the key does
not contain it. `key ++ 0x00` is the immediate successor of a byte string, so seeking there skips
that region and lands on the first one that might contain the key. Seeking to `key` itself is the
off-by-one this project would find in production rather than in a test.

**No per-record CRC.** `CLAUDE.md` invariant 2 says every on-disk byte is checksummed, and here it
already is: these bytes are values in an engine whose WAL records and SST blocks both carry CRC32C.
A second checksum would cover the same bytes twice and would have to be maintained. What the records
*do* carry is a format version byte and a strict decoder — an unknown version, a short field or a
trailing byte is an error value, never a guess — which is the part the engine cannot do for them.
This follows `esker_store::raft_log`, which made the same call for the same reason.

**The durability boundary is before the answer, not after.** Both allocators are monotone counters
that reserve ahead: reserving is what costs an fsync, and handing out is free until the reservation
runs out. Persisting *after* handing out would make the fsync cheap and the guarantee empty — a
crash in the gap resumes below values that are already in use. Persisting *before* costs a crash
some skipped ids and up to 3 s of skipped timestamps, which cost nothing at all: ids are 64 bits,
and a timestamp only has to be ordered, never dense.

**`max(clock, mark)` on restart, not one or the other.** The clock alone repeats timestamps whenever
it jumps backwards, which happens on ordinary machines after an NTP correction. The mark alone
freezes time on a PD that was down for a week — every timestamp for the next week would come from
the logical bits of one millisecond, and the batch that overflows them starts borrowing the future
one millisecond at a time. The maximum takes whichever is ahead, and is the only rule that survives
both.

## Consequences

- **A format change to any record needs a version bump, an ADR and a migration.** The bytes and the
  keys are pinned by `crates/esker-pd/tests/records.rs` against a golden file written by an
  independent encoder; the keys are pinned too, because a key layout that drifts does not fail to
  decode, it silently stops finding what is already there.
- **A crash wastes ids and milliseconds, and that is the intended trade.** With the default batch of
  1,000, a crash skips up to 1,000 ids; with the 3 s mark, up to 3 s of timestamps.
- **PD's on-disk format is not the wire format.** The two agree today and are encoded separately, so
  that a wire-compatibility decision cannot silently become an on-disk one. The cost is one
  hand-written encoder for `Region` inside `esker-pd`, which the goldens cover.
- **The ordering is checked by breaking it.** Resuming at the clock instead of `max(clock, mark)`
  turns three unit tests and the kill loop red; dropping either persist turns the "a failed persist
  hands out nothing" test red; removing the id reservation's write turns the kill loop red with "id
  1 did not advance on 34". A `SIGKILL` cannot distinguish `sync = true` from `sync = false` —
  bytes already in the kernel still land — so the fsync itself rests on the same argument as the
  engine's invariant-1 tests, and that limit is written down beside the test.
- **4e replaces the boundary, not the rule.** When PD becomes three nodes, the mark goes through
  Raft instead of through one engine; "persisted before handed out" is exactly what has to remain
  true, and the `Oracle`/`Allocator` state machines take a callback precisely so that the *what*
  can change without the *when* moving.
