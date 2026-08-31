//! The SQL node: listen, and speak PostgreSQL.
//!
//! One process, one listener, one executor per connection over a store shared by all of them.
//! Until phase 5's client is wired in, that store is the in-memory transactional fake
//! (`esker_sql::backend::MemoryBackend`) — real MVCC with real write-write conflict detection, but
//! only in this process and only until it exits. `TODO(phase-6a)`: the real `TxnClient`.
//!
//! Everything the executor cannot run is answered `0A000 feature_not_supported` naming the
//! construct, which is contract C2 working as intended rather than a placeholder: a client
//! connects, gets a prompt, and is told the truth about what this node can do — never a crash,
//! never a syntax error about valid SQL, and never a wrong answer.

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
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

    let address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5432".to_owned());
    let config = Config {
        address,
        auth: Auth::Trust,
        ..Config::default()
    };
    let sessions = Sessions {
        backend: Arc::new(MemoryBackend::new()),
        catalog: Arc::new(Catalog::new()),
    };
    serve(config, Arc::new(sessions)).await
}
