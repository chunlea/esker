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
use esker_sql::exec::redrive::ReDriver;
use esker_sql::fragment::{ClientFragments, FragmentSource};
use esker_sql::pd::{ColumnarReport, LeaseRefresher, PdConn, PdLease};
use esker_sql::pgwire::server::{Auth, Config, serve};
use esker_sql::pgwire::tls::TlsConfig;

/// The tenant every connection is served as, until there is a way to say otherwise.
const TENANT: u64 = 1;

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
    let lease = (!pd.is_empty()).then(|| Arc::new(PdLease::new()));
    // The router the client was built on, kept so the fragment path can share it: one region cache
    // for both, so an entry a row read warmed is warm for a fragment.
    let mut router: Option<Arc<esker_client::Router>> = None;
    let mut reads: Option<Arc<esker_client::TxnClient>> = None;
    let backend: Arc<dyn Backend> = if stores.is_empty() {
        if !pd.is_empty() {
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
        let members = pd.clone();
        let (client, oracle, built) =
            tokio::task::spawn_blocking(move || connect(&stores, &members))
                .await
                .map_err(std::io::Error::other)??;
        router = Some(built);
        let client = Arc::new(client);
        // Kept so the safepoint reporter can ask it what it still has open (ADR 0110). The
        // backend owns it either way; this is a second handle, not a second client.
        reads = Some(Arc::clone(&client));
        let backend = StoreBackend::new(client, oracle);
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
    let columnar: Option<Arc<dyn ColumnarReport>> = if let Some(lease) = lease {
        Some(attach_pd(&pd, lease, &backend, reads.clone()).await?)
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
        .filter(|_| !pd.is_empty())
        .map(|router| Arc::new(ClientFragments::new(router)) as Arc<dyn FragmentSource>);
    if fragments.is_some() {
        tracing::info!("columnar routing is available: fragments go to the learners PD placed");
    }

    let mut sessions = esker_sql::node::Sessions::new(backend, catalog);
    if let Some(report) = columnar {
        sessions = sessions.reporting_columnar_to(report);
    }
    if let Some(source) = fragments {
        sessions = sessions.asking_fragments_of(source);
    }
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
/// `esker-sql [listen] [store...]` — and `--pd a[,b,c]` may appear anywhere among them.
#[derive(Debug)]
struct Args {
    /// Where to listen for clients.
    address: String,
    /// The stores to connect to; empty runs the in-process fake.
    stores: Vec<String>,
    /// Every member of the placement-driver group, or empty for a node with no driver.
    ///
    /// **No default and no discovery.** A node started without `--pd` behaves exactly as it did
    /// before the flag existed, which is what makes the flag additive rather than a change of
    /// behaviour with an opt-out.
    ///
    /// A **list**, comma-separated, because only the group's leader answers and leadership moves —
    /// and because a member can be killed, which is the case a node holding one address cannot
    /// survive
    /// ([ADR 0108](../../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)).
    /// One address is still valid and is a group of one, so no invocation had to change.
    pd: Vec<std::net::SocketAddr>,
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
        let mut pd: Vec<std::net::SocketAddr> = Vec::new();
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
                    // Comma-separated, as `esker server --pd` already reads it: one flag naming a
                    // group, rather than a flag that has to be repeated and a reader that has to
                    // know it may be.
                    let raw = value?;
                    for part in raw
                        .split(',')
                        .map(str::trim)
                        .filter(|part| !part.is_empty())
                    {
                        pd.push(part.parse().map_err(|error| {
                            invalid(format!("{part} is not a placement-driver address: {error}"))
                        })?);
                    }
                    if pd.is_empty() {
                        return Err(invalid(format!("`--pd {raw}` names no placement driver")));
                    }
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
    members: &[std::net::SocketAddr],
    lease: Arc<PdLease>,
    backend: &Arc<dyn Backend>,
    reads: Option<Arc<esker_client::TxnClient>>,
) -> std::io::Result<Arc<dyn ColumnarReport>> {
    let conn = Arc::new(
        PdConn::to_group(members, esker_proto::TransportConfig::new())
            .map_err(std::io::Error::other)?,
    );
    let address = conn.address();
    let mut refresher = LeaseRefresher::new(Arc::clone(&conn), lease)
        .asserting_columnar_for(Arc::clone(backend), TENANT);
    // **The reader floor of the cluster's safepoint** (ADR 0110). A node that does not report
    // holds nothing down, so failing to take an id is loud but not fatal: PD's per-reporter TTL
    // decides what a silent node means, and the window is the other half.
    if let Some(client) = reads {
        match conn.alloc_id(1) {
            Ok(reporter_id) => {
                tracing::info!(reporter_id, "reporting this node's oldest open read to PD");
                refresher = refresher.reporting_reads_from(client, reporter_id);
            }
            Err(error) => tracing::warn!(
                %error,
                "could not take a reporter id; this node will not report its open reads, and the \
                 safepoint will rest on the retention window alone"
            ),
        }
    }
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
    pd: &[std::net::SocketAddr],
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
    // **One connection, both questions.** The driver says where the regions are *and* what time it
    // is, and a node that asked two different things about the cluster over two sockets would have
    // two ways to be half-connected.
    let (resolver, oracle): (
        Arc<dyn esker_client::RegionResolver>,
        Arc<dyn esker_client::TimestampOracle>,
    ) = if pd.is_empty() {
        // **A local counter is a correct oracle for exactly one node**, and without `--pd` there is
        // no driver to ask. It is *not* correct for two: two processes counting from one hand the
        // same `start_ts` to different transactions, which is `CLAUDE.md` invariant 6 gone and
        // every MVCC decision with it (`tests/two_nodes_one_clock.rs`). So this arm is the
        // single-node one and says so.
        let store_ids = transport.store_ids();
        (
            Arc::new(esker_client::StaticRegion::replicated(1, &store_ids)) as Arc<_>,
            Arc::new(esker_client::CountingOracle::starting_at(1)) as Arc<_>,
        )
    } else {
        let conn = Arc::new(
            PdConn::to_group(pd, TransportConfig::default()).map_err(std::io::Error::other)?,
        );
        (Arc::clone(&conn) as Arc<_>, conn as Arc<_>)
    };
    let router = Arc::new(esker_client::Router::new(Arc::new(transport), resolver));
    Ok((
        esker_client::TxnClient::on_router(Arc::clone(&router), Arc::clone(&oracle)),
        oracle,
        router,
    ))
}
