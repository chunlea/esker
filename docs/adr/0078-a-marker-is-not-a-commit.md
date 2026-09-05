# 0078 — A marker is not a commit

* Status: accepted
* Date: 2026-09-05

## Context

The `write` column family holds three kinds of record for a key, distinguished by
`WriteRecord::kind`: `Put` and `Delete` are **versions** — what a reader sees — while `Rollback`
and `Lock` are **bookkeeping**. `Kind::is_a_version` is the predicate that separates them, and
`docs/txn-spec.md` §5.1 states the rule for reads: a reader that meets a non-version record steps
past it to the next older one. `percolator::newest_version_at` implements exactly that, and
`esker-txn`'s `a_read_steps_past_a_rollback_marker_to_the_value_beneath` has guarded it since the
read path was written.

A rollback marker is written at **`commit_ts == start_ts`** (§5.4), which is what makes this sharp:
it is not filed off to one side, it is filed among the commits, above every commit older than the
transaction that died. Any code that asks "what is the newest record above `ts`" and reads only the
timestamp cannot tell a transaction that *committed* from one that *took the key and died*.

Two predicates did exactly that.

`TxnSnapshot::newest_write_after` was documented as "the newest `write` record with
`commit_ts > ts`" and both implementations returned the newest record of any kind. It has two
callers, and both of them are asking about **commits**:

* `percolator::check_prewrite` — first-committer-wins. A marker above the writer's snapshot was
  returned as the `winner`, and since a marker's `start_ts` is not the prewriting transaction's,
  the prewrite was refused: `40001` for a conflict with a transaction that committed nothing. The
  refusal even named the marker's timestamp, telling the client *"a commit at 50 beat you"* about a
  moment at which nothing was committed.
* `txnkv::latest_commit` — the `LatestCommit` question of [ADR 0067](0067-the-check-mutation-and-the-latest-commit-question.md).
  It compensated with `.filter(|version| version.record.kind != Kind::Rollback)` **on the `Option`**,
  which does not step past the marker to the commit beneath it: it discards the answer. So whenever
  the newest record on a key happened to be a marker, the store answered `None` — *nothing ever
  committed this key* — for a key with a perfectly good commit under it.

The second one is what made the first one reachable in `esker-sql`. `StoreTxn::changed_since_statement`
(ADR 0057's re-read, the thing whose entire job is to notice the writer in front) asks `LatestCommit`;
a `None` means "nothing moved", the statement does not re-run, and it is then refused at prewrite —
the precise failure `changed_since_statement` exists to prevent. It showed up as
`concurrent_increments_do_not_lose_one_against_real_stores` refusing a handful of 400 increments,
intermittently, with no load needed and no lost update: a spurious refusal, not a correctness
failure, and rare only because it needs a marker to be the newest record at the moment the check runs.

`newest_write_in_range`, the read-set range check of ADR 0067 §3, had the same defect in its own
shape: a marker inside the range counted as a phantom and refused a `SERIALIZABLE` prewrite. It had
no test at all.

## Options

1. **Filter at each caller.** What `latest_commit` already tried. It cannot work in general: the
   caller receives one record and cannot see what is underneath it, so the honest filter is not a
   filter but a second query. Three callers, three chances to forget — and the range caller had
   already forgotten.
2. **A separate `newest_commit_after` alongside the existing predicate.** Keeps the old meaning
   available. But nothing wants the old meaning: `write_of_txn` is how a resolver finds a specific
   transaction's record, marker included, and it is a different question asked by key rather than by
   timestamp. A predicate with no callers is a trap for the next reader.
3. **Make the predicate mean what its callers ask.** Chosen.

## Decision

`TxnSnapshot::newest_write_after` answers with the newest **committed** write above `ts`, stepping
past `Kind::Rollback` records. `newest_write_in_range` skips them the same way and keeps scanning.

**The predicate is `!= Rollback`, not `is_a_version`**, and the difference is `Kind::Lock`. The
first attempt at this change used `is_a_version` — the read path's predicate — and
`a_lock_kind_record_is_a_conflict_but_not_a_version` failed at once, which is exactly what that
test is for. A `Lock` record is written by a `SELECT … FOR UPDATE` that *committed*: it advances
`commit_ts` and it beat us, it simply left no value behind. A read steps past it because a read
wants a version; a conflict check must not, because it asks who committed. The two questions agree
about `Rollback` and differ about `Lock`, and the original `.filter(kind != Kind::Rollback)` in
`latest_commit` had this exactly right — its predicate was never the bug, only where it was
applied.

Both implementations change: the store walks on rather than stopping (`walk` already takes the
closure that decides), and `MemoryStore` stops looking at only the newest record. `latest_commit`
drops its `.filter`, which is now not merely redundant but wrong-shaped.

This is a change to a documented contract, not a bug fix behind it, which is why it is recorded
here: a future reader who sees `is_a_version` in a conflict check and thinks it is over-strict needs
to find this file rather than remove it.

## Consequences

* A transaction that rolls back no longer refuses the next writer of the same key, and no longer
  makes `LatestCommit` deny that the key was ever committed. The `40001`s this produced were always
  spurious — first-committer-wins was firing with no first committer.
* A refusal's `commit_ts` now always names a real commit. It could previously name a marker, which
  is a timestamp at which, by construction, nothing was committed.
* Nothing becomes more permissive in the direction that loses data: a commit *under* a marker is
  still found and still refuses, which is the half of the rule that
  `a_commit_under_a_rollback_marker_still_refuses_and_names_itself` exists to hold down. The
  distinction is between "somebody committed" and "somebody tried and gave up", and only the first
  is a conflict.
* `SELECT … FOR UPDATE` is unaffected: its `Lock` record still conflicts, still answers
  `LatestCommit`, and still counts as a phantom in a checked range.
* `write_of_txn` is unchanged and still finds a transaction's own marker — that is how `settled`,
  `rollback` and the resolver classify a transaction, and it asks by `start_ts`, not by "newest".

## Tests

* `esker-txn`: `a_rollback_marker_is_not_a_commit_and_refuses_nobody`,
  `a_commit_under_a_rollback_marker_still_refuses_and_names_itself` — the second one is the guard
  against over-skipping, and it also pins the reported `commit_ts`.
* `esker-store`: `the_newest_commit_survives_a_marker_written_above_it` (the `LatestCommit` face,
  which is the one `esker-sql` feels) and `a_marker_inside_a_checked_range_is_not_a_phantom` (the
  range face, previously untested).
* All four were run against the unfixed source and fail there. What they report on it is the
  defect stated in the machine's own words: `LatestCommit { newest: None }` for a key committed at
  20, `Prewrite { keys: [Conflict { commit_ts: 50 }] }` for a range nobody committed in, and
  `Conflict { commit_ts: 50 }` where the commit that really beat the writer was at 40.
