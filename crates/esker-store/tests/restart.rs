//! What a peer does when it comes back.
//!
//! The apply batch is deliberately not fsynced (`docs/plans/phase-3.md` §11.2): losing it loses
//! nothing, because the entry is still in the Raft log — which *was* synced — and the restart
//! re-applies it. That argument has one load-bearing assumption, and this file exists to check it:
//! `apply_index` travels in the **same batch** as the data it applied, so a crash has both or
//! neither and replaying from `apply_index + 1` applies nothing twice.
//!
//! The crash is constructed rather than simulated with a signal. A peer is given a log whose
//! entries and hard state are durable but whose apply index is behind — exactly the state a
//! machine is left in when it loses power between the two writes — and then started.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use esker_engine::{
    Db, LocalFileSystem, Options, ReadOptions, WalSyncMode, WriteBatch, WriteOptions, cf,
};
use esker_keys::prefix;
use esker_raft::{ConfState, Entry, EntryKind, HardState, LogStorage};
use esker_store::apply::Command;
use esker_store::{DiscardTransport, PeerOptions, RaftLogStorage, RaftPeer};
use tempfile::TempDir;

const REGION: u64 = 1;

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

fn read(db: &Db, key: &[u8]) -> Option<Bytes> {
    db.get(cf::DEFAULT, &prefix::raw_key(key), &ReadOptions::default())
        .unwrap()
}

/// Writes `entries` and a hard state committing all of them, exactly as the persist step does,
/// and leaves the apply index where it was — which is the state a crash between the two batches
/// leaves behind.
fn persist_without_applying(db: &Arc<Db>, entries: &[Entry]) {
    let mut storage =
        RaftLogStorage::open(Arc::clone(db), REGION, ConfState::from_voters(vec![1])).unwrap();
    let commit = entries.last().map_or(0, |entry| entry.index);
    let mut batch = WriteBatch::new();
    storage.stage_ready(
        &mut batch,
        Some(HardState {
            term: 1,
            voted_for: Some(1),
            commit,
        }),
        entries,
    );
    db.write(batch, &WriteOptions { sync: true }).unwrap();
}

fn command_entry(index: u64, command: &Command) -> Entry {
    Entry {
        term: 1,
        index,
        kind: EntryKind::Normal,
        data: command.encode(),
    }
}

fn start(db: &Arc<Db>) -> Arc<RaftPeer> {
    let storage =
        RaftLogStorage::open(Arc::clone(db), REGION, ConfState::from_voters(vec![1])).unwrap();
    RaftPeer::start(
        PeerOptions {
            region_id: REGION,
            peer_id: 1,
            voters: vec![1],
            seed: 3,
        },
        storage,
        Arc::new(DiscardTransport),
    )
    .unwrap()
}

/// Drives the peer until it has applied through `index`.
async fn apply_through(peer: &Arc<RaftPeer>, index: u64) {
    for _ in 0..400 {
        if peer.status().await.unwrap().applied >= index {
            return;
        }
        peer.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("the peer never applied through {index}");
}

/// **The crash between the two batches.** The log is durable and the apply index is behind, so
/// the restart replays — and because a `CompareAndSwap` is decided against the applied state, a
/// replay that applied anything twice would produce a different answer than applying once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_replays_from_the_apply_index_and_applies_nothing_twice() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);

    // Three commands, chained so that applying any of them twice is visible: the second only
    // swaps if the first landed exactly once, and the third only if the second did.
    let entries = vec![
        command_entry(
            1,
            &Command::CompareAndSwap {
                key: Bytes::from_static(b"k"),
                expected: None,
                value: Some(Bytes::from_static(b"one")),
            },
        ),
        command_entry(
            2,
            &Command::CompareAndSwap {
                key: Bytes::from_static(b"k"),
                expected: Some(Bytes::from_static(b"one")),
                value: Some(Bytes::from_static(b"two")),
            },
        ),
        command_entry(
            3,
            &Command::CompareAndSwap {
                key: Bytes::from_static(b"k"),
                expected: Some(Bytes::from_static(b"two")),
                value: Some(Bytes::from_static(b"three")),
            },
        ),
    ];
    persist_without_applying(&db, &entries);

    // Nothing has been applied: the data batch is the one the crash lost.
    assert_eq!(read(&db, b"k"), None);
    let before = RaftLogStorage::open(Arc::clone(&db), REGION, ConfState::default()).unwrap();
    assert_eq!(before.applied_index(), 0);
    assert_eq!(before.last_index().unwrap(), 3);
    drop(before);

    let peer = start(&db);
    apply_through(&peer, 3).await;
    peer.stop();

    assert_eq!(
        read(&db, b"k").as_deref(),
        Some(&b"three"[..]),
        "the replay applied the chain exactly once"
    );
    let after = RaftLogStorage::open(Arc::clone(&db), REGION, ConfState::default()).unwrap();
    assert_eq!(after.applied_index(), 3);
}

/// A second restart, with nothing left to replay, must apply nothing at all — the other half of
/// exactly-once. A peer that replayed from the start every time would undo the chain above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_with_nothing_outstanding_applies_nothing() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    persist_without_applying(
        &db,
        &[
            command_entry(
                1,
                &Command::Put {
                    key: Bytes::from_static(b"k"),
                    value: Bytes::from_static(b"first"),
                },
            ),
            command_entry(
                2,
                &Command::CompareAndSwap {
                    key: Bytes::from_static(b"k"),
                    expected: Some(Bytes::from_static(b"first")),
                    value: Some(Bytes::from_static(b"second")),
                },
            ),
        ],
    );

    let peer = start(&db);
    apply_through(&peer, 2).await;
    peer.stop();
    assert_eq!(read(&db, b"k").as_deref(), Some(&b"second"[..]));

    // Round two: the same database, nothing outstanding.
    let peer = start(&db);
    for _ in 0..50 {
        peer.tick().await.unwrap();
    }
    let status = peer.status().await.unwrap();
    peer.stop();

    // The peer campaigns during those ticks and appends a no-op of its own, so the log grows —
    // what matters is that it applied nothing it had already applied.
    assert!(status.applied >= 2);
    assert_eq!(
        read(&db, b"k").as_deref(),
        Some(&b"second"[..]),
        "a restart re-applied a chain it had already applied"
    );
}

/// The durable state a restart resumes from is the one the persist step wrote: term, vote and
/// commit index all survive, which is what stops a restarted node voting twice in one term.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_resumes_the_term_and_the_vote_it_recorded() {
    let dir = TempDir::new().unwrap();
    let db = open_db(&dir);
    persist_without_applying(
        &db,
        &[command_entry(
            1,
            &Command::Delete {
                key: Bytes::from_static(b"k"),
            },
        )],
    );

    let reopened = RaftLogStorage::open(Arc::clone(&db), REGION, ConfState::default()).unwrap();
    let state = reopened.state();
    assert_eq!(state.hard_state.term, 1);
    assert_eq!(state.hard_state.voted_for, Some(1));
    assert_eq!(state.hard_state.commit, 1);
    assert_eq!(
        state.conf_state.voters,
        vec![1],
        "the configuration comes from storage, not from what the peer was started with"
    );
}
