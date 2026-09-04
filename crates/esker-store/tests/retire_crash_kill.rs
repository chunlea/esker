//! A retirement interrupted by a real `SIGKILL`, on a real filesystem.
//!
//! `tests/retire.rs` covers the same rule against a simulated crash: it stops the store, performs
//! the first of a retirement's two durable steps, and reopens. That is the precise instrument —
//! it can stop exactly between the steps, which no signal can be aimed at — and it proves the
//! recovery logic. It cannot prove the half that is about the *process*: that the announcement is
//! on the platter and not in a buffer when the process ceases to exist, and that `Store::open` of
//! a database a killed process left behind finishes the retirement.
//!
//! So this is the coarse instrument on the real thing, in the shape
//! `crates/esker-engine/tests/crash_kill.rs` established: the test binary re-executes *itself*
//! with `--exact` and an environment variable, so the child is this file's
//! [`the_child_retires_and_waits_to_be_killed`] and there is no second binary to keep in step.
//!
//! # Why the kill is aimed rather than random
//!
//! The window is two adjacent synced writes; a kill thrown at a running retirement would land in
//! it about never, and a test that reaches its subject about never is a test that passes for
//! other reasons. The child instead performs step one, **reports that it is durable**, and then
//! waits to be killed. The kill is real and the process death is real; what is arranged is only
//! *when* it happens, which is the one thing the fault has to have in common with the bug.
//!
//! # What the child leaves behind
//!
//! A database whose region record is gone, whose range still holds keys, and which announces the
//! retirement of a range nothing owns — and it leaves it by dying, so nothing ran on the way out.
//! The parent then does what an operator does: it starts the store again.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use esker_proto::{RawKvReq, Region, RequestHeader};
use esker_store::pd::{FakePd, PdClient, StoreInfo};
use esker_store::server::RaftOptions;
use esker_store::split::SplitOptions;
use esker_store::{LogCompaction, PeerAddress, Store, StoreOptions};

/// Names the child. Its absence is what tells this file it is the parent.
const ENV_DIR: &str = "ESKER_RETIRE_CRASH_DIR";

/// The child is this file's own test, selected by name.
const CHILD_TEST: &str = "the_child_retires_and_waits_to_be_killed";

/// Keys the child writes into the range that is then retired.
const KEYS: u32 = 8;

/// What the child prints once step one of the retirement is durable.
const READY: &str = "RETIRED ";

fn key(n: u32) -> Bytes {
    Bytes::from(format!("k{n:05}").into_bytes())
}

fn raft_options(peers: Vec<PeerAddress>, bootstrap_voters: Option<Vec<u64>>) -> RaftOptions {
    let mut raft = RaftOptions::new(peers, 20_260_904);
    raft.tick = Duration::from_millis(25);
    raft.compaction = LogCompaction::new();
    raft.bootstrap_voters = bootstrap_voters;
    raft
}

fn store_options(
    store_id: u64,
    pd: &Arc<FakePd>,
    raft: RaftOptions,
    address: String,
) -> StoreOptions {
    StoreOptions {
        store_id,
        peer_id: store_id,
        region_id: store_id,
        raft: Some(raft),
        pd: Some(Arc::clone(pd) as Arc<dyn PdClient>),
        address,
        heartbeat_tick: Duration::from_millis(5),
        store_heartbeat: Duration::from_millis(20),
        region_heartbeat: Duration::from_millis(20),
        split: SplitOptions {
            region_split_size: u64::MAX,
            max_sampled_keys: 1024,
        },
        ..StoreOptions::new()
    }
}

/// Keys inside a region's range, per shipped column family.
fn counts(store: &Arc<Store>, region: &Region) -> Vec<(&'static str, usize)> {
    esker_store::snapshot::key_counts(store.db(), region)
        .unwrap()
        .to_vec()
}

// ---------------------------------------------------------------------------------------
// The child
// ---------------------------------------------------------------------------------------

/// Opens a store, fills a region, performs step one of its retirement, and waits to be killed.
///
/// In an ordinary run [`ENV_DIR`] is unset and this returns at once; it is a test at all only so
/// that the parent can select it with `--exact`.
#[test]
fn the_child_retires_and_waits_to_be_killed() {
    let Ok(dir) = std::env::var(ENV_DIR) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let pd = Arc::new(FakePd::new());
        let address = "127.0.0.1:59991".to_string();
        let peers = vec![PeerAddress::new(1, 1, address.parse().unwrap())];
        let store = Store::open(
            &dir,
            store_options(1, &pd, raft_options(peers, Some(vec![1])), address),
        )
        .unwrap();

        // A leader, so the writes below are committed rather than merely proposed.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !store.peer_of(1).is_some_and(|peer| peer.is_leader()) {
            assert!(std::time::Instant::now() < deadline, "no leader");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let region = store.regions().regions()[0].clone();
        for n in 0..KEYS {
            store
                .serve(
                    RequestHeader::new(region.id, region.epoch, 0),
                    RawKvReq::put(key(n), Bytes::from_static(b"v")),
                )
                .await
                .unwrap();
        }
        let held = counts(&store, &region);
        assert!(
            held.iter().any(|(_, keys)| *keys > 0),
            "the child wrote nothing to retire: {held:?}"
        );

        // Step one, exactly as `Store::retire_region` does it: the peer stops, then one synced
        // batch destroys the Raft state and the `'m'` record and announces the range.
        store.stop();
        esker_store::raft_log::destroy(store.db(), region.id, Some(&region)).unwrap();

        let default_keys = held
            .iter()
            .find(|(name, _)| *name == esker_engine::cf::DEFAULT)
            .map_or(0, |(_, keys)| *keys);
        println!("{READY}{default_keys}");
        std::io::stdout().flush().unwrap();

        // Step two never runs. The process is about to stop existing.
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    });
}

// ---------------------------------------------------------------------------------------
// The parent
// ---------------------------------------------------------------------------------------

/// A `SIGKILL` between a retirement's two durable steps costs a restart, not the range.
#[test]
fn a_sigkilled_retirement_is_finished_at_the_next_open() {
    if std::env::var(ENV_DIR).is_ok() {
        return; // This process *is* a child; it runs the test above.
    }
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().expect("the test binary has a path"))
        .args([CHILD_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(ENV_DIR, dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the child");

    // The child's marker, on a thread so a child that never gets there cannot hang the suite.
    // Searched for rather than stripped as a prefix: the harness's own `test <name> ... ` has no
    // trailing newline, so the child's first line is shared with it.
    let stdout = child.stdout.take().expect("the child's stdout is a pipe");
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(at) = line.find(READY) {
                let keys: usize = line[at + READY.len()..].trim().parse().unwrap_or(0);
                let _ = sender.send(keys);
                break;
            }
        }
    });
    let wrote = match receiver.recv_timeout(Duration::from_secs(120)) {
        Ok(keys) => keys,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child never reported a durable retirement: {error}");
        }
    };
    assert!(wrote > 0, "the child retired a range it had not filled");

    // The crash.
    child.kill().expect("killing the child");
    let status = child.wait().expect("reaping the child");
    assert!(
        !status.success(),
        "the child exited rather than being killed"
    );

    // The restart. A placement driver that already has a cluster, so this store is told it hosts
    // nothing — which is what a store removed from its only region really is told.
    let pd = Arc::new(FakePd::new());
    pd.bootstrap(&StoreInfo {
        store_id: 9,
        address: "127.0.0.1:59999".to_string(),
    })
    .unwrap();
    let address = "127.0.0.1:59992".to_string();
    let peers = vec![PeerAddress::new(1, 1, address.parse().unwrap())];
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let restarted = runtime
        .block_on(async {
            Store::open(
                dir.path(),
                store_options(1, &pd, raft_options(peers, None), address),
            )
        })
        .unwrap();

    assert!(
        restarted.regions().is_empty(),
        "the restarted store hosts a region whose record was destroyed"
    );
    let region = Region::bootstrap(1, 1, 1);
    assert_eq!(
        counts(&restarted, &region),
        vec![
            (esker_engine::cf::DEFAULT, 0),
            (esker_engine::cf::LOCK, 0),
            (esker_engine::cf::WRITE, 0)
        ],
        "a retirement a SIGKILL interrupted left its {wrote} keys on disk for ever"
    );
    assert!(
        esker_store::meta::load_retiring(restarted.db())
            .unwrap()
            .is_empty(),
        "the retirement finished but is still announced"
    );
    restarted.stop();
}
