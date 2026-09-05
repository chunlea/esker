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
use esker_sql::fragment::{ClientFragments, FragmentSource};
use esker_sql::pd::{ColumnarReport, LeaseRefresher, PdConn, PdLease};
use esker_sql::pgwire::server::{Auth, Config, Executors, serve};
use esker_sql::pgwire::session::Execute;
use esker_sql::pgwire::tls::TlsConfig;

/// The tenant every connection is served as, until there is a way to say otherwise.
const TENANT: u64 = 1;

/// The store and the catalog cache, shared; one [`Executor`] per session over them.
struct Sessions {
    backend: Arc<dyn Backend>,
    catalog: Arc<Catalog>,
    /// This node's advisory locks, shared by every session it serves.
    ///
    /// Beside the catalog cache because it has the same lifetime and the same scope: node-wide,
    /// in memory, gone when the process is. A session that took one and never released it loses it
    /// when its connection closes, which is what a real server does too (`esker_sql::advisory`).
    locks: Arc<esker_sql::advisory::Locks>,
    /// The node's reserved sequence blocks, shared by every session it serves (ADR 0072).
    sequences: Arc<esker_sql::sequence::Blocks>,
    /// Where an `ALTER ... SET (columnar_replicas = N)` reports to, on a node that has a PD.
    columnar: Option<Arc<dyn ColumnarReport>>,
    /// Where a plan fragment goes, on a node that can send one (ADR 0022 milestone 4).
    ///
    /// `None` without `--pd`, and that is not a degraded node: routing needs to know which peer of
    /// a region is the columnar learner, and only the placement driver can say. A node without one
    /// plans every query on rows and `EXPLAIN` says why.
    fragments: Option<Arc<dyn FragmentSource>>,
}

impl Executors for Sessions {
    fn for_session(
        &self,
        database: &str,
        identity: esker_sql::session::Backend,
    ) -> esker_sql::Result<Box<dyn Execute + Send>> {
        // **The directory decides the tenant**, and it is read once per connection rather than
        // per statement: the answer cannot change under a session, because dropping the database
        // it is serving is `55006` (ADR 0052).
        let txn = self.backend.begin()?;
        let tenant = esker_sql::catalog::database_id(&*txn, database)?
            .ok_or_else(|| esker_sql::SqlError::UndefinedDatabase(database.to_owned()))?;
        let _ = txn.rollback();
        let mut executor = Executor::new(
            Arc::clone(&self.backend),
            Arc::clone(&self.catalog),
            tenant,
            identity,
        )
        .serving_database(database)
        .sharing_advisory_locks(Arc::clone(&self.locks))
        // **The node's, not this connection's** — a pooled client is the normal client,
        // and a block per connection is what made five inserts answer 1, 33, 65, 97, 129
        // (ADR 0072).
        .sharing_sequence_blocks(Arc::clone(&self.sequences));
        if let Some(report) = &self.columnar {
            executor = executor.reporting_columnar_to(Arc::clone(report));
        }
        if let Some(source) = &self.fragments {
            executor = executor.asking_fragments_of(Arc::clone(source));
        }
        Ok(Box::new(executor))
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
        tls_cert,
        tls_key,
    } = Args::parse(std::env::args().skip(1))?;
    let tls = configure_tls(tls_cert, tls_key)?;
    let config = Config {
        address,
        auth: Auth::Trust,
        tls,
        ..Config::default()
    };
    // The lease this node holds, or nothing at all. **Absent `--pd` changes nothing**: no lease
    // source, so `Backend::schema_lease_remaining` answers "unbounded" and every write is
    // unrestricted, exactly as it was before this flag existed.
    let lease = pd.as_ref().map(|_| Arc::new(PdLease::new()));
    // The router the client was built on, kept so the fragment path can share it: one region cache
    // for both, so an entry a row read warmed is warm for a fragment.
    let mut router: Option<Arc<esker_client::Router>> = None;
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
        // Onto a blocking thread, because connecting is a **synchronous** client building its own
        // runtime and this function is inside `#[tokio::main]`'s. Doing it here panicked with
        // "Cannot start a runtime from within a runtime" — on the first line of every node
        // started against real stores, which is the one path no test took until this phase
        // started one from a shell.
        let (client, oracle, built) = tokio::task::spawn_blocking(move || connect(&stores, pd))
            .await
            .map_err(std::io::Error::other)??;
        router = Some(built);
        let backend = StoreBackend::new(Arc::new(client), oracle);
        match &lease {
            Some(lease) => Arc::new(backend.with_schema_lease(Arc::clone(lease) as Arc<_>)),
            None => Arc::new(backend),
        }
    };
    let catalog = Arc::new(Catalog::new());

    // The one cable. With it, three things that ship inert come alive: the lease arms fail-closed,
    // the re-driver below is given PD's interval, and `ALTER ... SET (columnar_replicas = N)`
    // becomes placement rather than a durable record nobody reads
    // (`docs/plans/phase-8-learner.md` §wiring). `zip` because the two are built together: a lease
    // with no address to renew it at, or an address with no lease to fill in, would both be this
    // function having gone wrong.
    let columnar: Option<Arc<dyn ColumnarReport>> = if let Some((address, lease)) = pd.zip(lease) {
        Some(attach_pd(address, lease, &backend).await?)
    } else {
        tracing::info!(
            "no placement driver given: writes are unrestricted, no schema lease is held, and \
             columnar placement is not reported"
        );
        None
    };

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

    // **Both halves or neither.** A fragment goes to a peer PD named, so a node with a router and
    // no placement driver has nowhere to send one; a node with a driver and the in-process fake
    // has no cluster to send it to. Either way the planner still decides and `EXPLAIN` still says
    // what it decided — on rows, naming the reason.
    let fragments: Option<Arc<dyn FragmentSource>> = router
        .filter(|_| pd.is_some())
        .map(|router| Arc::new(ClientFragments::new(router)) as Arc<dyn FragmentSource>);
    if fragments.is_some() {
        tracing::info!("columnar routing is available: fragments go to the learners PD placed");
    }

    let sessions = Sessions {
        backend,
        catalog,
        locks: Arc::new(esker_sql::advisory::Locks::new()),
        sequences: Arc::new(esker_sql::sequence::Blocks::default()),
        columnar,
        fragments,
    };
    serve(config, Arc::new(sessions)).await
}

/// Turns `--tls-cert`/`--tls-key` into a [`TlsConfig`], or refuses to start.
///
/// **Before anything else, and fatal if it fails.** A node told to serve TLS that came up without
/// it would serve plaintext on a port an operator believes is encrypted, which is the one outcome
/// worse than refusing to start — the same rule `esker_s3::Endpoint::parse` follows for `https://`
/// (ADR 0025), applied to the surface ADR 0055 put first. Whether it *can* succeed is decided at
/// compile time by the `tls` feature; [`TlsConfig::from_pem_files`] says so in the error when it
/// cannot, rather than handing back a disabled configuration that reads like success.
fn configure_tls(
    certificate: Option<std::path::PathBuf>,
    key: Option<std::path::PathBuf>,
) -> std::io::Result<TlsConfig> {
    let invalid = |message: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
    match (certificate, key) {
        (Some(certificate), Some(key)) => {
            let tls = TlsConfig::from_pem_files(&certificate, &key)
                .map_err(|error| invalid(error.to_string()))?;
            tracing::info!(cert = %certificate.display(), "terminating TLS on the client port");
            Ok(tls)
        }
        (None, None) => Ok(TlsConfig::disabled()),
        // One without the other is a typo with a security consequence, so it is refused rather
        // than half-honoured.
        (certificate, _) => {
            let missing = if certificate.is_some() {
                "--tls-key"
            } else {
                "--tls-cert"
            };
            Err(invalid(format!(
                "TLS needs both a certificate and a key: {missing} is missing"
            )))
        }
    }
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
    /// PEM certificate chain for the client port, or `None` to terminate no TLS.
    tls_cert: Option<std::path::PathBuf>,
    /// PEM private key for [`Args::tls_cert`]. Both or neither.
    tls_key: Option<std::path::PathBuf>,
}

impl Args {
    fn parse(arguments: impl Iterator<Item = String>) -> std::io::Result<Self> {
        let invalid =
            |message: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);
        let mut positional = Vec::new();
        let mut pd = None;
        let mut tls_cert = None;
        let mut tls_key = None;
        let mut arguments = arguments.peekable();
        // Each flag takes `--flag=value` or `--flag value`, because an operator who learned one
        // form on `--pd` should not find the other is required on `--tls-cert`.
        while let Some(argument) = arguments.next() {
            // `(flag, what it needs)` for the three flags that take a value. A flag whose value is
            // the next argument consumes it; one written with `=` carries it.
            let taken = ["--pd", "--tls-cert", "--tls-key"]
                .into_iter()
                .find_map(|flag| {
                    if let Some(value) = argument.strip_prefix(&format!("{flag}=")) {
                        Some((flag, Ok(value.to_owned())))
                    } else if argument == flag {
                        Some((
                            flag,
                            arguments
                                .next()
                                .ok_or_else(|| invalid(format!("{flag} needs a value"))),
                        ))
                    } else {
                        None
                    }
                });
            match taken {
                Some(("--pd", value)) => {
                    let raw = value?;
                    pd = Some(raw.parse().map_err(|error| {
                        invalid(format!("{raw} is not a placement-driver address: {error}"))
                    })?);
                }
                Some(("--tls-cert", value)) => tls_cert = Some(std::path::PathBuf::from(value?)),
                Some(("--tls-key", value)) => tls_key = Some(std::path::PathBuf::from(value?)),
                Some((_, _)) | None => positional.push(argument),
            }
        }
        let mut positional = positional.into_iter();
        Ok(Self {
            address: positional
                .next()
                .unwrap_or_else(|| "127.0.0.1:5432".to_owned()),
            stores: positional.collect(),
            pd,
            tls_cert,
            tls_key,
        })
    }
}

/// Fetches the first lease, starts the refresher thread, and hands back the report sink.
///
/// **The lease is fetched before this node serves anything.** A node that cannot reach PD at
/// startup does not come up holding a lease it never had; it fails, the way a store that cannot
/// reach PD fails to open (`esker_store::RemotePd`).
///
/// That first round also sends the first columnar report, which is what repairs a placement
/// driver that restarted while this node was up: the assertion is the whole set, so a node
/// starting is a node saying everything it knows.
async fn attach_pd(
    address: std::net::SocketAddr,
    lease: Arc<PdLease>,
    backend: &Arc<dyn Backend>,
) -> std::io::Result<Arc<dyn ColumnarReport>> {
    let conn = Arc::new(PdConn::new(address));
    let refresher = LeaseRefresher::new(Arc::clone(&conn), lease)
        .asserting_columnar_for(Arc::clone(backend), TENANT);
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
    Ok(conn)
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
    pd: Option<std::net::SocketAddr>,
) -> std::io::Result<(
    esker_client::TxnClient,
    Arc<dyn esker_client::TimestampOracle>,
    Arc<esker_client::Router>,
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
    // **With `--pd`, the routing table is PD's.** The `TODO(phase-6a)` that stood here is done:
    // a static one-region table is what a cluster *bootstraps* with, and it stops being true the
    // first time the cluster splits — and it can never say which peer of a region is a columnar
    // learner, because a learner joins through a conf change long after any table was written
    // down. Without `--pd` the old table stands, which is a node told where the stores are and
    // nothing else.
    let resolver: Arc<dyn esker_client::RegionResolver> = if let Some(address) = pd {
        Arc::new(PdConn::new(address))
    } else {
        let store_ids = transport.store_ids();
        Arc::new(esker_client::StaticRegion::replicated(1, &store_ids))
    };
    let router = Arc::new(esker_client::Router::new(Arc::new(transport), resolver));
    let oracle: Arc<dyn esker_client::TimestampOracle> =
        Arc::new(esker_client::CountingOracle::starting_at(1));
    Ok((
        esker_client::TxnClient::on_router(Arc::clone(&router), Arc::clone(&oracle)),
        oracle,
        router,
    ))
}
