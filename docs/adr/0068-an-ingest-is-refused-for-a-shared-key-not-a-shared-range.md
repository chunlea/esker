# 0068 — An ingest is refused for a shared key, not a shared range

* Status: accepted
* Date: 2026-09-04
* Supersedes nothing. Records a rule that was implemented in `616954e8` and never written down.

## Context

`Db::ingest` adopts an SST that was built elsewhere: phase 4 receives a region as a checkpoint's
files, phase 6 bulk-loads a table (`docs/DESIGN.md` §4.1). The file is linked into place and named
in a manifest edit; its bytes are never read or rewritten.

That is what makes the refusal rule delicate. **A file built elsewhere carries the sequence numbers
of the database that built it**, and this one's numbering is unrelated. For two entries under the
*same* user key there is therefore no answer to "which is newer" that is not a guess. For entries
under *different* user keys the question is never asked: a read resolves one user key at a time,
finds one version of it, and the sequence number decides nothing.

The engine originally refused an ingest whose **key range** overlapped anything — another file of
the same ingest, the memtable, or any level of the current version. Range disjointness is a cheap
sufficient proxy for key disjointness and it is the shape `RocksDB`-trained intuition reaches for.

It is a bad proxy **for this system in particular**. `esker-txn` encodes the MVCC version into the
key — `'x' ++ enc(user_key) ++ !ts` (`docs/DESIGN.md` §3) — so two versions of one row are two
distinct engine keys. Two files can interleave completely across a range while sharing not one key.
A bulk load of a time range for rows that already exist is exactly that shape: it overlaps
everything and collides with nothing. Refusing it was refusing arithmetic on the endpoints rather
than a real ambiguity, and it is the common case for a table that is being backfilled.

`debts-v1.md` #3 carried this as open at v1 with the note "c6 verified this as the one item of eight
that HEAD still owes". It was not open: `616954e8` landed the widening nine hours before that
register was written, and the site the register cites — `DbInner::place` — is placement rather than
permission and returns a level. What was genuinely missing is this file.

## Options

1. **Range disjointness.** Cheap, one comparison per level, and wrong for the MVCC key layout above:
   it refuses the bulk load the feature exists for.
2. **Key disjointness** — refuse exactly when the file holds a user key the column family already
   has an entry for. Costs a merged walk of the candidate's keys against the read path's own
   sources, which is one seek per distinct user key and, for a disjoint file, one seek in total.
3. **Rewrite the file's sequence numbers on the way in**, as `RocksDB` does with a per-file global
   sequence number in the footer. This would allow overlapping *keys* as well, because the ingested
   entries would then be comparable with local ones. It is a format change with a golden test.

## Decision

**Option 2.** An ingest is refused exactly when the candidate file holds a user key for which the
column family already has an entry — a value, a point tombstone, or a key covered by a range
tombstone — or which another file of the same ingest holds.

Three consequences of stating it as a rule about keys rather than about ranges:

* **A point tombstone is an entry, so deleting a key does not free it.** The sequence number would
  still have to decide between the delete and the ingested value, and it cannot. This is the part
  that looks like over-refusal and is not: it is the same ambiguity, under a key whose current
  value happens to be "absent".
* **A range tombstone counts even though no cursor would show it.** A range delete hides keys the
  merged run has never seen (ADR 0017), so the check consults the tombstone set as well as the
  cursors — `DbInner::merge_sources`, the read path's own list, from one pinned version. An ingest
  that consulted a different set than a read would refuse safe ingests or, far worse, admit one
  whose keys a reader can already see.
* **Range overlap is a placement question, not a permission one.** Levels 1 and below are sorted
  runs whose files must be range-disjoint or a seek cannot binary search the level; L0's files
  overlap by construction. So a file whose keys are free but whose range is not goes to L0 and a
  later compaction sorts it downward. The deepest level with room is still preferred, so a
  genuinely disjoint bulk load lands deep and does not immediately compact itself back up.

Option 3 stays a v2 feature. It is the only thing that would allow a *key* overlap, and it changes
the SST footer, which has a golden test.

## Consequences

* The bulk load this feature exists for — new versions of rows that already exist — is accepted
  rather than refused, and lands at L0 when its range overlaps.
* Refusal is still total and still before any file is linked: either every file of an ingest is
  adopted or none is, so a refusal leaves the database exactly as it was.
* The check is `O(distinct user keys in the file)` seeks rather than `O(levels)` comparisons.
  `40a42384` made the disjoint case one seek, so the common case does not pay for the rule.
* `tests/ingest_overlap.rs` property-tests both directions against a rule **computed from the
  inputs** rather than read off the code under test — values, point tombstones and an optional
  range delete — plus a case asserting that fully interleaved ranges sharing no key are all
  accepted.
* What is *not* changed by this ADR, and is a separate debt: an ingested entry keeps a sequence
  number from a numbering this database never issued, so a snapshot taken *before* an ingest can
  see the ingested keys. That is about when an ingest becomes visible, not about which of two
  versions wins, and closing it needs option 3's footer field.
