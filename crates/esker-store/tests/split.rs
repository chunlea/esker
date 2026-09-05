//! A region dividing in two, and everything that has to stay true while it does.
//!
//! 4a's tests hand-built the state a split leaves behind. These make the split actually happen —
//! a leader notices its region is over the threshold, picks a boundary, asks the placement driver
//! for ids, proposes an entry, and every peer applies it — and then check the properties that a
//! hand-built state cannot check: that the halves tile the parent exactly, that a write the split
//! overtook is refused rather than misfiled, and that a restart finds both halves or neither.
//!
//! The store here replicates with a single peer. That is not a shortcut around consensus: the
//! entry still goes through the log, still applies through the driver, and still writes both
//! records in the apply batch. It removes only the network, which `tests/cluster.rs` covers and
//! which would make a ten-split test a ten-second one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use esker_engine::{Db, LocalFileSystem, Options, WalSyncMode, WriteBatch, WriteOptions, cf};
use esker_proto::txn::TxnStatus;
use esker_proto::{
    ProtoError, RawKvReq, RawKvResp, Region, RequestHeader, TxnKvReq, TxnKvResp, TxnMutation,
};
use esker_raft::{ConfState, Entry, EntryKind, HardState};
use esker_store::apply::Command;
use esker_store::pd::{FakePd, PdClient};
use esker_store::server::RaftOptions;
use esker_store::split::SplitOptions;
use esker_store::{PeerAddress, RaftLogStorage, Store, StoreOptions, meta};

/// A region is split once it holds this many bytes — small enough that a few hundred keys reach
/// it, so a test that splits ten times takes milliseconds rather than gigabytes.
const TINY_SPLIT_SIZE: u64 = 8 * 1024;

struct Harness {
    store: Arc<Store>,
    _pd: Arc<FakePd>,
    _dir: tempfile::TempDir,
}

fn open(split: SplitOptions) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let pd = Arc::new(FakePd::new());
    let mut raft = RaftOptions::new(
        vec![PeerAddress::new(1, 1, "127.0.0.1:1".parse().unwrap())],
        20_260_830,
    );
    // Fast enough that a lone voter elects itself in a few milliseconds.
    raft.tick = Duration::from_millis(5);

    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id: 1,
            peer_id: 1,
            region_id: 1,
            raft: Some(raft),
            pd: Some(Arc::clone(&pd) as Arc<dyn PdClient>),
            address: "127.0.0.1:20160".to_owned(),
            heartbeat_tick: Duration::from_millis(5),
            split,
            ..StoreOptions::new()
        },
    )
    .unwrap();
    Harness {
        store,
        _pd: pd,
        _dir: dir,
    }
}

fn tiny() -> Harness {
    open(SplitOptions {
        region_split_size: TINY_SPLIT_SIZE,
        max_sampled_keys: 1024,
    })
}

/// A store that will never split, for the tests that want the threshold out of the way.
fn never() -> Harness {
    open(SplitOptions {
        region_split_size: u64::MAX,
        max_sampled_keys: 1024,
    })
}

impl Harness {
    async fn wait_for_leader(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self
            .store
            .regions()
            .states()
            .iter()
            .any(|state| state.peer().is_some_and(|peer| peer.is_leader()))
        {
            assert!(Instant::now() < deadline, "no region ever elected a leader");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// The header a client would send for `key`: the region covering it, at the epoch this store
    /// currently holds. Looking it up per call is exactly what a client's cache does after a
    /// refresh, and it is what makes a write during a split land on the right half.
    fn header_for(&self, key: &[u8]) -> Option<RequestHeader> {
        let state = self.store.regions().find(key)?;
        Some(RequestHeader::new(state.id(), state.region().epoch, 0))
    }

    /// Writes one key through the replicated path, retrying while the routing moves under it.
    async fn put(&self, key: &[u8], value: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let header = self.header_for(key).expect("every key is covered");
            let request = RawKvReq::put(Bytes::copy_from_slice(key), Bytes::copy_from_slice(value));
            match self.store.serve(header, request).await {
                Ok(_) => return,
                Err(error) => {
                    assert!(
                        error.is_retryable(),
                        "writing {key:?} failed terminally: {error}"
                    );
                    assert!(Instant::now() < deadline, "writing {key:?} never succeeded");
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        }
    }

    /// Commits one key as a client would: prewrite, then commit, each through the replicated
    /// path and each retried while the routing moves under it.
    ///
    /// One key per transaction, because a batch spanning a boundary the split has just drawn is
    /// two regions' work and this test is about the split, not about cross-region commits.
    async fn commit(&self, key: &[u8], value: &[u8], start_ts: u64) {
        let mutation = TxnMutation::Put {
            key: Bytes::copy_from_slice(key),
            value: Bytes::copy_from_slice(value),
            read_ts: None,
        };
        self.txn(
            key,
            TxnKvReq::Prewrite {
                start_ts,
                primary: Bytes::copy_from_slice(key),
                ttl_ms: 3_000,
                mutations: vec![mutation],
            },
        )
        .await;
        self.txn(
            key,
            TxnKvReq::Commit {
                start_ts,
                commit_ts: start_ts + 1,
                keys: vec![Bytes::copy_from_slice(key)],
            },
        )
        .await;
    }

    /// One transactional request, retried while the routing moves under it, refusing anything
    /// that came back as a Percolator status rather than as an error.
    async fn txn(&self, key: &[u8], request: TxnKvReq) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let header = self.header_for(key).expect("every key is covered");
            match self.store.serve_txn(header, request.clone()).await {
                Ok(TxnKvResp::Prewrite { keys }) => {
                    assert!(
                        keys.iter().all(|status| *status == TxnStatus::Ok),
                        "prewriting {key:?}: {keys:?}"
                    );
                    return;
                }
                Ok(TxnKvResp::Commit { status }) => {
                    assert_eq!(status, TxnStatus::Ok, "committing {key:?}");
                    return;
                }
                Ok(other) => panic!("{other:?}"),
                Err(error) => {
                    assert!(
                        error.is_retryable(),
                        "a transaction on {key:?} failed terminally: {error}"
                    );
                    assert!(
                        Instant::now() < deadline,
                        "a transaction on {key:?} never succeeded"
                    );
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        }
    }

    /// Reads one committed key at `ts`, through whichever region now owns it.
    async fn read(&self, key: &[u8], ts: u64) -> Option<Bytes> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let header = self.header_for(key).expect("every key is covered");
            let request = TxnKvReq::Get {
                key: Bytes::copy_from_slice(key),
                ts,
            };
            match self.store.serve_txn(header, request).await {
                Ok(TxnKvResp::Get { value }) => return value,
                Ok(other) => panic!("{other:?}"),
                Err(error) => {
                    assert!(error.is_retryable(), "reading {key:?}: {error}");
                    assert!(Instant::now() < deadline, "reading {key:?} never succeeded");
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        }
    }

    async fn get(&self, key: &[u8]) -> Option<Bytes> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let header = self.header_for(key).expect("every key is covered");
            let request = RawKvReq::get(Bytes::copy_from_slice(key));
            match self.store.serve(header, request).await {
                Ok(RawKvResp::Get { value }) => return value,
                Ok(other) => panic!("{other:?}"),
                Err(error) => {
                    assert!(error.is_retryable(), "reading {key:?}: {error}");
                    assert!(Instant::now() < deadline, "reading {key:?} never succeeded");
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        }
    }

    async fn wait_for_regions(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.store.regions().len() < count {
            assert!(
                Instant::now() < deadline,
                "the store stopped at {} regions, wanted {count}",
                self.store.regions().len()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

fn key(n: u32) -> Vec<u8> {
    format!("k{n:06}").into_bytes()
}

/// The user key of row `n` of one SQL table, built by `esker-keys` — what `esker-sql` hands the
/// transaction layer, and what the store therefore has to be able to measure and divide.
fn row_key(n: u32) -> Vec<u8> {
    let mut key = esker_keys::prefix::table_row_prefix(1, 7);
    key.extend_from_slice(&n.to_be_bytes());
    key
}

/// **The invariant the phase-4 simulator checks globally and this checks locally.** Every region
/// this store hosts, in key order, must tile `["", "")` exactly: no gap, no overlap, and the last
/// one unbounded.
fn assert_contiguous_partition(regions: &[Region]) {
    assert!(
        !regions.is_empty(),
        "a store hosting nothing owns no partition"
    );
    assert_eq!(
        regions[0].start_key,
        Bytes::new(),
        "the partition does not start at the beginning of the key space"
    );
    for pair in regions.windows(2) {
        assert_eq!(
            pair[0].end_key, pair[1].start_key,
            "regions {} and {} leave a gap or overlap",
            pair[0].id, pair[1].id
        );
        assert!(
            !pair[0].end_key.is_empty(),
            "region {} is unbounded but is not the last",
            pair[0].id
        );
    }
    assert_eq!(
        regions.last().unwrap().end_key,
        Bytes::new(),
        "the last region does not reach the end of the key space"
    );
}

// -- the split itself -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_region_that_grows_past_the_threshold_splits() {
    let harness = tiny();
    harness.wait_for_leader().await;
    assert_eq!(harness.store.regions().len(), 1);

    let value = vec![b'v'; 256];
    for n in 0..64 {
        harness.put(&key(n), &value).await;
    }
    harness.wait_for_regions(2).await;

    let regions = harness.store.regions().regions();
    assert_contiguous_partition(&regions);
    assert_eq!(regions[0].id, 1, "the parent keeps its id");
    assert_ne!(regions[1].id, 1);

    // Every region's `version` moved past the epoch every cached copy was made at, so no client
    // holding the pre-split region can be served. How many splits ran is deliberately not pinned:
    // the trigger is a size *hint*, and how many writes it takes to cross a threshold is not exact.
    for region in &regions {
        assert!(
            region.epoch.version >= 2,
            "region {} is still at the pre-split version",
            region.id
        );
        assert_eq!(
            region.epoch.conf_ver, 1,
            "a split is not a membership change"
        );
    }

    // The boundary is a key that exists, and it is the child's start.
    assert!(regions[1].start_key.starts_with(b"k"), "{:?}", regions[1]);

    // Every key written before the split is still readable, through whichever half now owns it.
    for n in 0..64 {
        assert_eq!(
            harness.get(&key(n)).await,
            Some(Bytes::from(value.clone())),
            "key {n} was lost by the split"
        );
    }
}

/// The child's Raft group starts from nothing. Its `conf_state` is the split-time membership,
/// which for a log that begins at index 0 is exactly what `InitialState::conf_state` is specified
/// to be — the anchor rule of `91de89a`, applied to a region with no history.
#[tokio::test(flavor = "multi_thread")]
async fn the_child_starts_its_own_group_on_the_parents_stores() {
    let harness = tiny();
    harness.wait_for_leader().await;
    let value = vec![b'v'; 256];
    for n in 0..64 {
        harness.put(&key(n), &value).await;
    }
    harness.wait_for_regions(2).await;

    let regions = harness.store.regions().regions();
    // The last two: whichever region split most recently, and the half it produced.
    let (parent, child) = (&regions[regions.len() - 2], &regions[regions.len() - 1]);
    assert_eq!(
        parent.peers.iter().map(|p| p.store_id).collect::<Vec<_>>(),
        child.peers.iter().map(|p| p.store_id).collect::<Vec<_>>(),
        "the child is replicated by the same stores"
    );
    assert!(
        child.peers.iter().all(|c| !parent.peers.contains(c)),
        "the child's peer ids are fresh"
    );

    // And it elects: a group of one whose log starts empty reaches office on its own.
    let deadline = Instant::now() + Duration::from_secs(5);
    let peer = harness
        .store
        .peer_of(child.id)
        .expect("the child has a peer");
    while !peer.is_leader() {
        assert!(
            Instant::now() < deadline,
            "the child never elected a leader"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // Its log is its own: an index in the parent's log says nothing about the child's.
    assert!(harness.store.peer_of(parent.id).is_some());
    harness.put(&child.start_key.clone(), b"x").await;
    assert_eq!(
        harness.get(&child.start_key).await,
        Some(Bytes::from_static(b"x"))
    );
}

/// The prompt's test: continuous writes across a region that splits ten times. Every write must be
/// durable and readable afterwards, and a scan across every boundary must come back in order.
#[tokio::test(flavor = "multi_thread")]
async fn ten_splits_under_continuous_writes_lose_nothing() {
    let harness = tiny();
    harness.wait_for_leader().await;

    let value = vec![b'v'; 256];
    let mut written: Vec<Vec<u8>> = Vec::new();
    for n in 0..600u32 {
        harness.put(&key(n), &value).await;
        written.push(key(n));
        if harness.store.regions().len() > 10 {
            break;
        }
    }
    harness.wait_for_regions(11).await;

    let regions = harness.store.regions().regions();
    assert!(regions.len() >= 11, "only {} regions", regions.len());
    assert_contiguous_partition(&regions);

    // Every write is still there, through whichever region now owns its key.
    for key in &written {
        assert_eq!(
            harness.get(key).await,
            Some(Bytes::from(value.clone())),
            "{key:?} was lost across the splits"
        );
    }

    // And a scan of each region comes back in order, with the union covering everything written.
    let mut seen: Vec<Bytes> = Vec::new();
    for region in &regions {
        let header = RequestHeader::new(region.id, region.epoch, 0);
        let request = RawKvReq::Scan {
            start: region.start_key.clone(),
            end: region.end_key.clone(),
            limit: 0,
            reverse: false,
        };
        let RawKvResp::Scan { pairs } = harness.store.serve(header, request).await.unwrap() else {
            panic!("not a scan response");
        };
        for (key, _) in pairs {
            assert!(
                seen.last().is_none_or(|last| *last < key),
                "a scan crossed a boundary out of order at {key:?}"
            );
            seen.push(key);
        }
    }
    assert_eq!(
        seen.len(),
        written.len(),
        "the regions' scans do not cover everything that was written"
    );
}

/// **The finding of `docs/plans/phase-16-mpp.md` §10, through the store rather than through PD.**
///
/// A SQL row is the *user key* of a transaction, so it reaches the engine as
/// `'x' ++ enc(key) ++ !ts` in the Percolator families — and the rows here are short enough to be
/// inlined into their `write` records, so the `default` family holds not one byte of this table.
/// Before [ADR 0073](../../../docs/adr/0073-a-regions-size-is-the-data-families-it-spans.md) the
/// region reported `~0` bytes and this stayed at one region until the deadline, at any threshold.
///
/// The assertions are the ones the finding needs: the split **happened**, both halves hold data,
/// and the boundary is a row.
#[tokio::test(flavor = "multi_thread")]
async fn a_region_of_committed_rows_splits() {
    const ROWS: u32 = 128;

    let harness = tiny();
    harness.wait_for_leader().await;
    assert_eq!(harness.store.regions().len(), 1);

    // Short enough to be inlined into the `write` record, which is what an ordinary SQL row is:
    // `esker_txn::SHORT_VALUE_MAX_LEN` is 255 and this is under it, so `default` stays empty and
    // every byte of the region is in `write`.
    let value = vec![b'v'; 200];
    for n in 0..ROWS {
        harness
            .commit(&row_key(n), &value, 10 + u64::from(n) * 2)
            .await;
    }
    harness.wait_for_regions(2).await;

    let regions = harness.store.regions().regions();
    assert_contiguous_partition(&regions);
    assert_eq!(regions[0].id, 1, "the parent keeps its id");

    // Every half holds rows. A split whose child owns a range with nothing in it is a hole in the
    // key space wearing a region's clothes, and it is what a boundary drawn from the wrong
    // keyspace would produce.
    for region in &regions {
        let counts = esker_store::snapshot::key_counts(harness.store.db(), region).unwrap();
        let held: usize = counts.iter().map(|(_, count)| count).sum();
        assert!(
            held > 0,
            "region {} owns {:?}..{:?} and holds nothing: {counts:?}",
            region.id,
            region.start_key,
            region.end_key
        );
    }

    // The boundary is a row of the table, not a synthesised midpoint.
    let boundary = &regions[1].start_key;
    assert!(
        (0..ROWS).any(|n| row_key(n) == boundary[..]),
        "the boundary is not one of the rows: {boundary:?}"
    );

    // Every epoch moved past the one a cached copy was made at, and a split is not a membership
    // change.
    for region in &regions {
        assert!(
            region.epoch.version >= 2,
            "region {} {:?}",
            region.id,
            region
        );
        assert_eq!(region.epoch.conf_ver, 1);
    }

    // And nothing was lost: every committed row still reads, through whichever half owns it.
    let ts = 10 + u64::from(ROWS) * 2;
    for n in 0..ROWS {
        assert_eq!(
            harness.read(&row_key(n), ts).await,
            Some(Bytes::from(value.clone())),
            "row {n} was lost by the split"
        );
    }
}

/// The measurement that made the finding, at the layer PD reads it from: a region holding this
/// table reports bytes, and the two halves' sizes add up to something like the whole.
///
/// `region_sizes` is private to the store, so this asks the same question the heartbeat does —
/// `split::approximate_size` — through the public path the store reports on.
#[tokio::test(flavor = "multi_thread")]
async fn a_region_of_committed_rows_reports_its_bytes() {
    const ROWS: u32 = 64;

    let harness = never();
    harness.wait_for_leader().await;

    let value = vec![b'v'; 200];
    for n in 0..ROWS {
        harness
            .commit(&row_key(n), &value, 10 + u64::from(n) * 2)
            .await;
    }

    let regions = harness.store.regions().regions();
    assert_eq!(regions.len(), 1, "this store was told never to split");
    let size = esker_store::split::approximate_size(harness.store.db(), &regions[0]).unwrap();
    let written = u64::from(ROWS) * 200;
    assert!(
        size >= written / 2,
        "{ROWS} committed rows of 200 bytes report {size} bytes, which is not a table PD can see"
    );
}

// -- what a split does to a request in flight --------------------------------------------

/// A client holding the pre-split region asks about a range that is now several. The refusal
/// carries **every** half it touches, so one round trip repairs the cache — and the parent no
/// longer serves the keys it gave away.
#[tokio::test(flavor = "multi_thread")]
async fn the_parent_stops_serving_what_it_gave_away() {
    let harness = tiny();
    harness.wait_for_leader().await;
    let stale = harness.header_for(b"k000000").expect("covered");

    let value = vec![b'v'; 256];
    for n in 0..64 {
        harness.put(&key(n), &value).await;
    }
    harness.wait_for_regions(2).await;
    let regions = harness.store.regions().regions();
    let boundary = regions[1].start_key.clone();

    // The pre-split epoch is refused, and the refusal names both halves.
    let error = harness
        .store
        .serve(stale, RawKvReq::scan(&b""[..], &b""[..], 0))
        .await
        .unwrap_err();
    match error {
        ProtoError::EpochNotMatch { current_regions } => {
            assert_eq!(current_regions.len(), regions.len());
            assert_contiguous_partition(&current_regions);
        }
        other => panic!("{other:?}"),
    }

    // At the *current* epoch, the parent still refuses a key that is now the child's — this is
    // the range check, not the epoch check, and it is the one that survives a stale client
    // guessing the right epoch.
    let parent = RequestHeader::new(regions[0].id, regions[0].epoch, 0);
    let error = harness
        .store
        .serve(parent, RawKvReq::get(boundary.clone()))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ProtoError::KeyNotInRegion { .. }),
        "the parent served a key it gave away: {error:?}"
    );
}

// -- restart ------------------------------------------------------------------------------

/// A crash has both halves or neither, because both records went into the batch that carried
/// `apply_index`. And the split entry is still in the log, so the restart re-applies it — which
/// must be a **no-op**, not a second split.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_finds_both_halves_and_does_not_split_again() {
    let dir = tempfile::tempdir().unwrap();
    let pd = Arc::new(FakePd::new());
    let options = |split: SplitOptions| {
        let mut raft = RaftOptions::new(
            vec![PeerAddress::new(1, 1, "127.0.0.1:1".parse().unwrap())],
            20_260_830,
        );
        raft.tick = Duration::from_millis(5);
        StoreOptions {
            store_id: 1,
            peer_id: 1,
            region_id: 1,
            raft: Some(raft),
            pd: Some(Arc::clone(&pd) as Arc<dyn PdClient>),
            address: "127.0.0.1:20160".to_owned(),
            heartbeat_tick: Duration::from_millis(5),
            split,
            ..StoreOptions::new()
        }
    };

    let before = {
        let store = Store::open(
            dir.path(),
            options(SplitOptions {
                region_split_size: TINY_SPLIT_SIZE,
                max_sampled_keys: 1024,
            }),
        )
        .unwrap();
        let harness = Harness {
            store,
            _pd: Arc::clone(&pd),
            _dir: tempfile::tempdir().unwrap(),
        };
        harness.wait_for_leader().await;
        let value = vec![b'v'; 256];
        for n in 0..64 {
            harness.put(&key(n), &value).await;
        }
        harness.wait_for_regions(2).await;
        // **Stopped before the regions are read**, and that ordering is the whole of this test's
        // stability. `wait_for_regions(2)` returns at *at least* two, and sixteen kilobytes of
        // values over an eight-kilobyte threshold does not stop at two: measured, this store is
        // at four regions when the wait returns and at five two hundred milliseconds later. Read
        // before the stop, `before` is a snapshot of a moving store and any split that lands
        // between the read and the stop makes the comparison below fail — which is what it did,
        // as a test that passed alone and failed under load. `stop` aborts the split checker and
        // drains the drivers, so what is read after it is what is on disk.
        harness.store.stop();
        let regions = harness.store.regions().regions();
        harness.store.flush().unwrap();
        regions
    };
    assert!(before.len() >= 2, "nothing split: {before:?}");

    // Reopened with the threshold out of reach, so nothing splits *again* on its own and any
    // third region could only have come from the replayed entry.
    let store = Store::open(
        dir.path(),
        options(SplitOptions {
            region_split_size: u64::MAX,
            max_sampled_keys: 1024,
        }),
    )
    .unwrap();
    let after = store.regions().regions();
    assert_eq!(
        after, before,
        "the restart did not recover the split exactly"
    );
    assert_contiguous_partition(&after);

    // Let the log replay finish, then check nothing was added by it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        store.regions().regions(),
        before,
        "replaying the split entry split again"
    );
    store.stop();
}

/// A store below the threshold does not split, however long it runs. The trigger is a size, not a
/// timer, and a store that split on a timer would shard an empty database into nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_small_region_is_left_alone() {
    let harness = never();
    harness.wait_for_leader().await;
    for n in 0..32 {
        harness.put(&key(n), b"v").await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(harness.store.regions().len(), 1);
    assert_contiguous_partition(&harness.store.regions().regions());
}

// -- the crash inside the split -----------------------------------------------------------

/// **Both or neither, constructed rather than raced.**
///
/// A `kill -9` during a split apply leaves one of exactly two states, because both halves' records
/// and `apply_index` are in the same batch: either the batch landed — both halves on disk, the
/// entry behind `apply_index` — or it did not, and neither half exists while the entry is still in
/// the log. There is no third state to test for, and the second is the one worth constructing: the
/// restart has to *finish* the split rather than forget it.
///
/// Built by hand, like `tests/restart.rs`: a durable, committed `Split` entry with the apply index
/// left behind it, which is exactly what losing power between the two writes produces.
/// Leaves a database in the state a crash *inside* a split apply produces: region 1 bootstrapped,
/// a committed `Split` entry in its log, and `apply_index` still behind it.
fn crash_inside_a_split(dir: &tempfile::TempDir, split_key: &Bytes) {
    let db = Arc::new(
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
    );
    // Region 1 as a bootstrap leaves it: the whole key space, one peer on this store.
    let cf_id = db.cf_id(cf::RAFT).unwrap();
    let mut batch = WriteBatch::new();
    meta::stage_region(&mut batch, cf_id, &Region::bootstrap(1, 1, 1));
    db.write(batch, &WriteOptions::synced()).unwrap();

    let entries = vec![
        Entry {
            term: 1,
            index: 1,
            kind: EntryKind::Normal,
            data: Bytes::new(),
        },
        Entry {
            term: 1,
            index: 2,
            kind: EntryKind::Normal,
            data: Command::Split {
                split_key: split_key.clone(),
                new_region_id: 7,
                new_peer_ids: vec![70],
            }
            .encode(),
        },
    ];
    let mut storage =
        RaftLogStorage::open(Arc::clone(&db), 1, ConfState::from_voters(vec![1])).unwrap();
    let mut batch = WriteBatch::new();
    storage.stage_ready(
        &mut batch,
        Some(HardState {
            term: 1,
            voted_for: Some(1),
            commit: 2,
        }),
        &entries,
    );
    db.write(batch, &WriteOptions::synced()).unwrap();
    assert_eq!(storage.applied_index(), 0, "the apply index is behind");
}

/// **Both or neither, constructed rather than raced.**
///
/// A `kill -9` during a split apply leaves one of exactly two states, because both halves' records
/// and `apply_index` are in the same batch: either the batch landed — both halves on disk, the
/// entry behind `apply_index` — or it did not, and neither half exists while the entry is still in
/// the log. There is no third state to test for, and the second is the one worth constructing: the
/// restart has to *finish* the split rather than forget it.
///
/// Built by hand, like `tests/restart.rs`: a durable, committed `Split` entry with the apply index
/// left behind it, which is exactly what losing power between the two writes produces.
#[tokio::test(flavor = "multi_thread")]
async fn a_split_committed_but_not_applied_is_finished_by_the_restart() {
    let dir = tempfile::tempdir().unwrap();
    let split_key = Bytes::from_static(b"m");
    crash_inside_a_split(&dir, &split_key);

    // Reopened with the threshold out of reach, so a second region can only have come from the
    // entry that was already in the log.
    let pd = Arc::new(FakePd::new());
    let mut raft = RaftOptions::new(
        vec![PeerAddress::new(1, 1, "127.0.0.1:1".parse().unwrap())],
        20_260_830,
    );
    raft.tick = Duration::from_millis(5);
    let store = Store::open(
        dir.path(),
        StoreOptions {
            store_id: 1,
            peer_id: 1,
            region_id: 1,
            raft: Some(raft),
            pd: Some(Arc::clone(&pd) as Arc<dyn PdClient>),
            address: "127.0.0.1:20160".to_owned(),
            heartbeat_tick: Duration::from_millis(5),
            split: SplitOptions {
                region_split_size: u64::MAX,
                max_sampled_keys: 1024,
            },
            ..StoreOptions::new()
        },
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while store.regions().len() < 2 {
        assert!(
            Instant::now() < deadline,
            "the restart never finished the split"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let regions = store.regions().regions();
    assert_eq!(regions.len(), 2, "{regions:?}");
    assert_contiguous_partition(&regions);
    assert_eq!(regions[0].id, 1);
    assert_eq!(regions[0].end_key, split_key);
    assert_eq!(regions[1].id, 7, "the id the entry named, not a fresh one");
    assert_eq!(regions[1].start_key, split_key);
    assert_eq!(
        regions[1]
            .peers
            .iter()
            .map(|p| p.peer_id)
            .collect::<Vec<_>>(),
        vec![70],
        "the peer ids the entry named"
    );
    for region in &regions {
        assert_eq!(region.epoch.version, 2);
    }
    store.stop();
}

/// The repair loop, end to end. A client holding the pre-split region is refused, takes the
/// regions the refusal carried, picks the one covering its key, and its next attempt succeeds —
/// which is the whole reason the refusal carries them.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_client_can_route_from_what_the_refusal_carried() {
    let harness = tiny();
    harness.wait_for_leader().await;
    let stale = harness.header_for(b"k000050").expect("covered");

    let value = vec![b'v'; 256];
    for n in 0..64 {
        harness.put(&key(n), &value).await;
    }
    harness.wait_for_regions(2).await;

    // The client still believes the pre-split region and asks for its key.
    let request = RawKvReq::get(Bytes::from(key(50)));
    let error = harness
        .store
        .serve(stale, request.clone())
        .await
        .unwrap_err();
    let ProtoError::EpochNotMatch { current_regions } = error else {
        panic!("expected an epoch refusal");
    };

    // It learns them, finds the one that owns its key, and asks again — no `GetRegion` needed.
    let owner = current_regions
        .iter()
        .find(|region| region.contains(&key(50)))
        .expect("the refusal named every region the request touched");
    let repaired = RequestHeader::new(owner.id, owner.epoch, 0);
    assert_eq!(
        harness.store.serve(repaired, request).await.unwrap(),
        RawKvResp::Get {
            value: Some(Bytes::from(value))
        }
    );
}
