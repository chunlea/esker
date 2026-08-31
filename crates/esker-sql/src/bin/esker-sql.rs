//! The SQL node: listen, and speak PostgreSQL.
//!
//! One process, one listener, one executor per connection over a store shared by all of them.
//!
//! The store is a real cluster when store addresses are given and the in-memory transactional fake
//! otherwise. The fake is real MVCC with real write-write conflict detection and is still only in
//! this process, which makes it right for a demonstration and wrong for anything else — so the
//! node says which one it opened, in a line an operator will see before they wonder.
//!
//! Everything the executor cannot run is answered `0A000 feature_not_supported` naming the
//! construct, which is contract C2 working as intended rather than a placeholder: a client
//! connects, gets a prompt, and is told the truth about what this node can do — never a crash,
//! never a syntax error about valid SQL, and never a wrong answer.

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend, StoreBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::pgwire::server::{Auth, Config, Executors, serve};
use esker_sql::pgwire::session::Execute;

/// The tenant every connection is served as, until there is a way to say otherwise.
const TENANT: u64 = 1;

/// The store and the catalog cache, shared; one [`Executor`] per session over them.
struct Sessions {
    backend: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
}

impl Executors for Sessions {
    fn for_session(&self) -> Box<dyn Execute + Send> {
        Box::new(Executor::new(
            Arc::clone(&self.backend),
            Arc::clone(&self.catalog),
            TENANT,
        ))
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // `esker-sql [listen] [store...]`. With no stores the node runs on the in-process fake.
    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "127.0.0.1:5432".to_owned());
    let stores: Vec<String> = args.collect();
    let config = Config {
        address,
        auth: Auth::Trust,
        ..Config::default()
    };
    let backend: Arc<dyn Backend> = if stores.is_empty() {
        tracing::warn!(
            "no store addresses given: running on the in-process fake, which keeps nothing"
        );
        Arc::new(MemoryBackend::new())
    } else {
        tracing::info!(stores = ?stores, "connecting to the cluster");
        Arc::new(StoreBackend::new(Arc::new(connect(&stores)?)))
    };
    let sessions = Sessions {
        backend,
        catalog: Arc::new(Catalog::new()),
    };
    serve(config, Arc::new(sessions)).await
}

/// Builds a client over the given stores, with routing that asks them where the regions are.
///
/// `TODO(phase-6a)`: the routing table comes from PD once this node speaks to it; until then a
/// node started against real stores is told about them on the command line.
fn connect(stores: &[String]) -> std::io::Result<esker_client::TxnClient> {
    use esker_proto::transport::TransportConfig;

    let addresses: Vec<std::net::SocketAddr> = stores
        .iter()
        .map(|address| {
            address.parse().map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("{address} is not a store address: {error}"),
                )
            })
        })
        .collect::<std::io::Result<_>>()?;
    let transport = esker_client::TcpStores::connect_all(&addresses, TransportConfig::default())
        .map_err(std::io::Error::other)?;
    // One region over every store given, which is what a cluster bootstraps with and what a
    // redirect needs to be able to follow. `TODO(phase-6a)`: the real routing table comes from
    // PD, and then a node is told where PD is rather than where the stores are.
    let store_ids = transport.store_ids();
    let resolver = Arc::new(esker_client::StaticRegion::replicated(1, &store_ids));
    let router = esker_client::Router::new(Arc::new(transport), resolver);
    Ok(esker_client::TxnClient::on_router(
        Arc::new(router),
        Arc::new(esker_client::CountingOracle::starting_at(1)),
    ))
}
