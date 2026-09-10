//! A long-lived client survives a store restarting under it.
//!
//! # What this is a regression on
//!
//! run 122 reproduced run 120's stall with the census running, and the census said it was **not
//! Raft**: through the whole eighty seconds every peer sat at `term=2`, `answered_by ==
//! handle_peer` without exception, and `applied`/`commit`/`last_index` were frozen at the same
//! number on all three. The group was healthy and idle — *no write ever arrived*. What the writer
//! saw was `request not sent: the connection is closed`, seven million times.
//!
//! `TcpStores` built one connection per store at construction and never built another, and did not
//! keep the addresses it built them from, so it could not have. A store that is killed closes its
//! connection; every later call to that store answers `NotSent`, which is deliberately **not**
//! retryable — it is the class that provably never left the client — so nothing above retries or
//! re-routes, and nothing ever will again.
//!
//! It is worse than a stalled load generator. **Every `esker-sql` node holds a long-lived client
//! too**, so before this a store restart cost that node the store for the life of the process.
//!
//! # Why run 121 did not see it
//!
//! Its poller spawned a fresh `esker raw` per probe — a new process, a new client, a new
//! connection. Only a client that outlives a kill can hold a dead one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_client::region_cache::StaticRegion;
use esker_client::{RawClient, TcpStores};
use tempfile::TempDir;

const REGION: u64 = 1;
/// How long the client has to notice a store is back. Generous against the 500 ms the supervisor
/// waits and the time a store takes to open: this is a bound against hanging, not the assertion.
const RECOVER_WITHIN: Duration = Duration::from_secs(30);
const LISTENING_WITHIN: Duration = Duration::from_secs(30);

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("a free port")
        .local_addr()
        .expect("its address")
        .port()
}

/// One store, on `address` and `dir`, with its output going to `log`.
fn start(dir: &Path, address: SocketAddr, log: &Path) -> Server {
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .expect("the server log");
    let errors = out.try_clone().expect("the server log");
    Server(
        Command::new(env!("CARGO_BIN_EXE_esker-cli"))
            .arg("server")
            .arg("--data-dir")
            .arg(dir)
            .arg("--listen")
            .arg(address.to_string())
            .arg("--store-id")
            .arg("1")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("the server command starts"),
    )
}

/// Waits until something answers a client handshake on `address`.
fn wait_until_up(address: SocketAddr) {
    let deadline = Instant::now() + LISTENING_WITHIN;
    while Instant::now() < deadline {
        if TcpStores::connect(address).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("no store answered on {address} within {LISTENING_WITHIN:?}");
}

#[test]
#[ignore = "starts a store process and kills it; run with --run-ignored all"]
fn a_client_finds_a_store_that_was_killed_and_came_back() {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("node");
    let log = dir.path().join("server.log");
    let address: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();

    let first = start(&data, address, &log);
    wait_until_up(address);

    // **One client, held across the kill.** This is the whole arrangement: `esker-sql` holds one
    // for the life of the process, and so does `esker durability record`.
    let stores = TcpStores::connect(address).expect("the client connects");
    let store_id = stores.only_store().expect("the store named itself");
    let client = RawClient::new(
        Arc::new(stores),
        Arc::new(StaticRegion::whole_key_space(REGION, store_id, 0)),
    );
    client
        .put(b"before", b"the kill")
        .expect("the cluster serves before the kill");

    // `kill -9` by pid, for the reason `cluster_chaos.rs` gives: this workspace compiles no C.
    let pid = first.0.id();
    assert!(
        Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status()
            .unwrap()
            .success(),
        "SIGKILL was sent"
    );
    drop(first);

    // Back on the same address and the same data directory, which is what a supervisor's respawn
    // and an operator's restart both do.
    let _second = start(&data, address, &log);
    wait_until_up(address);

    let deadline = Instant::now() + RECOVER_WITHIN;
    loop {
        let last = match client.put(b"after", b"the restart") {
            Ok(()) => break,
            Err(error) => error.to_string(),
        };
        assert!(
            Instant::now() < deadline,
            "the store has been back for {RECOVER_WITHIN:?} and the client has not found it: \
             {last}. A client that holds a closed connection for ever loses a store to every \
             restart, and an `esker-sql` node holds one for the life of the process."
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // And what was written before the kill is still there, read through the same client — so the
    // recovery is a reconnection and not a client that quietly started talking to somewhere else.
    assert_eq!(
        client.get(b"before").expect("a read after the recovery"),
        Some(bytes::Bytes::from_static(b"the kill"))
    );
}
