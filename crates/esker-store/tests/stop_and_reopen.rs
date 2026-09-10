//! `stop()`, `drop`, open again — a hundred times, and never `InUse`.
//!
//! This is what a store restarting in place does, and what half the tests in this crate do between
//! one arrangement and the next. It began failing on 2026-09-10 with
//! `Engine(InUse { dir: … })` at `schema_fetch.rs:110` — in a gate whose only other change was in
//! another crate, at 0.026 s.
//!
//! # What the failure is, and what it is not
//!
//! `Store::stop` aborts the tasks it spawned, and `JoinHandle::abort` is a **request**: the
//! runtime drops the task's future — and everything it holds — when it next gets to it. So a
//! store's database can outlive the `stop()` that closed it by however long the runtime takes, and
//! with the directory claim of #116 that stopped being invisible.
//!
//! **It did not reproduce.** A hundred rounds under a deliberately starved runtime, in two
//! arrangements — a replicated store with per-region tickers, and the one this file kept: a store
//! with a fake driver and `schema_fetch`'s five-millisecond heartbeat, which is the shape that
//! actually failed — stayed green both with and without a fix aimed at the tasks. A mechanism
//! that cannot be cornered is not a mechanism that can be fixed with confidence, so the fix is
//! not there: [`esker_engine`]'s `claim` **waits** for a directory somebody is still letting go
//! of, and refuses only after five seconds. Waiting cannot admit two live writers, because a live
//! writer never lets go.
//!
//! So this file is a regression test for the *property* — a reopen after a stop always works —
//! rather than for a race it could not make happen. Its load is kept because the failure came
//! from a loaded gate and a green run under load is worth more than a green run on an idle box.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use esker_store::pd::{FakePd, PdClient};
use esker_store::server::{Store, StoreOptions};
use tempfile::TempDir;

const ROUNDS: usize = 100;

/// One store on `dir`, with a driver to beat at and the cadence `schema_fetch.rs` uses.
fn open(dir: &TempDir) -> Result<Arc<Store>, esker_store::error::StoreError> {
    Store::open(
        dir.path(),
        StoreOptions {
            store_id: 1,
            peer_id: 1,
            region_id: 1,
            pd: Some(Arc::new(FakePd::new()) as Arc<dyn PdClient>),
            address: "127.0.0.1:1".to_owned(),
            heartbeat_tick: Duration::from_millis(5),
            store_heartbeat: Duration::from_millis(20),
            region_heartbeat: Duration::from_millis(20),
            ..StoreOptions::new()
        },
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_store_leaves_its_directory_free_every_time() {
    let dir = TempDir::new().unwrap();

    // **Under load, because that is the arrangement the failure came from.** The gate that found
    // this was running four thousand tests; what matters about that is not the count but that the
    // runtime's workers were busy, so a task asked to stop was not stopped for a while. These
    // occupy the workers by blocking in them on purpose.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut load = Vec::new();
    for _ in 0..6 {
        let stop = Arc::clone(&stop);
        load.push(tokio::spawn(async move {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }));
    }
    // The rounds run on the blocking pool, which is where a synchronous `stop()` belongs and
    // where the real binary's shutdown runs — so the starved workers above are the *other*
    // threads, exactly as they are in a gate.
    let rounds = tokio::task::spawn_blocking(move || {
        for round in 0..ROUNDS {
            // The open is the assertion: it is the one that finds the last round's claim if the
            // last round's tasks are still holding it.
            let store = open(&dir).unwrap_or_else(|error| {
            panic!(
                "round {round} could not open the directory the round before it had stopped and \
                 dropped: {error}. `stop()` returned while something still held the database."
            )
        });
            store.stop();
            drop(store);
        }
    })
    .await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for task in load {
        let _ = task.await;
    }
    rounds.expect("the rounds ran");
}
