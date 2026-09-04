//! `esker server` — open a store and serve it.
//!
//! The whole of the phase-2 process: an engine in a directory, one region covering everything,
//! and the `RawKv` API on a socket. It is deliberately the only place in the CLI that builds a
//! `tokio` runtime — `esker raw` and `bench --remote` go through `esker-client`, which does its
//! own blocking underneath (`CLAUDE.md`, "async only at the network edge").
//!
//! Shutdown is the part worth reading. Ctrl-C stops the listener, lets the requests already
//! running finish, and only then drops the `Store` — so the database is closed by a process
//! that knows no handler is still writing to it. A second ctrl-C stops waiting, because a
//! shutdown that cannot be interrupted is a process that has to be killed.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use esker_proto::{Server, TransportConfig};
use esker_store::server::RaftOptions;
use esker_store::{PdClient, PeerAddress, RemotePd, Store, StoreOptions, StoreService};

/// Where `--listen` points when nothing says otherwise.
///
/// The same port `esker raw --addr` defaults to, and the same one `TiKV` serves its store API on:
/// this layer is modelled on it (`CLAUDE.md`), so a familiar number is kinder than a new one.
pub(crate) const DEFAULT_LISTEN: &str = "127.0.0.1:20160";

/// What `esker server` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerOptions {
    /// The directory holding the database. Created if it is not there.
    pub(crate) data_dir: PathBuf,
    /// The address to listen on.
    pub(crate) listen: String,
    /// This store's id, reported in the handshake.
    pub(crate) store_id: u64,
    /// This store's Raft peer id for the region it serves. Defaults to the store id, which is
    /// what a single-region cluster wants and what `esker cluster start` passes.
    pub(crate) peer_id: u64,
    /// Every peer of the region, as `id@address`, this store's included. Empty means an
    /// unreplicated store — exactly phase 2's, and still the default.
    pub(crate) peers: Vec<(u64, String)>,
    /// Seed for the election-timeout RNG. A whole cluster shares one: the peer id selects the
    /// stream (`docs/adr/0008-raft-determinism-and-the-driver-contract.md`).
    pub(crate) seed: u64,
    /// The memtable size, in bytes, or `None` for the engine's 64 MiB default.
    ///
    /// Exposed because a small deployment wants a smaller one, and because an acceptance test
    /// that has to produce an SST otherwise has to push 64 MiB through Raft to get one
    /// (`docs/bench/phase-4.md` made the same complaint about the knobs this command does not
    /// have).
    pub(crate) write_buffer_size: Option<usize>,
    /// Tier the SSTs into `s3://bucket/prefix` instead of leaving them on local disk.
    ///
    /// One prefix per store: two databases sharing one would overwrite each other's
    /// `000007.sst`, because a file number restarts at one in every database
    /// (`crate::sst_store`). `esker cluster start` derives a per-node prefix for exactly that
    /// reason; an operator running `esker server` by hand owns it.
    pub(crate) sst_store: Option<String>,
    /// Claim an `--sst-store` prefix that already holds objects but carries no claim marker.
    ///
    /// The escape hatch, and deliberately an awkward one. A prefix with objects and no marker
    /// was either written before markers existed or had its marker deleted, and from outside
    /// there is no way to tell that from a prefix another live database is still using. Refusing
    /// is the default; this says "I have checked, it is mine"
    /// (`docs/adr/0029-the-sst-store-claim.md`).
    pub(crate) adopt_sst_store: bool,
    /// The placement driver to register with and report to. `None` is a store that bootstraps
    /// its own region and reports to nobody — phase 2's single node and phase 3e's static
    /// cluster, both of which this command still starts.
    pub(crate) pd: Option<String>,
    /// Approximate region bytes past which a leader looks for a split key, or `None` for the
    /// store's 96 MiB default (`docs/DESIGN.md` §14).
    ///
    /// A store with no `--pd` never splits whatever this says, because a split needs
    /// cluster-unique ids. Exposed for the same reason `--write-buffer-size` is: an acceptance
    /// run that wants a split otherwise has to push 96 MiB through Raft to get one, and
    /// `docs/bench/phase-4.md` had to wrap this binary to do it.
    pub(crate) region_split_size: Option<u64>,
    /// How often this store reports itself to PD, in milliseconds. `None` is §14's 10 s.
    pub(crate) store_heartbeat_ms: Option<u64>,
    /// How often each region's leader reports it absent a change, in milliseconds. `None` is
    /// §14's 60 s.
    ///
    /// **Also the latency of an operator**: PD answers a region heartbeat and has no other way to
    /// reach a store, so a repair waits at most this long. That is why a test cluster sets it low
    /// and why it is a flag rather than a constant.
    pub(crate) region_heartbeat_ms: Option<u64>,
    /// What one heartbeat tick is worth, in milliseconds. `None` is the store's default.
    ///
    /// The **resolution** of the schedule rather than its period: the two intervals above are
    /// counted in these ([`esker_store::Heartbeats`]), so an interval below one tick is rounded up
    /// to one and setting a short interval without also shortening the tick does nothing.
    pub(crate) heartbeat_tick_ms: Option<u64>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("esker-data"),
            listen: DEFAULT_LISTEN.to_owned(),
            store_id: 1,
            peer_id: 1,
            peers: Vec::new(),
            seed: 0,
            write_buffer_size: None,
            region_split_size: None,
            store_heartbeat_ms: None,
            region_heartbeat_ms: None,
            heartbeat_tick_ms: None,
            sst_store: None,
            adopt_sst_store: false,
            pd: None,
        }
    }
}

/// Opens the store, serves it, and returns when it has stopped cleanly.
pub(crate) fn run(options: &ServerOptions) -> Result<(), String> {
    let address: SocketAddr = options
        .listen
        .parse()
        .map_err(|error| format!("`--listen {}` is not an address: {error}", options.listen))?;

    // A replicated store's transport tasks and ticker live in a runtime, so the runtime is
    // built before the store rather than after it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("building the runtime: {error}"))?;
    let _guard = runtime.enter();

    let raft = if options.peers.is_empty() {
        None
    } else {
        let mut peers = Vec::with_capacity(options.peers.len());
        for (id, address) in &options.peers {
            let addr: SocketAddr = address
                .parse()
                .map_err(|error| format!("`--peer {id}@{address}` is not an address: {error}"))?;
            peers.push(PeerAddress::new(*id, *id, addr));
        }
        Some(RaftOptions::new(peers, options.seed))
    };

    // Connecting is lazy, so a placement driver that is not up yet fails the *bootstrap* with
    // a message naming it rather than failing here with one about a socket.
    let pd = match &options.pd {
        None => None,
        Some(listed) => {
            // A **list**, because a placement driver is a Raft group of up to three and only its
            // leader answers ([ADR 0058](../../../docs/adr/0058-pd-is-a-raft-group.md)). One
            // address is still one address, so every existing invocation means what it did.
            let mut endpoints = Vec::new();
            for part in listed.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                endpoints.push(part.parse::<SocketAddr>().map_err(|error| {
                    format!("`--pd {listed}`: `{part}` is not an address: {error}")
                })?);
            }
            if endpoints.is_empty() {
                return Err(format!("`--pd {listed}` names no placement driver"));
            }
            let client = RemotePd::connect_to(&endpoints, TransportConfig::new())
                .map_err(|error| format!("starting the placement-driver client: {error}"))?;
            Some(Arc::new(client) as Arc<dyn PdClient>)
        }
    };

    // Built before the store, and a failure here is a startup failure: a `--sst-store` that
    // cannot be reached is a misconfiguration, and a store that started anyway would write
    // SSTs nobody asked it to keep locally and report success.
    // The cluster id is not known until PD answers, and PD is not asked until the store opens,
    // so the marker carries the store id and the cluster the operator named. They are the
    // informational half of the claim; the authority is the id in the data directory.
    let fs = crate::sst_store::filesystem(
        options.sst_store.as_deref(),
        &options.data_dir,
        None,
        true,
        crate::sst_store::Claim {
            cluster_id: 0,
            store_id: options.store_id,
            adopt: options.adopt_sst_store,
        },
    )?;

    let mut engine = StoreOptions::new().engine;
    if let Some(size) = options.write_buffer_size {
        engine.cf_options.write_buffer_size = size;
        for override_options in engine.cf_overrides.values_mut() {
            override_options.write_buffer_size = size;
        }
    }

    let store = Store::open(
        &options.data_dir,
        store_options(options, engine, fs, raft, pd),
    )
    .map_err(|error| format!("opening {}: {error}", options.data_dir.display()))?;

    let served = runtime.block_on(serve(Arc::clone(&store), address, options));

    // The store outlives the server on purpose: `serve` has returned, so every handler has
    // finished, and only now is it safe to flush and drop the database. Replication stops first:
    // a peer still driving Raft would keep writing to a database about to be dropped.
    store.stop();
    if let Err(error) = store.flush() {
        eprintln!("esker server: flushing on shutdown: {error}");
    }
    drop(store);
    served
}

/// The store's options, from the flags and the pieces `run` has already built.
///
/// A function rather than an expression inside `run` because it is the half a test can check:
/// every knob here is a flag that parses fine and does nothing at all if it is dropped on the way
/// through, which is a failure no parse test can see.
fn store_options(
    options: &ServerOptions,
    engine: esker_engine::Options,
    fs: Arc<dyn esker_engine::fs::FileSystem>,
    raft: Option<RaftOptions>,
    pd: Option<Arc<dyn PdClient>>,
) -> StoreOptions {
    let defaults = StoreOptions::new();
    let mut split = defaults.split;
    if let Some(size) = options.region_split_size {
        split.region_split_size = size;
    }
    let ms = |value: Option<u64>, fallback: std::time::Duration| {
        value.map_or(fallback, std::time::Duration::from_millis)
    };

    StoreOptions {
        store_id: options.store_id,
        peer_id: options.peer_id,
        engine,
        fs,
        raft,
        pd,
        // What PD records as this store's address is the address it was told to listen on, not
        // the one it resolved to: `0.0.0.0:0` resolves to something no peer can use.
        address: options.listen.clone(),
        split,
        heartbeat_tick: ms(options.heartbeat_tick_ms, defaults.heartbeat_tick),
        store_heartbeat: ms(options.store_heartbeat_ms, defaults.store_heartbeat),
        region_heartbeat: ms(options.region_heartbeat_ms, defaults.region_heartbeat),
        ..defaults
    }
}

async fn serve(
    store: Arc<Store>,
    address: SocketAddr,
    options: &ServerOptions,
) -> Result<(), String> {
    let service = StoreService::new(Arc::clone(&store));
    let server = Server::bind(address, service, TransportConfig::new())
        .await
        .map_err(|error| format!("listening on {address}: {error}"))?;
    let bound = server
        .local_addr()
        .map_err(|error| format!("reading the bound address: {error}"))?;

    println!(
        "esker server: store {} listening on {bound}, data in {}",
        options.store_id,
        options.data_dir.display()
    );
    if let Some(pd) = &options.pd {
        println!("esker server: registered with the placement driver at {pd}");
    }
    // Every region this store hosts, in key order. In phase 4a that is one, bootstrapped to
    // cover everything; `TODO(phase-4b)` a split makes the list grow while the server runs, and
    // this line only says what it found at open.
    for region in store.regions().regions() {
        println!(
            "esker server: region {} covers [{}, {})",
            region.id,
            crate::bytes::escape(&region.start_key),
            if region.end_key.is_empty() {
                "+inf".to_owned()
            } else {
                crate::bytes::escape(&region.end_key)
            },
        );
        if !options.peers.is_empty() {
            println!(
                "esker server: peer {} of region {}, replicating with {} peers",
                options.peer_id,
                region.id,
                region.peers.len()
            );
        }
    }

    server
        .serve(shutdown_signal())
        .await
        .map_err(|error| format!("serving: {error}"))?;
    println!("esker server: stopped");
    Ok(())
}

/// Resolves on the first ctrl-C.
///
/// A failure to install the handler resolves immediately rather than being ignored: a server
/// that cannot be stopped politely should not pretend it can.
async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => println!("esker server: shutting down, finishing in-flight requests"),
        Err(error) => eprintln!("esker server: cannot listen for ctrl-c ({error}); stopping"),
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LISTEN, ServerOptions};

    /// The server and the client must default to the same place, or `esker server` followed by
    /// `esker raw get` fails for a reason that has nothing to do with either.
    #[test]
    fn the_server_and_the_client_default_to_the_same_address() {
        assert_eq!(DEFAULT_LISTEN, crate::raw::DEFAULT_ADDR);
        assert!(DEFAULT_LISTEN.parse::<std::net::SocketAddr>().is_ok());
    }

    #[test]
    fn the_defaults_are_usable() {
        let options = ServerOptions::default();
        assert_eq!(options.listen, DEFAULT_LISTEN);
        assert_eq!(options.store_id, 1);
        assert!(!options.data_dir.as_os_str().is_empty());
    }

    /// **The knobs reach the store.** Each of these parses fine and does nothing at all if it is
    /// dropped on the way through, which is a failure a parse test cannot see.
    #[test]
    fn the_new_knobs_reach_the_store_options() {
        use std::time::Duration;

        let options = ServerOptions {
            region_split_size: Some(4 * 1024 * 1024),
            store_heartbeat_ms: Some(250),
            region_heartbeat_ms: Some(500),
            heartbeat_tick_ms: Some(50),
            ..ServerOptions::default()
        };
        let built = super::store_options(
            &options,
            esker_store::StoreOptions::new().engine,
            std::sync::Arc::new(esker_engine::fs::LocalFileSystem::new()),
            None,
            None,
        );

        assert_eq!(built.split.region_split_size, 4 * 1024 * 1024);
        assert_eq!(built.store_heartbeat, Duration::from_millis(250));
        assert_eq!(built.region_heartbeat, Duration::from_millis(500));
        assert_eq!(built.heartbeat_tick, Duration::from_millis(50));
        // Nothing else moved: `max_sampled_keys` is the other half of `SplitOptions` and has no
        // flag, so it must still be the store's own default.
        assert_eq!(
            built.split.max_sampled_keys,
            esker_store::StoreOptions::new().split.max_sampled_keys
        );
    }

    /// Without the flags, the store's own defaults — `docs/DESIGN.md` §14's, and this is what
    /// keeps a flag's absence from being a different configuration from not having the flag.
    #[test]
    fn without_the_knobs_the_store_defaults_stand() {
        let defaults = esker_store::StoreOptions::new();
        let built = super::store_options(
            &ServerOptions::default(),
            defaults.engine.clone(),
            std::sync::Arc::new(esker_engine::fs::LocalFileSystem::new()),
            None,
            None,
        );
        assert_eq!(
            built.split.region_split_size,
            defaults.split.region_split_size
        );
        assert_eq!(built.store_heartbeat, defaults.store_heartbeat);
        assert_eq!(built.region_heartbeat, defaults.region_heartbeat);
        assert_eq!(built.heartbeat_tick, defaults.heartbeat_tick);
    }
}
