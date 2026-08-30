# Raft, condensed — and where each rule lives in the code

Ongaro's dissertation, *Consensus: Bridging Theory and Practice* (2014), Figure 3.1, restated rule by
rule, plus the features from later chapters that `esker-raft` implements. **Every rule names the
function that implements it and the test that proves it.** A rule whose "implemented by" column still
says `TBD` is a rule this crate does not yet obey, and the step it is waiting on is named.

This file is the checklist for phase 3's acceptance (`prompts/03-raft.md`). It is not a substitute
for the dissertation: it is an index into our code, written in the dissertation's numbering so the
two can be read side by side.

Paths are relative to `crates/esker-raft/src/`. Test names are the `#[test]` function.

## 0. State (Figure 3.1, "State")

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| S1 | `currentTerm`: latest term seen, initialised to 0, **persistent** | `types::HardState::term`, `core::Raft::term` | `a_fresh_node_is_a_follower_of_nobody` |
| S2 | `votedFor`: candidate voted for in `currentTerm`, or none, **persistent** | `types::HardState::voted_for`, `core::Raft::vote` | TBD (step 1) |
| S3 | `log[]`: entries with a command and the term when received, **persistent**, first index 1 | `types::Entry`, `storage::LogStorage`, `log::RaftLog` | `appended_entries_come_back_by_index_and_term` |
| S4 | `commitIndex`, `lastApplied`: volatile, initialised to 0 | `log::RaftLog::committed`, `log::RaftLog::applied` | `committed_entries_are_handed_out_once_and_in_order` |
| S5 | `nextIndex[]`, `matchIndex[]`: volatile on leaders, reinitialised after election | `progress::Progress::{next, matched}`, `core::Raft::rebuild_progress` | TBD (step 2) |

Our departure from S1–S3: they are persisted by the **driver**, not by this crate, because this
crate does no I/O. `raw_node::Ready` carries them out and its documentation is the contract; see §7.

## 1. AppendEntries (Figure 3.1, "AppendEntries RPC")

Arguments: `term`, `leaderId`, `prevLogIndex`, `prevLogTerm`, `entries[]`, `leaderCommit`.
Results: `term`, `success`. Our `Message::AppendEntries` adds `context`, which carries a `ReadIndex`
round's tag (§6); a heartbeat is this message with `entries` empty.

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| A1 | Reply false if `term < currentTerm` | `core::Raft::step_lower_term` | `an_append_from_an_older_term_is_answered_so_its_sender_learns` |
| A2 | Reply false if the log has no entry at `prevLogIndex` whose term is `prevLogTerm` | `log::RaftLog::{matches, maybe_append}` | `an_append_whose_previous_entry_does_not_match_is_rejected` |
| A3 | If an existing entry conflicts with a new one (same index, different term), delete it and everything after it | `log::RaftLog::{find_conflict, truncate_and_append}` | `an_append_that_conflicts_truncates_from_the_conflict_and_no_earlier`, `truncating_into_the_durable_prefix_shortens_the_log` |
| A4 | Append any new entries not already in the log | `log::RaftLog::maybe_append` | `a_duplicated_append_does_not_shorten_the_log` |
| A5 | If `leaderCommit > commitIndex`, set `commitIndex = min(leaderCommit, index of last new entry)` | `log::RaftLog::maybe_append` | `a_follower_never_commits_past_what_it_holds` |

## 2. RequestVote (Figure 3.1, "RequestVote RPC")

Arguments: `term`, `candidateId`, `lastLogIndex`, `lastLogTerm`. Results: `term`, `voteGranted`.

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| V1 | Reply false if `term < currentTerm` | `core::Raft::step_lower_term` | TBD (step 1) |
| V2 | If `votedFor` is null or `candidateId`, **and** the candidate's log is at least as up to date as this one, grant the vote (§5.2, §5.4.1) | `log::RaftLog::is_up_to_date`, TBD (step 1) | `the_up_to_date_check_compares_term_before_length` |

## 3. Rules for servers (Figure 3.1, "Rules for Servers")

### All servers

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| R1 | If `commitIndex > lastApplied`, apply `log[++lastApplied]` to the state machine | `log::RaftLog::next_committed`, `raw_node::RawNode::{ready, advance}` | `committed_entries_are_handed_out_once_and_in_order` |
| R2 | If a request or response carries `term > currentTerm`, set `currentTerm = term` and become a follower | `core::Raft::step_higher_term` | `an_append_from_an_older_term_is_answered_so_its_sender_learns`, `a_pre_vote_from_a_higher_term_does_not_move_this_node_s_term` (the §9.6 exception) |

### Followers

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| F1 | Respond to RPCs from candidates and leaders | `core::Raft::step` | `stepping_any_message_at_any_term_never_panics` |
| F2 | If no `AppendEntries` from the current leader and no vote granted within the election timeout, become a candidate | `core::Raft::tick_election` | TBD (step 1) |

### Candidates

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| C1 | On conversion: increment `currentTerm`, vote for self, reset the election timer, send `RequestVote` to all other servers | TBD (step 1) | TBD (step 1) |
| C2 | On votes from a majority, become leader | TBD (step 1) | TBD (step 1) |
| C3 | On `AppendEntries` from a new leader, become a follower | TBD (step 1) | TBD (step 1) |
| C4 | If the election times out, start a new one | `core::Raft::tick_election` | TBD (step 1) |

### Leaders

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| L1 | On election, and then periodically, send empty `AppendEntries` to every server so it does not time out | `core::Raft::tick_heartbeat`, TBD (step 2) | TBD (step 2) |
| L2 | On a client command, append the entry, then apply it once committed | TBD (step 2) | TBD (step 2) |
| L3 | If `lastLogIndex >= nextIndex[f]`, send `AppendEntries` from `nextIndex[f]`; on success update `nextIndex[f]` and `matchIndex[f]`, on failure decrement `nextIndex[f]` and retry | TBD (step 2) | TBD (step 2) |
| L4 | If a majority has `matchIndex >= N` for some `N > commitIndex` **and `log[N].term == currentTerm`**, set `commitIndex = N` (§5.4.2) | TBD (step 2) | TBD (step 2) |

L4's term condition is the one this project treats as a first-class trap: committing a prior-term
entry by counting replicas is safe-looking and wrong (`docs/plans/phase-3.md` §6 race 2).

## 4. Safety properties (Figure 3.2)

These are what the simulator and the model check after every event; they are properties of the whole
system, not of one function, so their "implemented by" is the argument, not a line of code.

| # | Property | Argued by | Checked by |
|---|---|---|---|
| P1 | **Election Safety** — at most one leader per term | V2 (a server votes once per term) plus quorum intersection | TBD (step 3 proptest, `esker-sim`) |
| P2 | **Leader Append-Only** — a leader never overwrites or deletes entries in its own log | leaders only append; truncation is a follower path (A3) | TBD (`esker-sim`) |
| P3 | **Log Matching** — two logs agreeing at an index and term are identical up to it | A2's induction | TBD (`esker-sim`) |
| P4 | **Leader Completeness** — a committed entry is present in every future leader's log | V2's up-to-date check plus L4's term condition | TBD (`esker-sim`) |
| P5 | **State Machine Safety** — no two servers apply different commands at the same index | P4 plus R1 | TBD (`esker-sim`) |

## 5. Log compaction and snapshots (dissertation §5)

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| N1 | A snapshot records the last included index and term, and the configuration at that point | `types::SnapshotMeta` | `restoring_a_snapshot_replaces_the_log_and_answers_from_its_metadata` |
| N2 | Discarded entries are still answerable for the term at the boundary, which the consistency check needs | `storage::MemStorage::term`, `log::RaftLog::term` | `compaction_keeps_the_term_at_the_boundary_and_loses_everything_below` |
| N3 | A leader sends `InstallSnapshot` when the entries a follower needs have been compacted | TBD (step 5) | TBD (step 5) |
| N4 | A follower installing a snapshot discards its log and adopts the snapshot's state and configuration | `log::RaftLog::restore` | `a_snapshot_that_disagrees_with_the_log_replaces_it_entirely` |
| N5 | A snapshot that the log has already passed, or that it already matches, is ignored | `log::RaftLog::should_restore` | `a_snapshot_the_log_has_already_passed_is_not_worth_installing`, `a_snapshot_the_log_already_matches_is_not_worth_installing` |

## 6. Beyond Figure 3.1

Features from later chapters, each of which this crate implements.

| # | Feature | Chapter | Implemented by | Tested by |
|---|---|---|---|---|
| X1 | Randomised election timeouts, redrawn per election | §3.4, §9.5 | `core::Raft::reset_election_timeout` | `nodes_sharing_a_seed_still_draw_different_election_timeouts` |
| X2 | **Membership change applied when the entry is appended, not committed** | §4.1 | `conf::ConfTracker::append` | TBD (step 6) |
| X3 | An uncommitted membership change that is truncated reverts the configuration | §4.1 | `conf::ConfTracker::truncate_from` | TBD (step 6) |
| X4 | One membership change at a time | §4.1 | `raw_node::RawNode::propose_conf_change` | TBD (step 6) |
| X5 | Learners: replicate without voting or counting toward quorum | §4.2.1 | `types::ConfState::quorum` | `learners_do_not_count_toward_a_quorum` |
| X6 | Leadership transfer via `TimeoutNow` | §3.10 | TBD (step 6) | TBD (step 6) |
| X7 | Check-quorum: a leader without quorum contact steps down | §6.2 | TBD (step 6) | TBD (step 6) |
| X8 | Pre-vote: a returning node does not bump the term to lose an election | §9.6 | `core::Raft::step_higher_term` (the exemption) | `a_pre_vote_from_a_higher_term_does_not_move_this_node_s_term` |
| X9 | `ReadIndex`: linearizable reads without a log write | §6.4 | `readonly::ReadOnly` | TBD (step 4) |

## 7. What the dissertation leaves to the implementation

Decisions this project made, each of which a future reader might reverse. Anything on this list that
is not yet an ADR becomes one before the phase closes (`prompts/03-raft.md`, "Acceptance").

| Decision | Where | Status |
|---|---|---|
| Heartbeats are `AppendEntries` with no entries, not their own message | `message.rs` | `docs/plans/phase-3.md` §3; ADR TBD |
| A snapshot is acknowledged with `AppendEntriesResponse`, not its own response | `message.rs` | `docs/plans/phase-3.md` §3; ADR TBD |
| The rejection hint carries a term as well as an index, so a leader skips a term per round trip rather than an index | `message.rs`, `progress.rs` | ADR TBD (step 2) |
| Persistence is the driver's, and the ordering is a documented contract rather than an enforced one | `raw_node.rs` | `docs/plans/phase-3.md` §4; ADR TBD |
| Single-server membership change only; joint consensus deferred | `conf.rs` | `docs/DESIGN.md` §5 |
| `ReadIndex` only; no lease reads, which would need a bounded-clock-skew assumption | `readonly.rs` | `docs/DESIGN.md` §5 |
| The RNG is injected and the node id selects its stream | `config.rs` | ADR TBD |
