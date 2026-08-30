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
