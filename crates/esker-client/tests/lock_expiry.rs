//! **A lock whose owner vanished is resolved once its lease runs out — and only if the oracle has a
//! physical half** (debt #84, [ADR 0116](../../../docs/adr/0116-a-single-node-oracle-needs-a-physical-half.md)).
//!
//! Percolator's way out of an orphaned lock is its lease: `is_expired` compares `ttl_ms` against the
//! **physical** half of the timestamps, so that no node judges another's transaction by its own
//! clock. `CountingOracle` hands out consecutive integers, whose physical half (`ts >> 18`) is zero
//! for its first 262,144 timestamps — so under it the lease never runs out and the row stays blocked
//! for ever. The single-node `esker-sql` binary shipped exactly that pairing until unit Q.
//!
//! Both halves are here, against a real store over a real socket, because the lease is settled by a
//! round trip (`settle_primary`) and an in-process fake would not take it:
//!
//! * with [`esker_client::WallClockOracle`], the stranded lock is resolved and the next transaction
//!   takes the key;
//! * with [`esker_client::CountingOracle`], it is not — however long the waiter is willing to wait.
//!   That case is the defect this file exists for, kept as a **documented** property rather than a
//!   comment, so that wiring a counter back into a single-node node fails here.
//!
//! **A transaction dropped without `commit` leaves its lock behind on purpose** — `Drop` "only
//! forgets" the renewal, and the locks are left to the lease and the resolver, which is what happens
//! to any abandoned transaction. That is how a client that died is spelled in a test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use esker_client::region_cache::StaticRegion;
use esker_client::router::Router;
use esker_client::{CountingOracle, Error, TcpStores, TimestampOracle, TxnClient, WallClockOracle};

/// The region a freshly bootstrapped store serves, as every other client test assumes.
const BOOTSTRAP_REGION: u64 = 1;

/// Short enough that a test can outlive it, long enough that a loaded box does not cross it between
/// two adjacent statements.
const TTL_MS: u64 = 50;

/// What the waiter gives the lease before deciding it should have expired. A readiness wait, so its
/// bound is sized for the worst machine rather than for this one.
const WAIT_MS: u64 = 400;

struct TestServer {
    addr: SocketAddr,
    _handle: esker_proto::transport::ServerHandle,
    _runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

/// A store on a temporary directory, served on a port the OS picks.
fn start_server() -> TestServer {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");

    let store = esker_store::Store::open(dir.path(), esker_store::StoreOptions::new())
        .expect("the store opens");
    let service: Arc<dyn esker_proto::transport::Service> = esker_store::StoreService::new(store);

    let handle = runtime.block_on(async {
        esker_proto::transport::Server::bind(
            "127.0.0.1:0",
            service,
            esker_proto::transport::TransportConfig::new(),
        )
        .await
        .expect("the server binds")
        .spawn()
        .expect("the server starts")
    });

    TestServer {
        addr: handle.local_addr(),
        _handle: handle,
        _runtime: runtime,
        _dir: dir,
    }
}

/// **The pairing the single-node binary builds**: a static one-region table and a local oracle, with
/// a lease short enough to watch.
fn single_node_client(server: &TestServer, oracle: Arc<dyn TimestampOracle>) -> TxnClient {
    let stores = TcpStores::connect(server.addr).expect("the client connects");
    let store_id = stores.only_store().expect("the server named its store");
    let router = Router::new(
        Arc::new(stores),
        Arc::new(StaticRegion::whole_key_space(BOOTSTRAP_REGION, store_id, 0)),
    );
    TxnClient::on_router(Arc::new(router), oracle).with_lock_ttl_ms(TTL_MS)
}

/// Takes an eager lock on `key` and abandons the transaction, which leaves the lock on the store.
fn strand_a_lock(client: &TxnClient, key: &[u8]) {
    let mut txn = client.begin().expect("a transaction begins");
    txn.lock(key).expect("the lock is taken");
    // Dropped, not committed and not rolled back: `Drop` only forgets the renewal, so what is left
    // behind is a real lock with a real lease and nobody coming back for it.
    drop(txn);
}

/// Whether a second transaction can have the key, after giving the lease time to run out.
fn key_is_free_after_the_lease(client: &TxnClient, key: &[u8]) -> Result<bool, Error> {
    std::thread::sleep(Duration::from_millis(WAIT_MS));
    let mut txn = client.begin()?;
    match txn.lock(key) {
        Ok(esker_client::Acquired::Taken) => {
            txn.rollback()?;
            Ok(true)
        }
        Ok(esker_client::Acquired::Held { .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

/// **With a physical half, the lease runs out and the row comes back.**
#[test]
fn a_stranded_lock_is_resolved_once_its_lease_runs_out() {
    let server = start_server();
    let key = b"expiry/with-a-physical-half";

    let client = single_node_client(&server, Arc::new(WallClockOracle::new()));
    strand_a_lock(&client, key);

    let free =
        key_is_free_after_the_lease(&client, key).expect("the waiter asked and was answered");
    assert!(
        free,
        "the lease ran out {WAIT_MS} ms ago and the row is still held"
    );
}

/// **Without one, it does not** — `physical_ms` of every timestamp a counter mints is zero, so the
/// lease never runs out. This is #84 as a property rather than as a sentence: a single-node node
/// wired back to a counter fails here.
#[test]
fn a_counter_never_lets_a_stranded_lock_expire() {
    let server = start_server();
    let key = b"expiry/with-a-counter";

    let client = single_node_client(&server, Arc::new(CountingOracle::starting_at(1)));
    strand_a_lock(&client, key);

    let free = key_is_free_after_the_lease(&client, key);
    match free {
        // Held, or the waiter spending its whole budget against a lock that cannot die — the same
        // defect, the second wearing the error it produced in the field: `a lock … could not be
        // cleared`.
        Ok(false) | Err(Error::LockNotCleared { .. }) => {}
        other => panic!("a counter's lease cannot run out, and this says it did: {other:?}"),
    }
}

/// The oracle's own contract, which is what the two tests above rest on.
#[test]
fn a_wall_clock_oracle_mints_a_physical_half_and_never_goes_backwards() {
    let oracle = WallClockOracle::new();

    let first = oracle.tso(1).expect("a timestamp");
    let second = oracle.tso(1).expect("another");
    assert!(second > first, "{second} did not follow {first}");
    assert!(
        esker_client::physical_ms(first) > 0,
        "a timestamp with no physical half is what #84 is about: {first}"
    );

    // A lease minted now is over a TTL later, which is the whole of what `is_expired` needs and
    // exactly what a counter cannot give it.
    let later = esker_client::ts_at_ms(esker_client::physical_ms(first) + TTL_MS + 1);
    assert!(esker_client::is_expired(first, TTL_MS, later));
    assert!(!esker_client::is_expired(first, TTL_MS, second));

    // The counter's half of this arithmetic is **not** restated here: it is already pinned by
    // `esker-sql`'s `statement_across_a_leader_kill.rs::a_lock_this_fixture_mints_can_run_out_of_lease`,
    // whose own doc says that assertion is the one to read if `CountingOracle` ever grows a clock.
    // What this file adds is the promoted oracle's contract and the end-to-end behaviour over a real
    // store, which #84's row records as having no test at all.
}
