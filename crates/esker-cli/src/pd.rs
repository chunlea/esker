//! `esker pd` — run the placement driver, or look at what it has stored.
//!
//! `serve` is the process `docs/DESIGN.md` §7 describes: one PD, its state in its own engine,
//! answering the six methods of service `0x03`. `inspect` opens that state directly and prints
//! it, the way `sst-dump` and `manifest-dump` read the formats they belong to — so a cluster
//! that is misrouting can be diagnosed without a client and without guessing.
//!
//! Phase 4a is a **single** PD. It is a single point of failure, and that is a stated property
//! of the sub-phase rather than an oversight: high availability is 4e, where three of these
//! replicate with `esker-raft` (`prompts/04-multiraft-pd.md`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use esker_pd::{Pd, PdOptions, PdService};
use esker_proto::{Server, TransportConfig};

/// Where `--listen` points when nothing says otherwise.
///
/// The port PD's clients use in `TiKV`, which this layer is modelled on (`CLAUDE.md`): a
/// familiar number is kinder than a new one.
pub(crate) const DEFAULT_LISTEN: &str = "127.0.0.1:2379";

/// Where PD keeps its state when nothing says otherwise.
pub(crate) const DEFAULT_DATA_DIR: &str = "esker-pd-data";

/// What `esker pd` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PdCommand {
    /// Open PD's state and serve it.
    Serve(ServeOptions),
    /// Print what PD has stored, without serving anything.
    Inspect(InspectOptions),
}

/// `esker pd serve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServeOptions {
    /// The directory holding PD's database. Created if it is not there.
    pub(crate) data_dir: PathBuf,
    /// The address to listen on.
    pub(crate) listen: String,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            listen: DEFAULT_LISTEN.to_owned(),
        }
    }
}

/// `esker pd inspect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InspectOptions {
    /// The directory holding PD's database. It must already exist.
    pub(crate) data_dir: PathBuf,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
        }
    }
}

/// Runs the command, and returns when it has finished.
pub(crate) fn run(command: &PdCommand) -> Result<(), String> {
    match command {
        PdCommand::Serve(options) => serve(options),
        PdCommand::Inspect(options) => {
            let mut stdout = std::io::stdout().lock();
            inspect(options, &mut stdout)
        }
    }
}

fn serve(options: &ServeOptions) -> Result<(), String> {
    let address: SocketAddr = options
        .listen
        .parse()
        .map_err(|error| format!("`--listen {}` is not an address: {error}", options.listen))?;

    let pd = Pd::open(&options.data_dir, PdOptions::new())
        .map_err(|error| format!("opening {}: {error}", options.data_dir.display()))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("building the runtime: {error}"))?;

    runtime.block_on(async move {
        let server = Server::bind(
            address,
            PdService::new(Arc::clone(&pd)),
            TransportConfig::new(),
        )
        .await
        .map_err(|error| format!("listening on {address}: {error}"))?;
        let bound = server
            .local_addr()
            .map_err(|error| format!("reading the bound address: {error}"))?;

        match pd.cluster() {
            Ok(Some(cluster)) => println!(
                "esker pd: cluster {} listening on {bound}, data in {}",
                cluster.cluster_id,
                options.data_dir.display()
            ),
            Ok(None) => println!(
                "esker pd: listening on {bound}, data in {} — no cluster yet, waiting for a \
                 store to bootstrap one",
                options.data_dir.display()
            ),
            Err(error) => return Err(format!("reading the cluster record: {error}")),
        }

        server
            .serve(shutdown_signal())
            .await
            .map_err(|error| format!("serving: {error}"))?;
        println!("esker pd: stopped");
        Ok(())
    })
}

/// Resolves on the first ctrl-C.
async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => println!("esker pd: shutting down, finishing in-flight requests"),
        Err(error) => eprintln!("esker pd: cannot listen for ctrl-c ({error}); stopping"),
    }
}

/// Prints PD's whole state: the cluster, the allocator, the oracle's mark, every store and
/// every region.
///
/// It opens the database, so it is for a PD that is **stopped** — the same rule the other
/// `dump` commands follow.
pub(crate) fn inspect(
    options: &InspectOptions,
    out: &mut impl std::io::Write,
) -> Result<(), String> {
    // An inspector must not create what it was asked to look at: a typo in a path should be an
    // error, not an empty database that looks like a wiped cluster.
    let engine = esker_engine::Options {
        create_if_missing: false,
        ..esker_engine::Options::default()
    };
    let pd = Pd::open(
        &options.data_dir,
        PdOptions {
            engine,
            ..PdOptions::new()
        },
    )
    .map_err(|error| format!("opening {}: {error}", options.data_dir.display()))?;

    let write = |error: std::io::Error| format!("writing: {error}");

    match pd.cluster().map_err(|error| error.to_string())? {
        Some(cluster) => {
            writeln!(out, "cluster        {}", cluster.cluster_id).map_err(write)?;
            writeln!(out, "first region   {}", cluster.first_region_id).map_err(write)?;
            writeln!(out, "created (ms)   {}", cluster.created_ms).map_err(write)?;
        }
        None => writeln!(out, "cluster        (not bootstrapped)").map_err(write)?,
    }
    writeln!(
        out,
        "tso mark (ms)  {}",
        pd.tso_high_water_ms().map_err(|error| error.to_string())?
    )
    .map_err(write)?;

    let stores = pd.stores().map_err(|error| error.to_string())?;
    writeln!(out, "\nstores ({})", stores.len()).map_err(write)?;
    for store in &stores {
        writeln!(
            out,
            "  {:>4}  {:<24} last beat {} ms  regions {}  leaders {}  {}/{} bytes free",
            store.store_id,
            store.address,
            store.last_heartbeat_ms,
            store.stats.region_count,
            store.stats.leader_count,
            store.stats.available,
            store.stats.capacity,
        )
        .map_err(write)?;
    }

    let regions = pd.regions().map_err(|error| error.to_string())?;
    writeln!(out, "\nregions ({})", regions.len()).map_err(write)?;
    for region in &regions {
        writeln!(
            out,
            "  {:>4}  [{}, {})  epoch ({}, {})  leader {}  term {}  ~{} bytes  applied {}",
            region.region.id,
            crate::bytes::escape(&region.region.start_key),
            if region.region.end_key.is_empty() {
                "+inf".to_owned()
            } else {
                crate::bytes::escape(&region.region.end_key)
            },
            region.region.epoch.conf_ver,
            region.region.epoch.version,
            region.leader_peer_id,
            region.term,
            region.approximate_size,
            region.applied_index,
        )
        .map_err(write)?;
        for peer in &region.region.peers {
            writeln!(
                out,
                "        peer {} on store {} ({:?})",
                peer.peer_id, peer.store_id, peer.role
            )
            .map_err(write)?;
        }
    }

    // The index is what a lookup actually walks, so a disagreement between it and the records
    // is the failure that would make routing wrong while everything above still looked right.
    let index = esker_pd::routing::range_index(pd.db()).map_err(|error| error.to_string())?;
    writeln!(out, "\nrange index ({})", index.len()).map_err(write)?;
    for (end, region_id) in &index {
        writeln!(
            out,
            "  {:<20} -> region {region_id}",
            if end.is_empty() {
                "+inf".to_owned()
            } else {
                crate::bytes::escape(end)
            },
        )
        .map_err(write)?;
    }
    if index.len() != regions.len() {
        writeln!(
            out,
            "\nWARNING: {} regions and {} index entries — the routing table is inconsistent",
            regions.len(),
            index.len()
        )
        .map_err(write)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LISTEN, InspectOptions, ServeOptions, inspect};

    #[test]
    fn the_defaults_are_usable() {
        let options = ServeOptions::default();
        assert_eq!(options.listen, DEFAULT_LISTEN);
        assert!(DEFAULT_LISTEN.parse::<std::net::SocketAddr>().is_ok());
        assert!(!options.data_dir.as_os_str().is_empty());
    }

    /// An inspector that created the database it was asked to read would turn a typo into a
    /// report of an empty cluster, which is the most misleading answer available.
    #[test]
    fn inspecting_a_directory_that_is_not_there_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let options = InspectOptions {
            data_dir: dir.path().join("nothing-here"),
        };
        let mut out = Vec::new();
        assert!(inspect(&options, &mut out).is_err());
        assert!(!dir.path().join("nothing-here").exists());
    }

    #[test]
    fn inspect_prints_the_cluster_the_regions_and_the_index() {
        let dir = tempfile::tempdir().unwrap();
        {
            let pd = esker_pd::Pd::open(dir.path(), esker_pd::PdOptions::new()).unwrap();
            pd.bootstrap(1, "127.0.0.1:20160").unwrap();
        }
        let mut out = Vec::new();
        inspect(
            &InspectOptions {
                data_dir: dir.path().to_path_buf(),
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("first region   1"), "{text}");
        assert!(text.contains("stores (1)"), "{text}");
        assert!(text.contains("127.0.0.1:20160"), "{text}");
        assert!(text.contains("regions (1)"), "{text}");
        assert!(text.contains("[, +inf)"), "the unbounded region: {text}");
        assert!(text.contains("range index (1)"), "{text}");
        assert!(!text.contains("WARNING"), "{text}");
    }

    /// A PD nothing has bootstrapped says so rather than printing a cluster id of zero.
    #[test]
    fn inspect_says_when_there_is_no_cluster() {
        let dir = tempfile::tempdir().unwrap();
        drop(esker_pd::Pd::open(dir.path(), esker_pd::PdOptions::new()).unwrap());
        let mut out = Vec::new();
        inspect(
            &InspectOptions {
                data_dir: dir.path().to_path_buf(),
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("(not bootstrapped)"), "{text}");
        assert!(text.contains("regions (0)"), "{text}");
    }
}
