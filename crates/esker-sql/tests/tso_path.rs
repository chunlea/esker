//! **Is the node's one connection to the driver a serialisation point?**
//!
//! Since #51 a SQL node takes every transaction timestamp from PD, through the `PdConn` it already
//! had for routing and the schema lease — **one connection for the whole process**. Every
//! transaction takes two timestamps, so on a busy node that is two driver round trips per
//! transaction through one socket behind one mutex.
//!
//! `esker durability record` measured the shape by accident while being fixed: **236 acknowledged
//! writes a second with a connection per writer, 36 with one between them.** That is a load
//! generator's plumbing, not a node's, and it raised the node's question rather than answering it.
//! This answers it.
//!
//! # The comparison is its own control
//!
//! Both arms talk to the **same** stand-in driver over the same framing, so anything the driver
//! itself cannot keep up with limits both equally. What differs is one connection against one per
//! session. If the shared arm flattens where the per-session arm keeps climbing, the connection is
//! the serialisation point; if both flatten together, the driver is, and the node's shape is not
//! the thing to change.
//!
//! Counts and rates only — no wall-clock claim survives a loaded host, and this file says so by
//! printing what it measured rather than asserting a number.
//!
//! ```text
//! cargo nextest run -p esker-sql --test tso_path --run-ignored all --no-capture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod standin_pd;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use esker_client::TimestampOracle;
use esker_sql::pd::PdConn;

/// How long each point on the curve runs. Short, because the question is a shape and not a number.
const FOR: Duration = Duration::from_millis(1_500);

/// One point: `sessions` threads taking timestamps for [`FOR`], from `oracles`.
///
/// `oracles[i % oracles.len()]` — so one oracle is the shared arm and `sessions` of them is the
/// per-session arm, with everything else identical.
fn point(sessions: usize, oracles: &[Arc<PdConn>]) -> (u64, u64) {
    let stop = Arc::new(AtomicBool::new(false));
    let taken = Arc::new(AtomicU64::new(0));
    let waited_micros = Arc::new(AtomicU64::new(0));

    let began = Instant::now();
    let mut threads = Vec::new();
    for id in 0..sessions {
        let oracle = Arc::clone(&oracles[id % oracles.len()]);
        let stop = Arc::clone(&stop);
        let taken = Arc::clone(&taken);
        let waited = Arc::clone(&waited_micros);
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // **Two per transaction**, which is what a write statement costs: a read timestamp
                // and a commit timestamp (`client/txn.rs:265` and `:1360`).
                let at = Instant::now();
                let first = oracle.timestamp();
                let second = oracle.timestamp();
                let cost = u64::try_from(at.elapsed().as_micros()).unwrap_or(u64::MAX);
                if first.is_ok() && second.is_ok() {
                    taken.fetch_add(1, Ordering::Relaxed);
                    waited.fetch_add(cost, Ordering::Relaxed);
                }
            }
        }));
    }
    std::thread::sleep(FOR);
    stop.store(true, Ordering::Relaxed);
    for thread in threads {
        let _ = thread.join();
    }

    let pairs = taken.load(Ordering::Relaxed);
    let elapsed = began.elapsed().as_micros().max(1);
    let per_second = u64::try_from(u128::from(pairs) * 1_000_000 / elapsed).unwrap_or(0);
    let mean_micros = waited_micros
        .load(Ordering::Relaxed)
        .checked_div(pairs)
        .unwrap_or(0);
    (per_second, mean_micros)
}

/// **The curve, both arms, printed rather than asserted.**
///
/// What is asserted is only what cannot be a matter of the host's mood: that both arms did work at
/// every point. The verdict is read off the shape and written up in `esker-coord/h1-tso-path.md`.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a measurement, not an assertion — see the module doc for how to run it"]
async fn what_one_connection_to_the_driver_costs_as_sessions_rise() {
    let (_pd, _handle, address) = standin_pd::serve().await;

    tokio::task::block_in_place(|| {
        let shared = vec![Arc::new(PdConn::new(address))];
        println!(
            "\n  sessions   shared: pairs/s  mean us   |   per session: pairs/s  mean us   ratio"
        );
        for sessions in [1usize, 4, 16, 64] {
            let (shared_rate, shared_us) = point(sessions, &shared);

            let each: Vec<Arc<PdConn>> = (0..sessions)
                .map(|_| Arc::new(PdConn::new(address)))
                .collect();
            let (each_rate, each_us) = point(sessions, &each);

            assert!(
                shared_rate > 0 && each_rate > 0,
                "a point did no work at {sessions} sessions, so it measured nothing"
            );
            let ratio = (each_rate * 100).checked_div(shared_rate).unwrap_or(0);
            println!(
                "  {sessions:>8}   {shared_rate:>15} {shared_us:>8}   |   {each_rate:>20} \
                 {each_us:>8}   {}.{}x",
                ratio / 100,
                (ratio % 100) / 10,
            );
        }
        println!();
    });
}
