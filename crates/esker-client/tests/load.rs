//! Sixty-four clients at once, and what the store is allowed to say to them.
//!
//! `prompts/02-single-node-server.md`: "64 concurrent clients for 60 s; assert no error other
//! than `ServerIsBusy`, and that `ServerIsBusy` corresponds to a real engine stall (metric)
//! rather than a bug."
//!
//! The assertion is a **whitelist of one**. Under load a store may legitimately shed work —
//! that is what `ServerIsBusy` is for, and it is retryable precisely because it is a refusal.
//! Anything else means the concurrency itself broke something: a demultiplexer handing a
//! response to the wrong caller shows up as `UnexpectedResponse`, a connection torn down under
//! pressure as `Closed`, a bound miscounted as `Internal`. None of those are load; they are
//! bugs that only appear under it.
//!
//! # Correctness, not only survival
//!
//! Counting errors is the weaker half. Every client also records the writes it was told
//! succeeded, and at the end every one of them must read back with the value it was given.
//! That is what catches the failure a pure error count cannot see: a response routed to the
//! wrong caller, where both calls "succeed" and one of them is answered with the other's data.
//!
//! # Where `ServerIsBusy` would come from
//!
//! In this phase it is the transport's in-flight bound, not the engine's write stall: the
//! engine *blocks* a stalled writer rather than refusing it, and there is no path from a stall
//! to a wire error yet. Sixty-four blocking clients hold at most one request each across
//! sixty-four connections, so nothing here should reach a bound of 4,096 per connection. The
//! test therefore expects zero, reports the engine's stall counters either way, and fails
//! loudly on anything that is not `ServerIsBusy` — so if a future phase does start shedding,
//! this is where it is accounted for rather than explained away.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use esker_client::region_cache::StaticRegion;
use esker_client::wire::ProtoError;
use esker_client::{Error, RawClient, TcpStores};

/// The prompt's number.
const CLIENTS: u32 = 64;

/// Seconds of load in an ordinary run. The prompt's sixty are the on-demand run below: a
/// minute of fsyncs in `just check` buys no coverage the first seconds have not already given.
const SECONDS: u64 = 3;

/// Seconds in the acceptance run.
const SECONDS_IGNORED: u64 = 60;

const REGION: u64 = 1;

/// A store, a server and a runtime, alive for as long as this value is.
struct Harness {
    addr: SocketAddr,
    store: Arc<esker_store::Store>,
    _handle: esker_proto::transport::ServerHandle,
    _runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

fn start() -> Harness {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("a runtime");

    let store = esker_store::Store::open(dir.path(), esker_store::StoreOptions::new())
        .expect("the store opens");
    let service: Arc<dyn esker_proto::transport::Service> =
        esker_store::StoreService::new(Arc::clone(&store));

    let handle = runtime.block_on(async {
        esker_proto::transport::Server::bind(
            "127.0.0.1:0",
            service,
            esker_proto::transport::TransportConfig::new(),
        )
        .await
        .expect("the server binds")
        .spawn()
        .expect("the server starts")
    });

    Harness {
        addr: handle.local_addr(),
        store,
        _handle: handle,
        _runtime: runtime,
        _dir: dir,
    }
}

fn client_for(addr: SocketAddr) -> RawClient {
    let stores = TcpStores::connect(addr).expect("the client connects");
    let store_id = stores.only_store().expect("the server named its store");
    RawClient::new(
        Arc::new(stores),
        Arc::new(StaticRegion::whole_key_space(REGION, store_id, 0)),
    )
}

fn key_of(client: u32, index: u64) -> Vec<u8> {
    format!("load{client:03}-{index:012}").into_bytes()
}

fn value_of(client: u32, index: u64) -> Vec<u8> {
    // The value names the write that made it, so a response delivered to the wrong caller is
    // visible as a value rather than as a count that does not add up.
    format!("client{client}/write{index}").into_bytes()
}

/// One failure, classified. The message is kept for the report; the variant is what is
/// asserted on.
#[derive(Debug)]
struct Failure {
    operation: &'static str,
    error: Error,
}

impl Failure {
    /// Whether this is the one thing a loaded store is allowed to say.
    fn is_server_busy(&self) -> bool {
        matches!(
            &self.error,
            Error::Store(ProtoError::ServerIsBusy { .. })
                | Error::RetriesExhausted { .. }
                | Error::DeadlineExceeded { .. }
        ) && self.busy_source().is_some()
    }

    fn busy_source(&self) -> Option<&ProtoError> {
        match &self.error {
            Error::Store(error @ ProtoError::ServerIsBusy { .. }) => Some(error),
            Error::RetriesExhausted { source, .. } => match source.as_ref() {
                error @ ProtoError::ServerIsBusy { .. } => Some(error),
                _ => None,
            },
            Error::DeadlineExceeded { source, .. } => match source.as_deref() {
                Some(error @ ProtoError::ServerIsBusy { .. }) => Some(error),
                _ => None,
            },
            _ => None,
        }
    }
}

/// What one run of the load produced.
struct Outcome {
    /// Which writes each client was told had succeeded.
    acked_by_client: BTreeMap<u32, Vec<u64>>,
    failures: Vec<Failure>,
    operations: u64,
}

/// Puts `CLIENTS` clients on the store until `deadline`, and collects what happened.
fn drive(addr: SocketAddr, deadline: Instant) -> Outcome {
    let stop = Arc::new(AtomicBool::new(false));
    let failures: Arc<Mutex<Vec<Failure>>> = Arc::new(Mutex::new(Vec::new()));
    let operations = Arc::new(AtomicU64::new(0));

    let workers: Vec<_> = (0..CLIENTS)
        .map(|client| {
            let stop = Arc::clone(&stop);
            let failures = Arc::clone(&failures);
            let operations = Arc::clone(&operations);
            std::thread::spawn(move || {
                // One connection per client, which is what "64 concurrent clients" means.
                let raw = client_for(addr);
                let mut acked: Vec<u64> = Vec::new();
                let mut index = 0u64;

                while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                    let key = key_of(client, index);
                    let value = value_of(client, index);

                    match raw.put(&key, &value) {
                        Ok(()) => acked.push(index),
                        Err(error) => failures.lock().unwrap().push(Failure {
                            operation: "put",
                            error,
                        }),
                    }
                    operations.fetch_add(1, Ordering::Relaxed);

                    // A read of something this client just wrote, so a mis-delivered response
                    // is caught immediately rather than only in the final sweep.
                    if let Some(written) = acked.last().copied() {
                        let key = key_of(client, written);
                        match raw.get(&key) {
                            Ok(found) => assert_eq!(
                                found.as_deref(),
                                Some(&value_of(client, written)[..]),
                                "client {client} read back the wrong value for its own write"
                            ),
                            Err(error) => failures.lock().unwrap().push(Failure {
                                operation: "get",
                                error,
                            }),
                        }
                        operations.fetch_add(1, Ordering::Relaxed);
                    }

                    // A scan every so often, so the range path is under load too.
                    if index % 16 == 0 {
                        let start = key_of(client, 0);
                        if let Err(error) = raw.scan(&start, b"", 32) {
                            failures.lock().unwrap().push(Failure {
                                operation: "scan",
                                error,
                            });
                        }
                        operations.fetch_add(1, Ordering::Relaxed);
                    }

                    index += 1;
                }
                (client, acked)
            })
        })
        .collect();

    let mut acked_by_client: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for worker in workers {
        let (client, acked) = worker.join().expect("no client panicked");
        acked_by_client.insert(client, acked);
    }
    stop.store(true, Ordering::Relaxed);

    Outcome {
        acked_by_client,
        failures: Arc::try_unwrap(failures)
            .expect("every worker is done")
            .into_inner()
            .unwrap(),
        operations: operations.load(Ordering::Relaxed),
    }
}

fn run_load(seconds: u64) {
    let harness = start();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let Outcome {
        acked_by_client,
        failures,
        operations,
    } = drive(harness.addr, deadline);

    // -- what the store said -------------------------------------------------------------

    let busy = failures.iter().filter(|f| f.is_server_busy()).count();
    let other: Vec<&Failure> = failures.iter().filter(|f| !f.is_server_busy()).collect();

    let stalls = harness
        .store
        .property("esker.write-stalls")
        .unwrap_or_else(|| "?".to_owned());
    let slowdowns = harness
        .store
        .property("esker.write-slowdowns")
        .unwrap_or_else(|| "?".to_owned());

    assert!(
        other.is_empty(),
        "{} of {operations} operations failed with something other than ServerIsBusy: {:?}",
        other.len(),
        other
            .iter()
            .map(|f| (f.operation, f.error.to_string()))
            .collect::<Vec<_>>()
    );

    // `ServerIsBusy` is legitimate only if something was actually congested. In this phase the
    // only source is the transport's in-flight bound; the engine stalls by blocking, and has
    // no path to a wire error yet. Either way it has to be accounted for, not shrugged at.
    if busy > 0 {
        let stalled: u64 = stalls.parse().unwrap_or(0);
        let slowed: u64 = slowdowns.parse().unwrap_or(0);
        assert!(
            stalled > 0 || slowed > 0,
            "the store answered ServerIsBusy {busy} times with no engine stall \
             ({stalls} stalls, {slowdowns} slowdowns) — that is a bug, not backpressure"
        );
    }

    // -- and whether it told the truth ---------------------------------------------------

    let reader = client_for(harness.addr);
    let mut verified = 0u64;
    for (client, acked) in &acked_by_client {
        for index in acked {
            let key = key_of(*client, *index);
            let found = reader
                .get(&key)
                .unwrap_or_else(|err| panic!("reading back client {client} write {index}: {err}"));
            assert_eq!(
                found.as_deref(),
                Some(&value_of(*client, *index)[..]),
                "client {client}'s acknowledged write {index} is not there, or is not its own"
            );
            verified += 1;
        }
    }

    assert!(
        verified > 0,
        "no client acknowledged a single write, so the run proved nothing"
    );
    println!(
        "load: {CLIENTS} clients, {seconds}s, {operations} operations, {verified} acknowledged \
         writes verified, {busy} ServerIsBusy, {stalls} engine stalls, {slowdowns} slowdowns"
    );
}

/// Sixty-four clients, and nothing but `ServerIsBusy` allowed.
#[test]
fn sixty_four_clients_see_no_error_the_store_cannot_justify() {
    run_load(SECONDS);
}

/// The prompt's full minute.
#[test]
#[ignore = "the acceptance run; a full minute of load"]
fn the_full_load_test() {
    run_load(SECONDS_IGNORED);
}
