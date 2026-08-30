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
| S2 | `votedFor`: candidate voted for in `currentTerm`, or none, **persistent** | `types::HardState::voted_for`, `election::Raft::handle_vote_request` | `the_ready_that_grants_a_vote_carries_the_vote_it_recorded`, `a_recorded_vote_survives_a_restart_and_is_not_cast_twice` |
| S3 | `log[]`: entries with a command and the term when received, **persistent**, first index 1 | `types::Entry`, `storage::LogStorage`, `log::RaftLog` | `appended_entries_come_back_by_index_and_term` |
| S4 | `commitIndex`, `lastApplied`: volatile, initialised to 0 | `log::RaftLog::committed`, `log::RaftLog::applied` | `committed_entries_are_handed_out_once_and_in_order` |
| S5 | `nextIndex[]`, `matchIndex[]`: volatile on leaders, reinitialised after election | `progress::Progress::{next, matched}`, `election::Raft::become_leader` | `the_in_flight_window_stops_a_leader_running_away_from_a_slow_follower` |

Our departure from S1–S3: they are persisted by the **driver**, not by this crate, because this
crate does no I/O. `raw_node::Ready` carries them out and its documentation is the contract; see §7.

## 1. AppendEntries (Figure 3.1, "AppendEntries RPC")

Arguments: `term`, `leaderId`, `prevLogIndex`, `prevLogTerm`, `entries[]`, `leaderCommit`.
Results: `term`, `success`. Our `Message::AppendEntries` adds `context`, which carries a `ReadIndex`
round's tag (§6); a heartbeat is this message with `entries` empty.

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| A1 | Reply false if `term < currentTerm` | `core::Raft::step_lower_term` | `an_append_from_an_older_term_is_answered_so_its_sender_learns` |
| A1b | The rejection hint: report the term at the conflict, so the leader skips a term per round trip rather than an entry | `replication::Raft::{conflict_hint, find_conflict_by_term}` | `the_rejection_hint_costs_a_round_trip_per_term_not_per_entry`, `an_append_past_the_end_of_the_log_is_refused_with_the_logs_own_end` |
| A2 | Reply false if the log has no entry at `prevLogIndex` whose term is `prevLogTerm` | `replication::Raft::handle_append_entries`, `log::RaftLog::{matches, maybe_append}` | `an_append_whose_previous_entry_does_not_match_is_rejected`, `an_append_past_the_end_of_the_log_is_refused_with_the_logs_own_end` |
| A3 | If an existing entry conflicts with a new one (same index, different term), delete it and everything after it | `log::RaftLog::{find_conflict, truncate_and_append}` | `an_append_that_conflicts_truncates_from_the_conflict_and_no_earlier`, `truncating_into_the_durable_prefix_shortens_the_log`, `a_follower_replaces_a_divergent_tail` |
| A4 | Append any new entries not already in the log | `log::RaftLog::maybe_append` | `a_duplicated_append_does_not_shorten_the_log`, `replaying_an_append_leaves_the_log_alone` |
| A5 | If `leaderCommit > commitIndex`, set `commitIndex = min(leaderCommit, index of last new entry)` | `log::RaftLog::maybe_append`, `replication::Raft::send_heartbeat` | `a_follower_never_commits_past_what_it_holds`, `a_heartbeat_never_advertises_a_commit_index_past_the_follower` |

## 2. RequestVote (Figure 3.1, "RequestVote RPC")

Arguments: `term`, `candidateId`, `lastLogIndex`, `lastLogTerm`. Results: `term`, `voteGranted`.

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| V1 | Reply false if `term < currentTerm` | `core::Raft::step_lower_term` | `a_vote_request_from_an_older_term_is_ignored` (see §7: we ignore rather than reply, except for pre-votes) |
| V2 | If `votedFor` is null or `candidateId`, **and** the candidate's log is at least as up to date as this one, grant the vote (§5.2, §5.4.1) | `election::Raft::handle_vote_request`, `log::RaftLog::is_up_to_date` | `only_one_vote_is_granted_per_term`, `a_repeated_request_from_the_same_candidate_is_granted_again`, `a_vote_is_refused_to_a_candidate_whose_log_is_behind`, `the_up_to_date_check_compares_term_before_length` |

## 3. Rules for servers (Figure 3.1, "Rules for Servers")

### All servers

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| R1 | If `commitIndex > lastApplied`, apply `log[++lastApplied]` to the state machine | `log::RaftLog::next_committed`, `raw_node::RawNode::{ready, advance}` | `committed_entries_are_handed_out_once_and_in_order`, `nothing_is_offered_twice_after_advance`, `committed_entries_are_durable_or_carried_alongside` |
| R2 | If a request or response carries `term > currentTerm`, set `currentTerm = term` and become a follower | `core::Raft::step_higher_term` | `an_append_from_an_older_term_is_answered_so_its_sender_learns`, `a_pre_vote_from_a_higher_term_does_not_move_this_node_s_term` (the §9.6 exception) |

### Followers

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| F1 | Respond to RPCs from candidates and leaders | `core::Raft::step` | `stepping_any_message_at_any_term_never_panics` |
| F2 | If no `AppendEntries` from the current leader and no vote granted within the election timeout, become a candidate | `core::Raft::tick_election` | `a_group_sharing_one_seed_still_elects_someone` |

### Candidates

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| C1 | On conversion: increment `currentTerm`, vote for self, reset the election timer, send `RequestVote` to all other servers | `election::Raft::{campaign, become_candidate}` | `a_candidate_with_a_majority_becomes_leader`, `a_learner_does_not_campaign` |
| C2 | On votes from a majority, become leader | `election::Raft::{poll, handle_vote_response, become_leader}` | `a_candidate_with_a_majority_becomes_leader`, `a_lone_voter_elects_itself`, `votes_from_nodes_outside_the_configuration_do_not_count`, `a_candidate_refused_by_a_majority_reverts_to_follower` |
| C3 | On `AppendEntries` from a new leader, become a follower | `core::Raft::step_current_term` | `a_candidate_concedes_to_a_leader_of_its_own_term` |
| C4 | If the election times out, start a new one | `core::Raft::tick_election` | `a_partitioned_node_running_pre_votes_never_raises_its_term` |

### Leaders

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| L1 | On election, and then periodically, send empty `AppendEntries` to every server so it does not time out | `core::Raft::tick_heartbeat`, `replication::Raft::{bcast_heartbeat, send_heartbeat}` | `heartbeats_keep_a_follower_from_campaigning`, `a_returning_node_does_not_depose_a_healthy_leader` |
| L0 | On election, append an empty entry of the new term, so §5.4.2 lets the backlog commit | `election::Raft::become_leader` | `a_new_leader_appends_an_empty_entry_of_its_own_term`, `a_new_leader_commits_its_inherited_entries_behind_its_own_no_op` |
| L5 | Flow control: at most `max_inflight_msgs` entry-carrying appends outstanding per follower, and at most `max_size_per_msg` bytes in one | `progress::Inflights`, `replication::Raft::send_append` | `the_in_flight_window_stops_a_leader_running_away_from_a_slow_follower`, `an_append_is_bounded_by_the_byte_budget` |
| L2 | On a client command, append the entry, then apply it once committed | `replication::Raft::propose_entry`, `raw_node::RawNode::propose` | `a_proposal_replicates_and_commits_everywhere`, `a_follower_refuses_a_proposal` |
| L3 | If `lastLogIndex >= nextIndex[f]`, send `AppendEntries` from `nextIndex[f]`; on success update `nextIndex[f]` and `matchIndex[f]`, on failure decrement `nextIndex[f]` and retry | `replication::Raft::{send_append, handle_append_response}`, `progress::Progress::{maybe_update, maybe_decr_to}` | `the_rejection_hint_costs_a_round_trip_per_term_not_per_entry`, `a_leader_without_a_majority_appends_but_does_not_commit` |
| L4 | If a majority has `matchIndex >= N` for some `N > commitIndex` **and `log[N].term == currentTerm`**, set `commitIndex = N` (§5.4.2) | `replication::Raft::maybe_commit` | **`a_prior_term_entry_on_a_majority_does_not_commit_by_counting`**, `a_new_leader_commits_its_inherited_entries_behind_its_own_no_op` |

L4's term condition is the one this project treats as a first-class trap: committing a prior-term
entry by counting replicas is safe-looking and wrong (`docs/plans/phase-3.md` §6 race 2).

## 4. Safety properties (Figure 3.2)

These are what the simulator and the model check after every event; they are properties of the whole
system, not of one function, so their "implemented by" is the argument, not a line of code.

| # | Property | Argued by | Checked by |
|---|---|---|---|
| P1 | **Election Safety** — at most one leader per term | V2 (a server votes once per term) plus quorum intersection | `proptests::{three,five}_nodes_never_have_two_leaders_in_one_term`, `an_even_group_never_breaks_a_tie_by_electing_twice`; `esker-sim` |
| P2 | **Leader Append-Only** — a leader never overwrites or deletes entries in its own log | leaders only append; truncation is a follower path (A3) | TBD (`esker-sim`) |
| P3 | **Log Matching** — two logs agreeing at an index and term are identical up to it | A2's induction | `testkit::Harness::check_log_matching`, run after every action of the proptests; `esker-sim` |
| P4 | **Leader Completeness** — a committed entry is present in every future leader's log | V2's up-to-date check plus L4's term condition | TBD (`esker-sim`) |
| P5 | **State Machine Safety** — no two servers apply different commands at the same index | P4 plus R1 | `testkit::Harness::check_log_matching`'s committed-prefix half, run after every action of the proptests; `esker-sim` |

## 5. Log compaction and snapshots (dissertation §5)

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| N1 | A snapshot records the last included index and term, and the configuration at that point | `types::SnapshotMeta` | `restoring_a_snapshot_replaces_the_log_and_answers_from_its_metadata` |
| N2 | Discarded entries are still answerable for the term at the boundary, which the consistency check needs | `storage::MemStorage::term`, `log::RaftLog::term` | `compaction_keeps_the_term_at_the_boundary_and_loses_everything_below` |
| N3 | A leader sends `InstallSnapshot` when the entries a follower needs have been compacted | `snapshot::Raft::send_snapshot`, reached from `replication::Raft::{send_append, send_heartbeat}` | `a_compacted_leader_sends_a_snapshot_and_waits_for_it`, `a_snapshot_in_flight_is_not_counted_as_replicated` |
| N4 | A follower installing a snapshot discards its log and adopts the snapshot's state and configuration | `snapshot::Raft::{handle_install_snapshot, restore}`, `log::RaftLog::restore` | `installing_a_snapshot_replaces_the_log_and_acknowledges_its_index`, `a_snapshot_discards_an_unpersisted_tail`, `a_snapshot_that_disagrees_with_the_log_replaces_it_entirely` |
| N6 | After installing, the follower's position comes from the snapshot's metadata, and it refuses appends below it | `log::RaftLog::{last_index, term}` | `a_node_that_installed_a_snapshot_still_refuses_a_shorter_candidate`, `an_append_below_the_installed_snapshot_is_refused` |
| N7 | A snapshot in flight is recorded but not counted as replicated; the follower acknowledges with an ordinary `AppendEntriesResponse` | `progress::Progress::become_snapshot`, `snapshot::Raft::handle_install_snapshot` | `a_snapshot_in_flight_is_not_counted_as_replicated`, `a_rejected_snapshot_returns_the_follower_to_probing` |
| N5 | A snapshot that the log has already passed, or that it already matches, is ignored | `log::RaftLog::should_restore` | `a_snapshot_the_log_has_already_passed_is_not_worth_installing`, `a_snapshot_the_log_already_matches_is_not_worth_installing`, `a_snapshot_the_log_has_passed_is_acknowledged_but_not_installed`, `a_snapshot_at_the_end_of_the_index_space_is_refused` |

## 6. Beyond Figure 3.1

Features from later chapters, each of which this crate implements.

| # | Feature | Chapter | Implemented by | Tested by |
|---|---|---|---|---|
| X1 | Randomised election timeouts, redrawn per election | §3.4, §9.5 | `core::Raft::reset_election_timeout`, called from `reset` and `become_pre_candidate` | `nodes_sharing_a_seed_still_draw_different_election_timeouts`, `a_group_sharing_one_seed_still_elects_someone` |
| X2 | **Membership change applied when the entry is appended, not committed** | §4.1 | `conf::ConfTracker::append` | TBD (step 6) |
| X3 | An uncommitted membership change that is truncated reverts the configuration | §4.1 | `conf::ConfTracker::truncate_from` | TBD (step 6) |
| X4 | One membership change at a time | §4.1 | `raw_node::RawNode::propose_conf_change` | TBD (step 6) |
| X5 | Learners: replicate without voting or counting toward quorum | §4.2.1 | `types::ConfState::quorum` | `learners_do_not_count_toward_a_quorum` |
| X6 | Leadership transfer via `TimeoutNow` | §3.10 | TBD (step 6) | TBD (step 6) |
| X7 | Check-quorum, voter half: a follower with a healthy leader refuses votes | §6.2 | `core::Raft::vetoed_by_leader_lease` | `check_quorum_makes_a_follower_refuse_a_vote_while_its_leader_is_healthy`, `a_forced_vote_request_is_not_vetoed_by_the_lease` |
| X7b | Check-quorum, leader half: a leader without quorum contact steps down | §6.2 | TBD (step 6) | TBD (step 6) |
| X8 | Pre-vote: a returning node does not bump the term to lose an election | §9.6 | `core::Raft::step_higher_term` (the exemption), `election::Raft::{campaign, become_pre_candidate}` | `a_pre_vote_from_a_higher_term_does_not_move_this_node_s_term`, `a_granted_pre_vote_records_no_vote`, `a_partitioned_node_running_pre_votes_never_raises_its_term`, `without_pre_vote_a_returning_node_deposes_the_leader`, `a_late_pre_vote_grant_does_not_count_toward_a_real_election` |
| X9 | `ReadIndex`: linearizable reads without a log write | §6.4 | `readonly::Raft::{read_index, record_read_ack, answer_read}` | `a_read_is_answered_at_the_commit_index_after_a_heartbeat_quorum`, `a_leader_without_a_quorum_answers_no_read`, `a_read_does_not_survive_a_change_of_leadership`, `an_earlier_round_completes_with_a_later_one` |
| X9b | A leader postpones reads until an entry of **its own term** has committed — until then its commit index is inherited and unproven | §6.4 | `readonly::Raft::{has_committed_in_current_term, flush_postponed_reads}` | `a_read_waits_until_the_leader_has_committed_in_its_own_term` |
| X9c | A follower forwards a read to the leader and reports the answer | §6.4 | `readonly::Raft::{read_index, handle_read_index_response}` | `a_follower_forwards_a_read_and_reports_the_answer`, `a_read_on_a_node_that_knows_no_leader_is_dropped` |

### The driver contract

Not in Figure 3.1 at all, because the paper assumes a server that persists its own state. Splitting
the decision from the I/O is what makes this crate simulatable, and it moves five rules out of the
algorithm and into `Ready`'s documentation.

| # | Rule | Implemented by | Tested by |
|---|---|---|---|
| D1 | Persist `hard_state` and `entries` before sending `messages` from the same `Ready` | `raw_node::Ready` (documentation), `testkit::Harness::drain_ready` (a driver that obeys it) | `the_ready_that_grants_a_vote_carries_the_vote_it_recorded`, `a_message_never_precedes_the_entries_it_depends_on`; violations are `esker-sim`'s to inject |
| D2 | Apply `snapshot` before `entries` | `raw_node::RawNode::advance`, `testkit::Harness::drain_ready` | `a_snapshot_discards_an_unpersisted_tail`, `a_compacted_leader_sends_a_snapshot_and_waits_for_it` |
| D3 | Apply `committed_entries` in order, exactly once | `raw_node::RawNode::{ready, advance}` | `committed_entries_are_durable_or_carried_alongside`, `nothing_is_offered_twice_after_advance` |
| D4 | Answer a read only past its index | `readonly`, `raw_node::Ready::read_states` | the rule is the driver's; `testkit::Harness::drain_ready` records read states as a driver would, and `esker-sim` injects violations |
| D5 | A `Ready` not advanced is re-offered unchanged | `raw_node::RawNode::ready` | `a_ready_that_is_not_advanced_is_offered_again` |

D1 is also what makes the leader's own bookkeeping sound: a leader counts itself as holding an entry
the moment it appends one, before any fsync, and that is safe only because no follower can
acknowledge the entry until the leader has sent it, and it may not send until it has persisted.

## 7. What the dissertation leaves to the implementation

Decisions this project made, each of which a future reader might reverse. Anything on this list that
is not yet an ADR becomes one before the phase closes (`prompts/03-raft.md`, "Acceptance").

| Decision | Where | Status |
|---|---|---|
| Heartbeats are `AppendEntries` with no entries, not their own message | `message.rs` | `docs/plans/phase-3.md` §3; ADR TBD |
| A `RequestVote` from an older term is **ignored**, not refused — Figure 3.1 says reply false. A stale candidate cannot win whatever we say, and staying quiet keeps a looping node from being answered forever. A stale *pre-vote* is refused, because that reply is how a node behind the cluster learns its term | `core.rs` | ADR TBD |
| A pre-vote round redraws the election timeout even though it does not reset the term, so two nodes that pre-campaigned together do not do so again | `election.rs` | ADR TBD |
| A snapshot is acknowledged with `AppendEntriesResponse`, not its own response | `message.rs` | `docs/plans/phase-3.md` §3; ADR TBD |
| The rejection hint carries a term as well as an index, so a leader skips a term per round trip rather than an index | `message.rs`, `replication.rs` | ADR TBD |
| An append carrying no entries does not consume the in-flight window — it is a heartbeat by another name, and charging it would throttle the messages that advertise a new commit index | `replication.rs` | ADR TBD |
| Persistence is the driver's, and the ordering is a documented contract rather than an enforced one | `raw_node.rs` | `docs/plans/phase-3.md` §4; ADR TBD |
| Single-server membership change only; joint consensus deferred | `conf.rs` | `docs/DESIGN.md` §5 |
| `ReadIndex` only; no lease reads, which would need a bounded-clock-skew assumption | `readonly.rs` | `docs/DESIGN.md` §5 |
| The RNG is injected and the node id selects its stream | `config.rs` | ADR TBD |
