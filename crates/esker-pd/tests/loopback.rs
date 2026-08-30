//! The placement driver over a real socket.
//!
//! `prompts/04-multiraft-pd.md` asks for it directly: "pd serve → bootstrap → heartbeats →
//! `GetRegion` over real TCP". Everything here goes through the kernel and through the framing,
//! so the encodings, the service dispatch, the cluster check and the engine underneath are all
//! exercised as one thing — which is the only way to find the seam where two of them disagree.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_pd::clock::TestClock;
use esker_pd::{Clock, Pd, PdOptions, PdService};
use esker_proto::transport::{Server, ServerHandle, TransportConfig};
use esker_proto::{
    Epoch, Operator, PdChannel, Peer, ProtoError, Region, StoreInfo, TcpTransport, Transport,
};

/// A PD on an ephemeral port, and the clock driving it.
struct Cluster {
    _dir: tempfile::TempDir,
    clock: Arc<TestClock>,
    pd: Arc<Pd>,
    handle: ServerHandle,
}

impl Cluster {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let pd = Pd::open(
            dir.path(),
            PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>),
        )
        .unwrap();
        let handle = Server::bind(
            "127.0.0.1:0",
            PdService::new(Arc::clone(&pd)),
            TransportConfig::new(),
        )
        .await
        .unwrap()
        .spawn()
        .unwrap();
        Self {
            _dir: dir,
            clock,
            pd,
            handle,
        }
    }

    async fn channel(&self) -> PdChannel {
        let transport = TcpTransport::connect(self.handle.local_addr())
            .await
            .unwrap();
        PdChannel::new(Arc::new(transport) as Arc<dyn Transport>)
    }
}

/// The sequence the phase prompt names, in order, over TCP.
#[tokio::test]
async fn bootstrap_then_heartbeats_then_a_lookup() {
    let cluster = Cluster::start().await;
    let pd = cluster.channel().await;

    // Bootstrap. The first store gets the region covering everything.
    let (cluster_id, region) = pd
        .bootstrap(StoreInfo::new(1, "127.0.0.1:20160"))
        .await
        .unwrap();
    assert_ne!(cluster_id, 0);
    let region = region.expect("the first store bootstraps the cluster");
    assert_eq!(region.id, 1);
    assert!(region.start_key.is_empty() && region.end_key.is_empty());
    assert_eq!(
        pd.cluster_id(),
        cluster_id,
        "the channel latched the cluster id"
    );

    // A store heartbeat, then a region heartbeat naming a leader.
    cluster.clock.advance(10_000);
    pd.store_heartbeat(1, 1 << 40, 1 << 39, 1, 1, 4_096)
        .await
        .unwrap();
    let leader = region.peers[0].peer_id;
    pd.region_heartbeat(region.clone(), leader, 3, 1 << 20, 42)
        .await
        .unwrap();

    // And the lookup, which now knows who leads and how to reach it.
    let (found, hint, stores) = pd
        .get_region(&b"any key"[..])
        .await
        .unwrap()
        .expect("a region covers every key");
    assert_eq!(found, region);
    assert_eq!(hint, Some(leader));
    assert_eq!(stores, vec![StoreInfo::new(1, "127.0.0.1:20160")]);

    // Ids and timestamps, over the same connection.
    let first = pd.alloc_id(4).await.unwrap();
    let second = pd.alloc_id(1).await.unwrap();
    assert_eq!(second, first + 4, "the block of four was really reserved");

    let start_ts = pd.tso(8).await.unwrap();
    let next_ts = pd.tso(1).await.unwrap();
    assert_eq!(next_ts, start_ts + 8);
}

/// A second client sees what the first one wrote: PD is one process with one state, and the
/// connection is not where anything is kept.
#[tokio::test]
async fn a_second_client_sees_the_first_ones_writes() {
    let cluster = Cluster::start().await;
    let first = cluster.channel().await;
    let (cluster_id, _) = first
        .bootstrap(StoreInfo::new(1, "127.0.0.1:20160"))
        .await
        .unwrap();

    // The second client is told the cluster id rather than bootstrapping, which is what a
    // client that is not a store does.
    let second = cluster.channel().await;
    second.set_cluster_id(cluster_id);
    let (region, _, _) = second
        .get_region(Bytes::from_static(b"k"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(region.id, 1);

    // Ids handed out on two connections never collide.
    let mut ids = Vec::new();
    for _ in 0..4 {
        ids.push(first.alloc_id(1).await.unwrap());
        ids.push(second.alloc_id(1).await.unwrap());
    }
    let unique: std::collections::BTreeSet<u64> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len(), "two connections shared an id");
}

/// The two typed refusals, over the wire rather than in process: an error is only useful if it
/// survives the encoding.
#[tokio::test]
async fn the_refusals_arrive_as_themselves() {
    let cluster = Cluster::start().await;
    let pd = cluster.channel().await;

    // Before anything has bootstrapped.
    let error = pd.get_region(&b"k"[..]).await.unwrap_err();
    assert!(
        matches!(error, ProtoError::NotBootstrapped),
        "got {error:?} instead of NotBootstrapped"
    );
    assert!(
        !error.is_retryable(),
        "waiting for a cluster is not a retry"
    );

    let (cluster_id, _) = pd.bootstrap(StoreInfo::new(1, "a:1")).await.unwrap();

    // And a client pointed at the wrong cluster.
    pd.set_cluster_id(cluster_id ^ 1);
    let error = pd.tso(1).await.unwrap_err();
    match error {
        ProtoError::ClusterMismatch { expected, actual } => {
            assert_eq!(expected, cluster_id);
            assert_eq!(actual, cluster_id ^ 1);
        }
        other => panic!("got {other:?} instead of ClusterMismatch"),
    }
}

/// A stale heartbeat still gets an operator: PD schedules against the record it *holds*, not
/// against the beat it was sent. A leader whose beat crossed a newer one on the network is
/// still the leader that has to do the work.
#[tokio::test]
async fn a_stale_heartbeat_still_carries_the_repair() {
    let cluster = Cluster::start().await;
    let pd = cluster.channel().await;
    let (_, region) = pd
        .bootstrap(StoreInfo::new(1, "127.0.0.1:1"))
        .await
        .unwrap();
    let region = region.unwrap();
    for store_id in 2..=4 {
        pd.bootstrap(StoreInfo::new(store_id, format!("127.0.0.1:{store_id}")))
            .await
            .unwrap();
    }
    let peers = vec![
        Peer::voter(1, region.peers[0].peer_id),
        Peer::voter(2, 20),
        Peer::voter(3, 30),
    ];
    let current = Region {
        peers: peers.clone(),
        epoch: Epoch::new(4, 1),
        ..region.clone()
    };
    pd.region_heartbeat(current, 10, 9, 0, 0).await.unwrap();

    cluster
        .clock
        .advance(esker_pd::pd::MAX_STORE_DOWN_TIME_MS + 1);
    for store_id in [1, 2, 4] {
        pd.store_heartbeat(store_id, 0, 0, 0, 0, 0).await.unwrap();
    }

    // A beat from an older term at an older epoch: dropped from the table, answered anyway.
    let behind = Region {
        peers,
        epoch: Epoch::new(3, 1),
        ..region
    };
    let operator = pd
        .region_heartbeat(behind, 10, 8, 0, 0)
        .await
        .unwrap()
        .expect("the repair the region needs");
    assert_eq!(
        operator.epoch(),
        Epoch::new(4, 1),
        "the operator is addressed to the epoch PD holds, not the one the beat carried"
    );
}

/// A heartbeat for an epoch PD has already moved past is dropped, and the answer is the same
/// as for one that was applied — the sender has nothing to do differently.
#[tokio::test]
async fn a_stale_heartbeat_is_accepted_on_the_wire_and_dropped_in_the_table() {
    let cluster = Cluster::start().await;
    let pd = cluster.channel().await;
    let (_, region) = pd.bootstrap(StoreInfo::new(1, "a:1")).await.unwrap();
    let region = region.unwrap();

    let newer = Region {
        epoch: Epoch::new(1, 5),
        peers: vec![Peer::voter(1, region.peers[0].peer_id)],
        ..region.clone()
    };
    pd.region_heartbeat(newer, 10, 4, 0, 0).await.unwrap();
    // The pre-split epoch, arriving late.
    pd.region_heartbeat(region, 99, 4, 0, 0).await.unwrap();

    let held = cluster.pd.regions().unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].region.epoch, Epoch::new(1, 5));
    assert_eq!(held[0].leader_peer_id, 10, "the stale beat won");
}

/// 4c over the wire: a store goes quiet, and the next heartbeat from a surviving leader comes
/// back carrying the repair. The whole point of putting the operator on the heartbeat response
/// is that no new call and no new connection is needed for it, and that is what this checks.
#[tokio::test]
async fn a_dead_store_earns_a_repair_on_the_heartbeat_response() {
    let cluster = Cluster::start().await;
    let pd = cluster.channel().await;

    let (_, region) = pd
        .bootstrap(StoreInfo::new(1, "127.0.0.1:1"))
        .await
        .unwrap();
    let region = region.unwrap();
    for store_id in 2..=4 {
        pd.bootstrap(StoreInfo::new(store_id, format!("127.0.0.1:{store_id}")))
            .await
            .unwrap();
    }

    // Three replicas, all live.
    let three = Region {
        peers: vec![
            Peer::voter(1, region.peers[0].peer_id),
            Peer::voter(2, 20),
            Peer::voter(3, 30),
        ],
        ..region
    };
    assert_eq!(
        pd.region_heartbeat(three.clone(), 10, 4, 0, 0)
            .await
            .unwrap(),
        None,
        "a healthy region is left alone"
    );

    // Store 3 stops beating; the others carry on.
    cluster
        .clock
        .advance(esker_pd::pd::MAX_STORE_DOWN_TIME_MS + 1);
    for store_id in [1, 2, 4] {
        pd.store_heartbeat(store_id, 1 << 40, 1 << 39, 1, 0, 0)
            .await
            .unwrap();
    }

    let operator = pd
        .region_heartbeat(three.clone(), 10, 4, 0, 0)
        .await
        .unwrap()
        .expect("a repair on the heartbeat response");
    match operator {
        Operator::AddPeer {
            region_id,
            store_id,
            epoch,
            ..
        } => {
            assert_eq!(region_id, three.id);
            assert_eq!(store_id, 4, "the only live store without a peer");
            assert_eq!(epoch, three.epoch);
        }
        other => panic!("expected an AddPeer, got {other:?}"),
    }

    // The same operator until something changes — never a second one.
    let again = pd
        .region_heartbeat(three, 10, 4, 0, 0)
        .await
        .unwrap()
        .expect("still asking");
    assert_eq!(again, operator);
    assert_eq!(cluster.pd.in_flight().unwrap().len(), 1);
}

/// PD is not a store. A client that reached it by mistake is told so, rather than being
/// answered with something that looks like data.
#[tokio::test]
async fn a_key_value_request_is_refused_over_the_wire() {
    let cluster = Cluster::start().await;
    let transport = TcpTransport::connect(cluster.handle.local_addr())
        .await
        .unwrap();
    let error = transport
        .call(esker_proto::Request::raw_kv(
            esker_proto::RequestHeader::default(),
            esker_proto::RawKvReq::get(&b"k"[..]),
        ))
        .await
        .unwrap_err();
    assert!(matches!(error, ProtoError::InvalidRequest { .. }));
}

/// The state is on disk, so a PD that is restarted underneath a client is the same cluster.
#[tokio::test]
async fn the_cluster_survives_a_restart_of_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let options = || PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>);

    let (cluster_id, highest_ts, highest_id) = {
        let pd = Pd::open(dir.path(), options()).unwrap();
        let handle = Server::bind("127.0.0.1:0", PdService::new(pd), TransportConfig::new())
            .await
            .unwrap()
            .spawn()
            .unwrap();
        let transport = TcpTransport::connect(handle.local_addr()).await.unwrap();
        let channel = PdChannel::new(Arc::new(transport) as Arc<dyn Transport>);
        let (cluster_id, _) = channel.bootstrap(StoreInfo::new(1, "a:1")).await.unwrap();
        let ts = channel.tso(4).await.unwrap();
        let id = channel.alloc_id(1).await.unwrap();
        handle.shutdown().await.unwrap();
        (cluster_id, ts, id)
    };

    // The clock comes back an hour behind, which must change nothing.
    clock.set(1_700_000_000_000 - 3_600_000);
    let pd = Pd::open(dir.path(), options()).unwrap();
    let handle = Server::bind("127.0.0.1:0", PdService::new(pd), TransportConfig::new())
        .await
        .unwrap()
        .spawn()
        .unwrap();
    let transport = TcpTransport::connect(handle.local_addr()).await.unwrap();
    let channel = PdChannel::new(Arc::new(transport) as Arc<dyn Transport>);

    let (again, region) = channel.bootstrap(StoreInfo::new(1, "a:1")).await.unwrap();
    assert_eq!(again, cluster_id, "the cluster id changed across a restart");
    assert_eq!(region, None, "the cluster was bootstrapped a second time");
    assert!(channel.tso(1).await.unwrap() > highest_ts);
    assert!(channel.alloc_id(1).await.unwrap() > highest_id);
}
