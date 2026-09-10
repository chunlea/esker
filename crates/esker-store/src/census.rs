//! What every region's peer believes, on a cadence, so a run can be diagnosed after it ends.
//!
//! # Why this exists
//!
//! run 120 killed four stores in eighty seconds, lost no acknowledged write, and then stopped
//! acknowledging any for the remaining hundred and eleven seconds. Six mechanisms fit that
//! equally well from outside — below quorum, a returning peer that never catches up, a term
//! ladder, [ADR 0099](../../../docs/adr/0099-one-core-per-region-per-store.md)'s displaced core,
//! a leader that leads and cannot commit, and a client that cannot find who does — and nothing a
//! *client* can see separates them. Every one of them is a statement about what the stores
//! believed, and no store said.
//!
//! `esker-store/tests/promotion.rs` has composed exactly the right line since debt #9 was found
//! with it — term, role, believed leader, **which core answered**, membership from two sources,
//! and the election counters — but only inside a test. This is that line, from a running store.
//!
//! # A cadence, and never an event
//!
//! [`esker_raft::Counters`]' own documentation records why: *a promotion stall that reproduces
//! one run in ten under load passed 4 of 4 with `RUST_LOG=esker_raft=debug`*. An instrument that
//! logs each election event displaces the race it was built to find. A counter read on a timer
//! does not — it costs one round trip per region per period, and the period is the operator's.
//!
//! # Off unless asked for
//!
//! [`crate::StoreOptions::region_census`] is `None` by default. `esker server --region-census-ms
//! <n>` and `esker cluster start --region-census-ms <n>` turn it on.
//!
//! # The driver is asked, and a driver that does not answer is the finding
//!
//! Three of the six candidates are only visible in state the region's **driver thread** holds:
//! the role, who this peer voted for, which core answers for the region, and the counters. So a
//! census round asks, with [`ANSWER_WITHIN`] to bound it — and reports a peer that did not answer
//! in time as exactly that. A driver too busy to answer for half a second is not a gap in the
//! record; on a cluster that has stopped serving it is the most interesting line in it.

use std::time::{Duration, Instant};

use esker_proto::Epoch;

/// How long one region's driver has to answer before the census gives up on it for this round.
///
/// Short, because the census must never be the reason a round is late, and because "it did not
/// answer" is itself an answer worth having on time.
pub const ANSWER_WITHIN: Duration = Duration::from_millis(500);

/// One region, as this store's peer for it believes it to be.
///
/// Every `Option` here means **the driver did not answer**, not "there is nothing to say".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionCensus {
    /// The store this line came from.
    ///
    /// **Peer ids are not store ids**, and run 122 is where that cost a reading: its census named
    /// peers 2, 9 and 10 on a four-store cluster, and nothing on the line said which store held
    /// which — so "three kills and none of them interrupted a peer's stream" could not be told
    /// from "the kills hit stores whose peers are not in this list". A peer id is allocated by the
    /// placement driver when a peer joins a region; a store id is the store's own.
    pub store_id: u64,
    /// The region.
    pub region_id: u64,
    /// Its epoch, from the region record this store keeps.
    pub epoch: Epoch,
    /// The peer id the *handle* in the region map publishes.
    pub handle_peer: u64,
    /// The peer id the core that **answered** reports as its own.
    ///
    /// The two differ only in the state [ADR 0099](../../../docs/adr/0099-one-core-per-region-per-store.md)
    /// closes: a handle whose core was displaced publishes nothing while another core answers for
    /// the region, and the request path reads the published pair. #9's original line could not say
    /// this, and saying it is what turned that line into a mechanism.
    pub answered_by: Option<u64>,
    /// The term the handle publishes.
    pub term: u64,
    /// What the core believes it is.
    pub role: Option<String>,
    /// Whether the handle says this store leads the region.
    pub is_leader: bool,
    /// Who the handle believes leads it.
    pub believes_leader: Option<u64>,
    /// Who the core voted for in its term.
    pub voted_for: Option<u64>,
    /// How far the handle says the state machine has applied.
    pub applied: u64,
    /// The core's commit index.
    pub commit: Option<u64>,
    /// The last index in the core's log.
    pub last_index: Option<u64>,
    /// The membership **the core** believes is in force.
    pub core_voters: Vec<u64>,
    /// The learners in that membership.
    pub core_learners: Vec<u64>,
    /// The peers **the region record** names, which is what the placement driver believes.
    ///
    /// Beside `core_voters` because the two coming apart is
    /// [ADR 0085](../../../docs/adr/0085-a-vote-is-not-granted-to-a-learner.md)'s stall.
    pub record_peers: Vec<u64>,
    /// The election counters, monotonic for the life of the process.
    pub elections: Option<esker_raft::Counters>,
    /// Why the driver's half is missing, when it is.
    pub unanswered: Option<String>,
}

impl RegionCensus {
    /// Emits this census as one `info` event.
    ///
    /// Fields rather than a formatted sentence: the default `tracing` formatter renders them as
    /// `key=value`, which greps like a log line and parses like a record, and a reader chasing one
    /// region does not have to know how the sentence was worded.
    pub fn emit(&self) {
        let elections = self.elections.unwrap_or_default();
        tracing::info!(
            target: "esker_store::census",
            store = self.store_id,
            region = self.region_id,
            epoch_conf = self.epoch.conf_ver,
            epoch_version = self.epoch.version,
            handle_peer = self.handle_peer,
            answered_by = self.answered_by,
            term = self.term,
            // `%` and not the default: a `&str` field is rendered with `Debug`, so `role` would
            // arrive as `role="Leader"` and every grep for `role=Leader` would miss it.
            role = %self.role.as_deref().unwrap_or("?"),
            is_leader = self.is_leader,
            believes_leader = self.believes_leader,
            voted_for = self.voted_for,
            applied = self.applied,
            commit = self.commit,
            last_index = self.last_index,
            core_voters = ?self.core_voters,
            core_learners = ?self.core_learners,
            record_peers = ?self.record_peers,
            campaigns_pre = elections.campaigns_pre,
            campaigns_real = elections.campaigns_real,
            vote_requests_sent = elections.vote_requests_sent,
            vote_responses_granted = elections.vote_responses_granted,
            vote_responses_ignored = elections.vote_responses_ignored,
            check_quorum_step_downs = elections.check_quorum_step_downs,
            unanswered = self.unanswered.as_deref(),
            "region census"
        );
    }
}

/// How long is left of a round's budget, or `None` when it is spent.
///
/// A store with many regions and a stalled driver would otherwise make one round outlast its own
/// period; past the budget the remaining regions are still reported, as unanswered, because a
/// census that skipped them would look like a store that had stopped hosting them.
#[must_use]
pub fn left_of(budget: Duration, started: Instant) -> Option<Duration> {
    budget
        .checked_sub(started.elapsed())
        .filter(|left| !left.is_zero())
}
