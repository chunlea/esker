# 0032 — a snapshot carries every column family

Status: accepted. Phase 8, unit a6-1, from the blocker
`docs/plans/phase-8-learner.md` §store names as "a columnar learner PD places never receives the
region's existing data".

## Context

A Raft snapshot moves a region's *state* to a peer the log cannot catch up. Version 1 of the
stream (`crates/esker-store/src/snapshot.rs`) walked one column family, `default`, over one
namespace in it, `'r'`, and re-added that namespace byte on the receiving side.

A region's data is not in one column family. `docs/DESIGN.md` §8 puts Percolator's records in
three — the value in `default` under `'x' ++ enc(user_key) ++ !start_ts`, the commit record in
`write`, the lock in `lock` — and everything above RawKV is transactional: every SQL row, every
index entry, every catalog record. So version 1 shipped the RawKV pairs of a region and **silently
dropped everything a transaction had ever written to it**.

Nothing failed loudly, for a reason worth writing down. A row learner is normally caught up by the
log; when it is caught up by a snapshot instead, promotion waits on `matched`, which the transfer
moves whether or not the bytes were complete, and nothing reads its `write` records until it
leads. A **columnar learner** is never promoted and its whole job is to read those records, so it
is the first replica in this system for which an incomplete transfer is *visible* rather than
merely true. What it looked like on a four-store cluster: a placed learner at the leader's applied
index with two `write` records where the leader had ten — two, because two commits happened after
the transfer and came down the log.

The same one-family assumption was in the two functions that undo a transfer: `clear_range`, which
empties a range so a snapshot can refill it, and `discard_range`, which cleans up after a partial
receive. Both left the `lock` and `write` families and the `'x'` half of `default` in place, so a
replaced region would have served a mix of the old state and the new.

## Decision

**1. The stream carries `default`, `lock` and `write`, and each chunk names its family.** Format
version 2:

```text
header = 2:u8 ++ 1:u8(kind) ++ region ++ index ++ term ++ voters ++ learners
pairs  = 2:u8 ++ 2:u8(kind) ++ cf:u8 ++ crc32c:u32 ++ count ++ (key ++ value)*
```

A chunk naming a family this build does not ship is refused as corrupt rather than defaulted into
`default`; defaulting is how one family's bytes end up in another. `raft` is not shipped and will
not be: it holds this store's log, hard state and region records, which are one peer's facts about
a region rather than the region's contents, and the receiver builds its own from the header.

**2. The keys on the wire are engine keys**, namespace byte and timestamp suffix included, written
into the named family exactly as they arrive. Version 1 sent user keys because with `'r'` as the
only namespace the byte was derivable; with `'x'` beside it the receiver would have to guess which
to re-add, and a guess is what this whole change is about not making. `CLAUDE.md` invariant 7
holds: nothing here reads a key, it copies bytes between two column families of the same name.

**3. A region's user-key range maps to one engine range per physical namespace**, `'r' ++ key` and
`'x' ++ enc(key) ++ !ts` — the same mapping `txnkv::scan` gives a client's range, valid because
the memcomparable encoding is order-preserving and prefix-free. Both are walked in every shipped
family: `lock` and `write` hold only `'x'` keys today and the `'r'` walk over them costs one seek
that finds nothing, which is a cheaper guarantee than a table of which family may hold which
namespace, and one that cannot go stale.

**4. No compatibility with version 1, and the refusal is typed.** A stream lives for the length of
one transfer between two stores of one cluster, so there is no persisted v1 stream to read.
Refusing a v1 chunk is also the right answer on its own terms: adopting one would produce exactly
the silently incomplete region this version exists to stop.

A v1 chunk is refused on its **first byte**, with `ProtoError::WireVersion { expected: 2, actual:
1 }` — the same error and the same no-downgrade rule as the transport handshake
(`esker_proto::transport`), and deliberately not `Corrupt`: an older store is not damaged, and
only one of those two is worth retrying. First byte matters as much as the type. A v1 pairs chunk
is `1 ++ 2 ++ crc32:u32 ++ ..` and a v2 one is `2 ++ 2 ++ cf:u8 ++ crc32:u32 ++ ..`, so a decoder
that checked the kind before the version would read the first byte of v1's checksum as v2's
column family — a coin flip deciding which family a region's data landed in.
`a_version_one_chunk_is_refused_by_version` pins both halves for both chunk kinds, and
`the_chunk_bytes_are_the_format` is the golden: the header and the pairs chunk byte for byte, so
a change to either is a deliberate format change rather than a re-run expectation.

## What a user would have seen, and why no suite saw it

The columnar learner is where this was found, but it is not the worst case and not the oldest. The
user-facing shape needs no columnar anything:

> A store joins a running cluster. It is added as a learner, the leader's log has already been
> compacted past it, so a snapshot is what catches it up. It is promoted to voter, it is elected,
> and it answers a client's read of a row committed before it joined — with **nothing**. No error,
> no refusal, no epoch complaint: a committed row read as absent, by a leader.

That is `tests/snapshot.rs::a_voter_caught_up_by_snapshot_can_lead_and_answer_an_old_row`, and on
the version-1 stream it fails as `Get { value: None }` where the row is. A peer added after a
compaction, promoted, and elected is a sequence phases 4 and 5 already had; only the read at the
end is new.

**Could the acceptance suites have triggered it? No — and the reason is a gap between files, not a
weak assertion in one.** The two halves never met:

* `crates/esker-store/tests/snapshot.rs` is the only place that installs a snapshot on a peer and
  reads it back afterwards, and before this change it contained **zero** transactional writes
  (`git show 15e2604:crates/esker-store/tests/snapshot.rs | grep -c TxnKvReq` → 0). It wrote
  `RawKV`, which is the one namespace version 1 shipped.
* `crates/esker-store/tests/promotion.rs` drives learners to voters under load — this test's first
  half — but its writer is `RawKv::put` and its assertions are about roles, not values.
* `esker-client`'s transactional suites (`bank`, `anomalies`, `txn_linearizability`,
  `txn_crash_boundaries`, `time_machine`, `txn`, `refusals`) never move a region: no `Operator::`
  and no `AddPeer` appears in any of them, so no snapshot is ever installed under them.
* `crates/esker-store/tests/txnkv.rs` and `esker-txn`'s matrix write Percolator records against a
  single store that never transfers a region.
* `esker-sim`'s `raft_snapshot.rs` is the pure-Raft simulator and does not reference `esker_store`
  at all; `esker-cli`'s `tier_acceptance.rs` has neither transactional writes nor peer moves.

The one place both halves were present is `esker-sql/tests/joint_gate.rs`'s differential — SQL
writes are transactional and PD places a learner that is caught up by snapshot — and that test was
`#[ignore]`d as unrunnable, because the fragment service it reads through did not exist yet
(`docs/plans/phase-8-learner.md` §wiring). So the suite that would have caught this is the suite
that was waiting on the feature that found it.

Two things follow. Any store added to a **running** cluster with data has been receiving an
incomplete region since phase 4c, which the recorded transcript in
`docs/bench/columnar-learner.md` shows happening. And the crossing that was missing —
*transactional data, a transfer, then a read on the receiving store* — is now a test rather than
an observation.

## Consequences

* A snapshot now moves the bytes it says it moves. `tests/snapshot.rs`'s
  `a_region_arrives_with_its_transactional_records`,
  `a_placed_columnar_learner_holds_what_the_leader_holds` and
  `a_voter_caught_up_by_snapshot_can_lead_and_answer_an_old_row` are the regressions, all three
  red before this change in under three seconds.
* Transfers are bigger, by exactly what was being lost. A region of SQL data was previously
  shipping almost nothing.
* `clear_range` writes a range tombstone per family per namespace and discharges each, so
  replacing a held region costs six compactions rather than one. It runs once per transfer.
* A store that begins writing a **third** physical namespace has to add it to
  `PHYSICAL_NAMESPACES`, which does not compile until `physical_ranges` returns a range for it.
  That is the guard against this happening again; it is a compile error rather than a comment
  because the first version of this bug was also a reasonable reading of the code at the time.
* The one-family assumption is worth re-checking wherever else a range is swept. `split.rs`
  samples `default` alone to pick a boundary, which is a size estimate and stays correct; `gc.rs`
  walks the families it collects from deliberately. Neither is a data-loss shape, but a fourth
  instance of "a walk that means the region and reads one family" is the thing to look for.
