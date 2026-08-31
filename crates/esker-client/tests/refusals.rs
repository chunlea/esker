//! A refusal is a promise, and this is the test that can catch it being broken.
//!
//! [`esker_client::Error::changed_nothing`] says the database is untouched. Three separate
//! places spend that promise rather than checking it:
//!
//! * `esker-client`'s router `debug_assert`s it before retrying — *"a retryable error must be
//!   one the store provably did not apply"* — because retrying a write that did apply is how a
//!   compare-and-swap loop double-applies.
//! * The transaction layer branches on it to decide whether a failed prewrite left a lock
//!   behind.
//! * `chaos_linearizability.rs` drops refused operations from the history it checks. That is
//!   what makes the linearizability search affordable, and it is sound *exactly* when a refusal
//!   is true. A refused write that landed leaves a later read no operation can explain, and the
//!   checker reports a lost write that was never lost — a false violation, which is the most
//!   expensive kind of wrong a test can be.
//!
//! So the promise gets its own test, at the layer that makes it, rather than being assumed by
//! all three.
//!
//! # How it is caught
//!
//! Every write goes to **a key nothing else will ever touch**, `refused-c<client>-w<sequence>`.
//! That is the whole trick: a refused write that landed cannot be hidden by a later write to the
//! same key, so the question stops being "what is this key's final value" — which only catches a
//! refusal that landed *last* — and becomes "does this key exist at all", which catches every
//! one. After the kills the cluster is settled and every refused key is read back. A key that
//! exists is a write the client was told had not happened.
//!
// The bug it was written for
//
// It failed when it was written, and the cause was not in this crate. `esker-store`'s
// `PeerCore::propose` records a proposal in `pending` only *after* the Raft node has appended
// it, so a pending proposal is an entry in the leader's log that a surviving majority may still
// commit. When the peer stopped, `driver.rs` failed those with `ProtoError::not_sent` — which
// `esker-proto` documents as *"a request that provably never left this process"* and maps to
// `RequestOutcome::NotApplied`, "safe to send again". It had left, and it had sometimes been
// applied: nine of thirty-two refused writes were in the database.
//
// Fixed in `e06acbf`: `fail_outstanding` now takes the reason and picks the outcome itself, so
// proposals get `Closed` (`Unknown`) and only the reads — a `ReadIndex` token, no entry, no
// effect — keep `NotSent`. This is the regression test, and it is not a narrow one: it asserts
// the promise rather than the shape of that particular mistake, so any other way of breaking it
// fails here too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;

#[path = "chaos_cluster/mod.rs"]
mod chaos_cluster;

use chaos_cluster::{Cluster, connect};

/// Concurrent writers. One in-flight write each, so a kill can strand at most this many.
const CLIENTS: usize = 6;
/// Leader kills, and the pause on either side of each one.
const KILLS: u32 = 4;
const BETWEEN: Duration = Duration::from_millis(400);

/// One write the client was told had not happened.
struct Refusal {
    /// The key only that write ever addressed.
    key: Vec<u8>,
    /// What it was refused with, so a failure names the error that lied.
    error: String,
}

#[test]
fn a_refused_write_is_never_in_the_database() {
    let cluster = Cluster::start(3);
    assert!(
        cluster.settle(Duration::from_secs(30)),
        "the cluster never elected a leader to begin with"
    );

    let stop = Arc::new(AtomicBool::new(false));
    let refusals: Arc<Mutex<Vec<Refusal>>> = Arc::new(Mutex::new(Vec::new()));
    let acked = Arc::new(AtomicU64::new(0));
    let addrs = cluster.addrs.clone();

    let writers: Vec<_> = (0..CLIENTS)
        .map(|at| {
            let (addrs, stop, refusals, acked) = (
                addrs.clone(),
                Arc::clone(&stop),
                Arc::clone(&refusals),
                Arc::clone(&acked),
            );
            std::thread::spawn(move || {
                let client_id = at as u64 + 1;
                let Some(mut client) = connect(&addrs, Instant::now() + Duration::from_secs(10))
                else {
                    return;
                };
                let mut sequence = 0_u64;
                while !stop.load(Ordering::Relaxed) {
                    sequence += 1;
                    let key = format!("refused-c{client_id}-w{sequence}").into_bytes();
                    let value = Bytes::from(key.clone());
                    match client.put(&key, &value) {
                        Ok(()) => {
                            acked.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => {
                            // Only the refusals. An ambiguous outcome is the client saying it
                            // does not know, which is not a claim and cannot be broken.
                            if error.changed_nothing() {
                                refusals.lock().unwrap().push(Refusal {
                                    key,
                                    error: format!("{error:?}"),
                                });
                            }
                            // `TcpStores` opens its connections once, so a client that lost the
                            // store it was talking to has to build a new book to find another.
                            if let Some(fresh) =
                                connect(&addrs, Instant::now() + Duration::from_secs(5))
                            {
                                client = fresh;
                            }
                        }
                    }
                }
            })
        })
        .collect();

    let mut killed = 0;
    for _ in 0..KILLS {
        std::thread::sleep(BETWEEN);
        let Some(at) = cluster.leader() else { continue };
        cluster.kill_node(at);
        killed += 1;
        std::thread::sleep(BETWEEN);
        cluster.start_node(at);
    }

    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        writer.join().unwrap();
    }
    assert!(
        cluster.settle(Duration::from_secs(30)),
        "the cluster never came back after {killed} kills"
    );

    let refusals = refusals.lock().unwrap();
    let acked = acked.load(Ordering::Relaxed);
    println!(
        "{killed} leader kills, {CLIENTS} writers: {acked} acknowledged writes, {} refused",
        refusals.len()
    );

    let landed = read_back(&addrs, &refusals);
    cluster.shutdown();

    assert!(
        killed > 0,
        "no leader was ever killed, so nothing was tested"
    );
    assert!(
        acked > 0,
        "not one write was acknowledged; the cluster never worked"
    );
    assert!(
        !refusals.is_empty(),
        "not one write was refused, so the promise was never made and nothing was checked"
    );
    assert!(
        landed.is_empty(),
        "{} of {} refused writes are in the database. The client was told the store provably \
         did not apply them — `Error::changed_nothing` — and it did. A caller that retries on \
         that promise double-applies, and `chaos_linearizability.rs` drops these operations \
         from the history it checks, so it will report a lost write that was never lost.\n{}",
        landed.len(),
        refusals.len(),
        landed.join("\n")
    );
}

/// Reads every refused key back, and returns a line for each one that is there.
///
/// Retried, because a settled cluster can still refuse one call while a connection is being
/// re-established; a read that never answered is *not* counted as absent, because "I could not
/// look" is not evidence that nothing is there.
fn read_back(addrs: &[std::net::SocketAddr], refusals: &[Refusal]) -> Vec<String> {
    let Some(client) = connect(addrs, Instant::now() + Duration::from_secs(20)) else {
        panic!("no store was reachable to read the refused keys back");
    };
    let mut landed = Vec::new();
    for refusal in refusals {
        let mut answer = None;
        for _ in 0..40 {
            match client.get(&refusal.key) {
                Ok(found) => {
                    answer = Some(found);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let Some(found) = answer else {
            panic!(
                "could not read {} back at all, so it is not known whether the refusal held",
                String::from_utf8_lossy(&refusal.key)
            );
        };
        if found.is_some() {
            landed.push(format!(
                "  {} is in the database, refused with {}",
                String::from_utf8_lossy(&refusal.key),
                refusal.error
            ));
        }
    }
    landed
}
