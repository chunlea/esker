//! A table whose rows and whose catalog live in different regions still gets a columnar copy.
//!
//! The decoder a columnar learner needs is built from a **catalog record**, and a catalog record
//! lives in the cluster's `'m'` key space. A store can read one only if it hosts the region
//! covering `'m'`. Every cluster this feature was built and tested on was single-region, where
//! every store does — so the copy worked everywhere it was looked at and existed nowhere else.
//! `crates/esker-store/src/columnar/region.rs`'s own module header names the gap and names the fix.
//!
//! This file is the two-region case, which is every real cluster: the catalog below `"t"` on one
//! store, the table's rows above it on another, and a fragment asked of the store that has the
//! rows. It fetches the record from the store that has the catalog
//! ([ADR 0037](../../../docs/adr/0037-a-columnar-learner-fetches-the-schema-it-cannot-read.md)).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use esker_engine::{WriteBatch, WriteOptions, cf};
use esker_keys::columnar::Published;
use esker_keys::value::{ColumnType, Datum};
use esker_proto::{
    Epoch, Peer, PeerRole, Region, RequestHeader, Server, ServerHandle, Service, TransportConfig,
    TxnKvReq, TxnMutation,
};
use esker_store::pd::{FakePd, PdClient};
use esker_store::{Store, StoreOptions, StoreService, meta};

const TENANT: u64 = 1;
const TABLE: u64 = 7;

/// Where the key space is cut. `'m'` is the catalog namespace and `'t'` is the SQL one, and `'m'`
/// sorts below `'t'` — so one split at `"t"` puts every catalog record on one side and every row
/// on the other, which is the shape this file needs and is also what a real cluster grows into.
const SPLIT_AT: &[u8] = b"t";

struct Node {
    store: Arc<Store>,
    handle: ServerHandle,
    _dir: tempfile::TempDir,
}

impl Node {
    async fn stop(self) {
        self.store.stop();
        let _ = self.handle.shutdown().await;
    }
}

fn reserve() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// A store hosting exactly `regions`, on a socket, with no Raft.
///
/// No Raft because this file is about where a *record* can be read from, and replication would
/// only add elections to a question that has none in it.
///
/// The region records are **written and the store reopened onto them**, rather than reached
/// through a real split. What matters here is the end state — one store holding the catalog's
/// range and another holding the rows' — and a real split would add a Raft group to a test with no
/// consensus in it. The reopen is the load-bearing half: a record on disk is not a region the map
/// serves until an open reads it, which the first version of this helper did not do and the epoch
/// check caught immediately.
async fn open(
    address: std::net::SocketAddr,
    store_id: u64,
    pd: &Arc<FakePd>,
    regions: &[Region],
) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let options = || StoreOptions {
        store_id,
        peer_id: store_id,
        region_id: store_id,
        pd: Some(Arc::clone(pd) as Arc<dyn PdClient>),
        address: address.to_string(),
        heartbeat_tick: Duration::from_millis(5),
        store_heartbeat: Duration::from_millis(20),
        region_heartbeat: Duration::from_millis(20),
        ..StoreOptions::new()
    };

    let store = Store::open(dir.path(), options()).unwrap();
    let cf_id = store.db().cf_id(cf::RAFT).unwrap();
    let mut batch = WriteBatch::new();
    for existing in store.regions().regions() {
        if !regions.iter().any(|region| region.id == existing.id) {
            meta::stage_removal(&mut batch, cf_id, existing.id);
        }
    }
    for region in regions {
        meta::stage_region(&mut batch, cf_id, region);
    }
    store.db().write(batch, &WriteOptions::synced()).unwrap();
    store.stop();
    drop(store);

    let store = Store::open(dir.path(), options()).unwrap();
    assert_eq!(
        store.regions().regions().len(),
        regions.len(),
        "store {store_id} did not come back onto the records it was given"
    );
    let server = Server::bind(
        address,
        StoreService::new(Arc::clone(&store)) as Arc<dyn Service>,
        TransportConfig::new(),
    )
    .await
    .unwrap();
    let handle = server.spawn().unwrap();
    Node {
        store,
        handle,
        _dir: dir,
    }
}

/// The catalog's half of the key space, on store 1.
fn catalog_region() -> Region {
    Region {
        id: 1,
        start_key: Bytes::new(),
        end_key: Bytes::copy_from_slice(SPLIT_AT),
        peers: vec![Peer::voter(1, 11)],
        epoch: Epoch::new(1, 2),
    }
}

/// The rows' half, on store 2, held as **columns**. The role is what `serve_fragment` checks
/// before it will answer a fragment at all.
fn rows_region() -> Region {
    Region {
        id: 2,
        start_key: Bytes::copy_from_slice(SPLIT_AT),
        end_key: Bytes::new(),
        peers: vec![Peer {
            store_id: 2,
            peer_id: 22,
            role: PeerRole::ColumnarLearner,
        }],
        epoch: Epoch::new(1, 2),
    }
}

/// `id int8, name text`, which is what the rows below encode.
fn published() -> Vec<u8> {
    esker_keys::columnar::encode(
        1,
        Some(&Published {
            schema_version: 1,
            columns: vec![(ColumnType::Int8, None), (ColumnType::Text, None)],
        }),
    )
    .unwrap()
}

fn row_key(id: i64) -> Bytes {
    Bytes::from(esker_keys::row::row_key(TENANT, TABLE, &[Datum::Int8(id)]).unwrap())
}

fn row(id: i64, name: &str) -> Bytes {
    Bytes::from(
        esker_keys::row::encode_row(
            &[ColumnType::Int8, ColumnType::Text],
            &[Datum::Int8(id), Datum::Text(name.into())],
        )
        .unwrap(),
    )
}

/// Commits one transaction through the store's own transactional path, as a client would.
async fn commit(
    store: &Arc<Store>,
    region: &Region,
    start_ts: u64,
    commit_ts: u64,
    mutations: Vec<TxnMutation>,
) {
    let keys: Vec<Bytes> = mutations.iter().map(|m| m.key().clone()).collect();
    let header = RequestHeader::new(region.id, region.epoch, 0);
    store
        .serve_txn(
            header,
            TxnKvReq::Prewrite {
                start_ts,
                primary: keys[0].clone(),
                ttl_ms: 60_000,
                mutations,
            },
        )
        .await
        .expect("the prewrite was refused");
    store
        .serve_txn(
            header,
            TxnKvReq::Commit {
                start_ts,
                commit_ts,
                keys,
            },
        )
        .await
        .expect("the commit was refused");
}

/// A fragment that reads both columns of every row.
fn fragment() -> esker_proto::fragment::FragmentReq {
    let fragment = esker_columnar::Fragment::scan(
        esker_columnar::TableRef {
            tenant: TENANT,
            table_id: TABLE,
        },
        vec![0, 1],
    );
    esker_proto::fragment::FragmentReq {
        fragment: Bytes::from(esker_columnar::fragment::encode(&fragment)),
        ts: u64::MAX,
        min_apply_index: 0,
    }
}

/// **The unit's assertion.** The store holding the rows answers a fragment for a table whose
/// catalog record it cannot read, because it fetches the record from the store that can.
///
/// Before this, `serve_fragment` answered `NotColumnar` — a refusal that tells the planner *this
/// replica does not have a copy, read the rows instead* — for ever, on every table of every
/// multi-region cluster. Which is the worst shape a missing feature can take: not an error, not a
/// wrong answer, just a fallback that never stops falling back.
#[tokio::test(flavor = "multi_thread")]
async fn a_learner_without_the_catalog_fetches_the_schema_and_answers() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();

    let pd = Arc::new(FakePd::new());
    let catalog_address = reserve();
    let rows_address = reserve();
    // The two halves, one store each.
    let catalog = open(catalog_address, 1, &pd, &[catalog_region()]).await;
    let rows = open(rows_address, 2, &pd, &[rows_region()]).await;

    // The catalog record, on the store that owns the catalog's range and nowhere else.
    commit(
        &catalog.store,
        &catalog_region(),
        10,
        11,
        vec![TxnMutation::Put {
            key: Bytes::from(esker_keys::columnar::key(TENANT, TABLE)),
            value: Bytes::from(published()),
        }],
    )
    .await;
    assert!(
        esker_store::columnar::region::published_record(rows.store.db(), TENANT, TABLE)
            .unwrap()
            .is_none(),
        "the store holding the rows can read the catalog record, so this is not the case under test"
    );

    // The rows, on the store that owns theirs.
    for (id, name) in [(1_i64, "one"), (2, "two"), (3, "three")] {
        commit(
            &rows.store,
            &rows_region(),
            20 + u64::try_from(id).unwrap() * 2,
            21 + u64::try_from(id).unwrap() * 2,
            vec![TxnMutation::Put {
                key: row_key(id),
                value: row(id, name),
            }],
        )
        .await;
    }

    // PD told where each half lives, last, so a heartbeat cannot have overwritten it. This is
    // what makes `get_region(<the record's key>)` answer "region 1, on store 1", which is the one
    // thing the fetching store cannot work out for itself.
    pd.place(catalog_region());
    pd.place(rows_region());

    // Over the wire, through the service, because that is the only way a fragment ever arrives
    // and because the fetch this test is about is itself a wire call.
    let connection = esker_proto::TcpTransport::connect(rows_address)
        .await
        .unwrap();
    let reply = esker_proto::Transport::call(
        &connection,
        esker_proto::Request::Fragment {
            header: RequestHeader::new(2, rows_region().epoch, 0),
            request: fragment(),
        },
    )
    .await
    .expect("the fragment was an error frame rather than an answer");
    let esker_proto::Response::Fragment(answer) = reply else {
        panic!("a fragment was answered with something else");
    };

    match answer {
        esker_proto::fragment::FragmentResp::Result { result, .. } => {
            let body = esker_proto::fragment::result::decode(&result).unwrap();
            let esker_proto::fragment::result::Body::Rows { rows: got, .. } = body else {
                panic!("a projection answered with groups");
            };
            assert_eq!(got.len(), 3, "the copy answered {} of 3 rows", got.len());
        }
        esker_proto::fragment::FragmentResp::Refused { reason, detail } => {
            panic!(
                "the learner refused a table whose schema it could have fetched: {reason:?}: \
                 {detail}"
            );
        }
    }

    rows.stop().await;
    catalog.stop().await;
}
