//! The membership a peer applied is the membership it comes back with.
//!
//! # The sighting
//!
//! run 123's census, four peers of one region at the same `epoch_conf=7`:
//!
//! ```text
//! store=1 handle_peer=2  core_voters=[2, 9, 10, 13] core_learners=[]
//! store=2 handle_peer=9  core_voters=[2, 9, 10, 13] core_learners=[]
//! store=4 handle_peer=13 core_voters=[2, 9, 10, 13] core_learners=[]
//! store=3 handle_peer=10 core_voters=[2, 9, 13]     core_learners=[10]
//! ```
//!
//! Store 3 calls **itself** a learner while the other three call it a voter. Its own census says
//! when it started doing so, to the line: `core_voters=[2, 9, 10, 13]` for seventy-three rounds,
//! then `node 3 exited with signal: 9`, then `node 3 restarted`, and the first census sixteen
//! milliseconds later is the wrong one. It never corrected itself. A voter that reads itself out
//! of the voters does not campaign (`is_voter(self)`), so the group silently loses a candidate —
//! and nobody is told, because every *other* peer's view is right.
//!
//! # The mechanism, and the numbers name it exactly
//!
//! Two facts move together while a peer runs and come from different records when it opens.
//!
//! * [`RaftPeer`]'s region — the peer list as of the **apply index** — is written by the conf
//!   change's own batch, and read back from that record at open.
//! * `applied_conf`, whose doc says the same words, is rebuilt at open from
//!   [`PersistedState::conf_state`](esker_store::PersistedState) — which is the membership as of
//!   the **truncated index**, a different index, because the core replays the log's conf-change
//!   entries on top of it.
//!
//! So every restart drops from `applied_conf` every conf change applied since the last
//! truncation. The core is still right — it replays those entries — but `applied_conf` is what a
//! **compaction records** and what a **snapshot names**, so the stale value is written back to
//! disk as the anchor and the next restart has nothing left to replay.
//!
//! Store 3 was killed three times. Peer 10 was promoted at 22:16:11, peer 13 at 22:18:11, and the
//! wrong census follows the third kill: a stale `applied_conf` of `[2, 9] + learner 10`, plus the
//! promotion of 13 applied on top of it, is `voters=[2, 9, 13] learners=[10]` — the field's line,
//! to the digit.
//!
//! # What is *not* the mechanism
//!
//! Writing the membership in force into `PersistedState::conf_state` — the first fix this bug
//! suggested — would break the core. That field is specified to be the membership as of the index
//! the log begins after, precisely so `replay_conf_changes` can rebuild the **revertible** tail
//! from the entries above it. Seeding it with the applied membership instead leaves those changes
//! folded into the base, so a leader truncating them away can no longer revert them: the
//! two-leaders-in-one-term the simulator found on `ESKER_SIM_SEED=42705`. The record that answers
//! "the membership as of the apply index" is the **region record**, and it was already in the
//! peer's hand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use esker_engine::{Db, LocalFileSystem, Options, WalSyncMode, WriteBatch, WriteOptions, cf};
use esker_proto::{PeerRole, Region};
use esker_raft::ConfState;
use esker_store::apply::Command;
use esker_store::{
    DiscardTransport, DriverPool, LogCompaction, NoHost, PeerOptions, RaftLogStorage, RaftPeer,
};
use tempfile::TempDir;

const REGION: u64 = 1;
/// The peer this store hosts, and the only voter the group ever has.
const MINE: u64 = 1;
/// The peer the group is told to take on. It lives on a store that does not exist, which is what
/// keeps the arrangement to one process: adding a **learner** does not move the voter set, so the
/// entry commits on a quorum of one and applies — the event this file is about — while the peer
/// itself never has to answer anything.
const ADDED: u64 = 2;

fn open_db(dir: &TempDir) -> Arc<Db> {
    Arc::new(
        Db::open_with(
            dir.path(),
            Options {
                create_if_missing: true,
                wal_sync_mode: WalSyncMode::Never,
                ..Options::default()
            },
            Arc::new(LocalFileSystem::new()),
            &cf::BUILTIN,
        )
        .unwrap(),
    )
}

/// Writes the region record a bootstrapped store leaves behind, so the peers below are started
/// from a record on disk exactly as `Store::open` starts them.
fn bootstrap_record(db: &Arc<Db>) -> Region {
    let region = Region::bootstrap(REGION, MINE, MINE);
    let cf_id = db.cf_id(cf::RAFT).unwrap();
    let mut batch = WriteBatch::new();
    esker_store::meta::stage_region(&mut batch, cf_id, &region);
    db.write(batch, &WriteOptions::synced()).unwrap();
    region
}

/// The region record as it stands on disk — the store's answer to "who is in this group".
fn record(db: &Arc<Db>) -> Region {
    let mut regions = esker_store::meta::load_regions(db).unwrap();
    assert_eq!(regions.len(), 1, "one region was written and one is read");
    regions.pop().unwrap()
}

/// The membership that record names, in the core's vocabulary.
///
/// The same computation `start_peer` does, and the reason it is a function here: it is the
/// definition of "the membership as of the apply index", and both assertions below are that the
/// peer agrees with it.
fn membership(region: &Region) -> ConfState {
    let mut conf = ConfState {
        voters: region
            .peers
            .iter()
            .filter(|peer| peer.role == PeerRole::Voter)
            .map(|peer| peer.peer_id)
            .collect(),
        learners: region
            .peers
            .iter()
            .filter(|peer| matches!(peer.role, PeerRole::Learner | PeerRole::ColumnarLearner))
            .map(|peer| peer.peer_id)
            .collect(),
    };
    conf.normalize();
    conf
}

/// Starts the peer the way `start_peer` does: the core's configuration and the peer's region both
/// come from the region record.
fn start(db: &Arc<Db>, region: &Region, compaction: LogCompaction) -> Arc<RaftPeer> {
    let conf = membership(region);
    let storage = RaftLogStorage::open(Arc::clone(db), REGION, conf.clone()).unwrap();
    RaftPeer::start(
        PeerOptions {
            region: region.clone(),
            peer_id: MINE,
            voters: conf.voters,
            learners: conf.learners,
            seed: 20_260_910,
            compaction,
            columnar: None,
        },
        storage,
        Arc::new(DiscardTransport),
        Arc::new(NoHost),
        Arc::new(DriverPool::new(1).unwrap()),
    )
    .unwrap()
    .commit()
}

/// Ticks until the peer leads its group of one, which is what lets it propose.
async fn lead(peer: &Arc<RaftPeer>) {
    for _ in 0..400 {
        if peer.is_leader() {
            return;
        }
        peer.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("the peer never became leader of its group of one");
}

/// Adds [`ADDED`] as a learner and waits for the change to **apply**, which is the event the
/// region record and `applied_conf` are both supposed to have taken.
async fn add_the_learner(peer: &Arc<RaftPeer>) {
    peer.propose_conf_change(
        esker_raft::ConfChangeKind::AddLearner,
        ADDED,
        ADDED,
        PeerRole::Learner,
    )
    .await
    .expect("a learner does not move the voter set, so a group of one can take one");
}

/// **The value under test.** `SnapshotSource::meta.conf` is `applied_conf` itself: the membership
/// this peer would name in a snapshot it shipped, and the same value a compaction records.
async fn names(peer: &Arc<RaftPeer>) -> ConfState {
    let mut conf = peer
        .snapshot_source()
        .await
        .expect("the peer answers")
        .meta
        .conf;
    conf.normalize();
    conf
}

/// **The defect, read straight off the peer.** A conf change applies; the peer restarts with
/// nothing else happening; the membership it names is not the one its own record holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_peer_names_the_membership_its_record_holds() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let region = bootstrap_record(&db);

    let peer = start(&db, &region, LogCompaction::new());
    lead(&peer).await;
    add_the_learner(&peer).await;

    let applied = record(&db);
    assert_eq!(
        membership(&applied),
        ConfState {
            voters: vec![MINE],
            learners: vec![ADDED],
        },
        "the conf change did not apply, so this test never reached its question"
    );
    assert_eq!(
        names(&peer).await,
        membership(&applied),
        "before the restart the two agree, which is what makes the restart the only event"
    );
    peer.stop();

    let reopened = start(&db, &applied, LogCompaction::new());
    let named = names(&reopened).await;
    reopened.stop();
    assert_eq!(
        named,
        membership(&applied),
        "the peer came back naming a different membership than the one it applied. That value is \
         what a compaction records and what a snapshot carries, so a restart writes it back to \
         disk as the anchor and ships it to the peer being caught up."
    );
}

/// **The field sighting, end to end.** The stale value above is written back to disk by the first
/// compaction after the restart, and from then on the core itself is wrong: the entries that said
/// otherwise have been truncated away, so there is nothing left to replay and nothing to correct
/// it. This is store 3, in one process.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_membership_change_survives_a_compaction_after_the_restart() {
    // Small enough that a handful of writes truncates past the conf change; the allowance is what
    // decides that [`ADDED`], which acknowledges nothing ever, stops holding the log open — the
    // same abandonment a learner that is never coming back gets in the field.
    let compaction = LogCompaction {
        threshold: 4,
        keep: 1,
        slow_peer_allowance: 1,
    };
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    let region = bootstrap_record(&db);

    let peer = start(&db, &region, compaction);
    lead(&peer).await;
    add_the_learner(&peer).await;
    let applied = record(&db);
    let change_at = peer.status().await.unwrap().applied;
    peer.stop();

    // The restart, and then ordinary traffic — which is all a compaction needs.
    let peer = start(&db, &applied, compaction);
    lead(&peer).await;
    for round in 0..20_u64 {
        peer.propose(&Command::Put {
            key: Bytes::from(format!("k{round:03}")),
            value: Bytes::from_static(b"v"),
        })
        .await
        .expect("the leader of a group of one commits its own writes");
    }
    peer.stop();

    let truncated = RaftLogStorage::open(Arc::clone(&db), REGION, membership(&applied))
        .unwrap()
        .truncated_index();
    assert!(
        truncated >= change_at,
        "the log never compacted past the conf change at {change_at} (truncated to {truncated}), \
         so this test never reached its question"
    );

    // Nothing has changed the membership. The only events are a restart and a compaction.
    let reopened = start(&db, &applied, compaction);
    let core = reopened.status().await.unwrap().conf;
    reopened.stop();
    let mut core = core;
    core.normalize();
    assert_eq!(
        core,
        membership(&applied),
        "the core came back with a membership its own region record contradicts, and there is no \
         entry left in the log to correct it. This is run 123's store 3: a peer that reads itself \
         out of the voters does not campaign, and the group loses a candidate without anybody \
         being told."
    );
}
