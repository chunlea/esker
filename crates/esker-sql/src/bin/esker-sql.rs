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
use esker_sql::exec::redrive::ReDriver;
use esker_sql::pd::{LeaseRefresher, PdConn, PdLease};
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

    // `esker-sql [--pd HOST:PORT] [listen] [store...]`. With no stores the node runs on the
    // in-process fake.
    let Args {
        address,
        stores,
        pd,
    } = Args::parse(std::env::args().skip(1))?;
    let config = Config {
        address,
        auth: Auth::Trust,
        ..Config::default()
    };
    // The lease this node holds, or nothing at all. **Absent `--pd` changes nothing**: no lease
    // source, so `Backend::schema_lease_remaining` answers "unbounded" and every write is
    // unrestricted, exactly as it was before this flag existed.
    let lease = pd.as_ref().map(|_| Arc::new(PdLease::new()));
    let backend: Arc<dyn Backend> = if stores.is_empty() {
        if pd.is_some() {
            // The fake keeps nothing and is in this process; a lease from a real placement driver
            // over it would be a safety property with nothing behind it, and a columnar report
            // would name ranges no store holds. Refused rather than half-wired.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--pd needs a cluster: give this node the store addresses to connect to",
            ));
        }
        tracing::warn!(
            "no store addresses given: running on the in-process fake, which keeps nothing"
        );
        Arc::new(MemoryBackend::new())
    } else {
        tracing::info!(stores = ?stores, "connecting to the cluster");
        {
            let (client, oracle) = connect(&stores)?;
            let backend = StoreBackend::new(Arc::new(client), oracle);
            match &lease {
                Some(lease) => Arc::new(backend.with_schema_lease(Arc::clone(lease) as Arc<_>)),
                None => Arc::new(backend),
            }
        }
    };
    let catalog = Arc::new(Catalog::new());

    // The one cable. With it, two things that ship inert come alive: the lease arms fail-closed,
    // and the re-driver below is given PD's interval (`docs/plans/phase-8-learner.md` §wiring).
    // `zip` because the two are built together: a lease with no address to renew it at, or an
    // address with no lease to fill in, would both be this function having gone wrong.
    if let Some((address, lease)) = pd.zip(lease) {
        attach_pd(address, lease).await?;
    } else {
        tracing::info!(
            "no placement driver given: writes are unrestricted and no schema lease is held"
        );
    }

    // Every node runs a re-driver, so a schema change whose node died is finished by whichever
    // node notices rather than by a human calling `esker_schema_step` (ADR 0020 as amended,
    // `docs/plans/debt-c2.md`). It is a thread of its own rather than a task: it sleeps a step
    // interval between passes and would hold a runtime worker for the whole of one.
    //
    // It does nothing until this node holds a schema lease, because the lease is what carries
    // PD's step interval and a node that cannot be told the wait must not invent one. With `--pd`
    // it has one by the time this runs, so the interval below is PD's; without one it starts,
    // finds no interval, and waits — which is a node with no placement driver and no staged
    // schema change to be behind on, not a node that has lost anything.
    let redriver = ReDriver::new(Arc::clone(&backend), Arc::clone(&catalog), TENANT);
    if let Some(interval) = redriver.interval() {
        tracing::info!(
            step_ms = interval.step_ms,
            removal_extra_ms = interval.removal_extra_ms,
            "re-driving orphaned schema-change jobs"
        );
    } else {
        tracing::info!(
            "no schema step interval published to this node: orphaned schema-change jobs will \
             wait for esker_schema_step"
        );
    }
    std::thread::Builder::new()
        .name("schema-redriver".to_owned())
        .spawn(move || redriver.run())?;

    let sessions = Sessions { backend, catalog };
    serve(config, Arc::new(sessions)).await
}

/// What this node was told on its command line.
///
/// Hand-parsed, like every other argument list in this project. The positional form is unchanged —
/// `esker-sql [listen] [store...]` — and `--pd` may appear anywhere among them.
#[derive(Debug)]
struct Args {
    /// Where to listen for clients.
    address: String,
    /// The stores to connect to; empty runs the in-process fake.
    stores: Vec<String>,
    /// The placement driver, or `None`.
    ///
    /// **No default and no discovery.** A node started without `--pd` behaves exactly as it did
    /// before the flag existed, which is what makes the flag additive rather than a change of
    /// behaviour with an opt-out.
    pd: Option<std::net::SocketAddr>,
}

impl Args {
    fn parse(arguments: impl Iterator<Item = String>) -> std::io::Result<Self> {
        let invalid =
            |message: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
        let mut positional = Vec::new();
        let mut pd = None;
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            let raw = if let Some(value) = argument.strip_prefix("--pd=") {
                value.to_owned()
            } else if argument == "--pd" {
                arguments
                    .next()
                    .ok_or_else(|| invalid("--pd needs an address".to_owned()))?
            } else {
                positional.push(argument);
                continue;
            };
            pd = Some(raw.parse().map_err(|error| {
                invalid(format!("{raw} is not a placement-driver address: {error}"))
            })?);
        }
        let mut positional = positional.into_iter();
        Ok(Self {
            address: positional
                .next()
                .unwrap_or_else(|| "127.0.0.1:5432".to_owned()),
            stores: positional.collect(),
            pd,
        })
    }
}

/// Fetches the first lease and starts the refresher thread.
///
/// **The lease is fetched before this node serves anything.** A node that cannot reach PD at
/// startup does not come up holding a lease it never had; it fails, the way a store that cannot
/// reach PD fails to open (`esker_store::RemotePd`).
async fn attach_pd(address: std::net::SocketAddr, lease: Arc<PdLease>) -> std::io::Result<()> {
    let conn = Arc::new(PdConn::new(address));
    let refresher = LeaseRefresher::new(conn, lease);
    // Onto a blocking thread and back, because this function is inside `#[tokio::main]`'s
    // `block_on`: a synchronous client refuses a thread that is *driving* a runtime, and
    // `spawn_blocking` is the seam for exactly that — the same one every statement takes
    // (`crate::pgwire::server`).
    let (refresher, held) = tokio::task::spawn_blocking(move || {
        let held = refresher.refresh();
        (refresher, held)
    })
    .await
    .map_err(std::io::Error::other)?;
    let held = held.map_err(|error| {
        std::io::Error::other(format!(
            "fetching the schema lease from the placement driver at {address}: {error}"
        ))
    })?;
    tracing::info!(
        pd = %address,
        lease_ms = held.lease_ms,
        step_ms = held.step.step_ms,
        removal_extra_ms = held.step.removal_extra_ms,
        "holding a schema lease"
    );
    // A thread rather than a task, and for the reason the re-driver beside it gives: a refresh
    // sleeps a period between passes and would hold a runtime worker for the whole of one.
    std::thread::Builder::new()
        .name("schema-lease".to_owned())
        .spawn(move || refresher.run())?;
    Ok(())
}

/// Builds a client over the given stores, with routing that asks them where the regions are.
///
/// `TODO(phase-6a)`: the routing table comes from PD once this node speaks to it; until then a
/// node started against real stores is told about them on the command line.
/// The oracle comes back beside the client because the backend needs a timestamp of its own — the
/// bound on a historical read is "not in the future", and the future is what the oracle says it is
/// (`CLAUDE.md` invariant 6).
fn connect(
    stores: &[String],
) -> std::io::Result<(
    esker_client::TxnClient,
    Arc<dyn esker_client::TimestampOracle>,
)> {
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
    let oracle: Arc<dyn esker_client::TimestampOracle> =
        Arc::new(esker_client::CountingOracle::starting_at(1));
    Ok((
        esker_client::TxnClient::on_router(Arc::new(router), Arc::clone(&oracle)),
        oracle,
    ))
}
