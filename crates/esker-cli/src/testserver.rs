//! A real store on a real socket, for the tests of the two commands that need one.
//!
//! `raw` and `bench --remote` exist to drive the network API. Testing them against a mock
//! would leave out the framing, the request id, the epoch header and the store's `'r'`
//! namespace — which is to say, everything they are for. So the tests open an actual
//! `esker-store` on a temporary directory and serve it on a port the operating system picks.
//!
//! This is `#[cfg(test)]`: the shipped binary contains no server of its own, and gets one when
//! `esker-cli server` lands.

use std::net::SocketAddr;
use std::sync::Arc;

use esker_proto::transport::{Server, ServerHandle, Service, TransportConfig};
use esker_store::{Store, StoreOptions, StoreService};

/// A store, a server and a runtime, alive for as long as this value is.
///
/// The field order is the shutdown order: the handle first, which stops the server, then the
/// runtime. Reversing them would drop a runtime with live tasks still on it.
#[derive(Debug)]
pub(crate) struct TestServer {
    addr: SocketAddr,
    _handle: ServerHandle,
    _runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

impl TestServer {
    /// Opens a store on a temporary directory and serves it on a free port.
    pub(crate) fn start() -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime");

        let store = Store::open(dir.path(), StoreOptions::new()).expect("the store opens");
        let service: Arc<dyn Service> = StoreService::new(store);

        let handle = runtime.block_on(async {
            Server::bind("127.0.0.1:0", service, TransportConfig::new())
                .await
                .expect("the server binds")
                .spawn()
                .expect("the server starts")
        });

        Self {
            addr: handle.local_addr(),
            _handle: handle,
            _runtime: runtime,
            _dir: dir,
        }
    }

    /// Where it is listening, as `--addr` would be given it.
    pub(crate) fn addr(&self) -> String {
        self.addr.to_string()
    }
}

/// Wraps a service and counts the requests that reach it, per method.
///
/// The observable behind "one round trip instead of sixty". A round-trip count is not visible in
/// anything a command prints, and a claim about it that no test can see is a claim that stops
/// being true the moment somebody reintroduces a loop.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn Service>,
    calls: Arc<std::sync::Mutex<std::collections::BTreeMap<&'static str, usize>>>,
}

impl Service for Counting {
    fn call(
        &self,
        request: esker_proto::Request,
    ) -> esker_proto::BoxFuture<'_, Result<esker_proto::transport::Reply, esker_proto::ProtoError>>
    {
        if let Ok(mut calls) = self.calls.lock() {
            *calls.entry(request.method().name()).or_default() += 1;
        }
        self.inner.call(request)
    }

    fn store_id(&self) -> u64 {
        self.inner.store_id()
    }
}

/// A real placement driver on a real socket, already bootstrapped.
///
/// Bootstrapped on purpose, and it is the whole point: a fresh PD has no cluster id, and a
/// bootstrapped one has a random non-zero one that every later call has to carry. A tool tested
/// only against an empty PD would never find out that it was addressing cluster `0`.
#[derive(Debug)]
pub(crate) struct TestPd {
    addr: SocketAddr,
    cluster_id: u64,
    pd: Arc<esker_pd::Pd>,
    calls: Arc<std::sync::Mutex<std::collections::BTreeMap<&'static str, usize>>>,
    _handle: ServerHandle,
    _runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

impl TestPd {
    /// Opens a placement driver on a temporary directory, registers `stores` of them, and serves
    /// it on a free port.
    pub(crate) fn start(stores: u64) -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime");

        let pd = esker_pd::Pd::open(dir.path(), esker_pd::PdOptions::new()).expect("PD opens");
        let mut cluster_id = 0;
        for store_id in 1..=stores {
            cluster_id = pd
                .bootstrap(store_id, &format!("127.0.0.1:{}", 20_160 + store_id))
                .expect("the store registers")
                .cluster_id;
        }
        let calls = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new()));
        let service: Arc<dyn Service> = Arc::new(Counting {
            inner: esker_pd::PdService::new(Arc::clone(&pd)),
            calls: Arc::clone(&calls),
        });

        let handle = runtime.block_on(async {
            Server::bind("127.0.0.1:0", service, TransportConfig::new())
                .await
                .expect("the server binds")
                .spawn()
                .expect("the server starts")
        });

        Self {
            addr: handle.local_addr(),
            cluster_id,
            pd,
            calls,
            _handle: handle,
            _runtime: runtime,
            _dir: dir,
        }
    }

    /// Where it is listening, as `--pd` would be given it.
    pub(crate) fn addr(&self) -> String {
        self.addr.to_string()
    }

    /// How many requests of each method have reached it.
    pub(crate) fn calls(&self, method: &str) -> usize {
        self.calls
            .lock()
            .map_or(0, |calls| calls.get(method).copied().unwrap_or(0))
    }

    /// The driver itself, so a test can put regions into its table without a store.
    pub(crate) fn pd(&self) -> &Arc<esker_pd::Pd> {
        &self.pd
    }

    /// The cluster it created, which is not zero.
    pub(crate) fn cluster_id(&self) -> u64 {
        self.cluster_id
    }
}
