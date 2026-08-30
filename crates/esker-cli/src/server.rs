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
use esker_store::{Store, StoreOptions, StoreService};

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
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("esker-data"),
            listen: DEFAULT_LISTEN.to_owned(),
            store_id: 1,
        }
    }
}

/// Opens the store, serves it, and returns when it has stopped cleanly.
pub(crate) fn run(options: &ServerOptions) -> Result<(), String> {
    let address: SocketAddr = options
        .listen
        .parse()
        .map_err(|error| format!("`--listen {}` is not an address: {error}", options.listen))?;

    let store = Store::open(
        &options.data_dir,
        StoreOptions {
            store_id: options.store_id,
            ..StoreOptions::new()
        },
    )
    .map_err(|error| format!("opening {}: {error}", options.data_dir.display()))?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("building the runtime: {error}"))?;

    let served = runtime.block_on(serve(Arc::clone(&store), address, options));

    // The store outlives the server on purpose: `serve` has returned, so every handler has
    // finished, and only now is it safe to flush and drop the database.
    if let Err(error) = store.flush() {
        eprintln!("esker server: flushing on shutdown: {error}");
    }
    drop(store);
    served
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
    println!(
        "esker server: region {} covers the whole key space",
        store.region().id
    );

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
}
