//! The region census arrives on its cadence, from a store that is really running.
//!
//! The instrument exists to be read out of a real run's log after a cluster has stopped serving
//! (`esker-coord/h1-52-hypotheses.md`), so the thing worth pinning is not the shape of the struct —
//! it is that a **running store emits a line per region per period**, with the fields the six
//! candidate mechanisms are told apart by, and that it emits them on a *timer*: an instrument that
//! logged an election event would displace the race it was built to find.
//!
//! # Why these are `tokio` tests
//!
//! `Store::open` keeps `Handle::try_current().ok()` and every schedule it owns — the heartbeats,
//! the split checker, this census — is spawned on it. A store opened *outside* a runtime runs no
//! schedule at all, silently, which is what `Store::open`'s own doc means by "must be called from
//! inside a `tokio` runtime when `raft` or `pd` is set". This test found that the hard way: as a
//! plain `#[test]` it opened a store, hosted one region, took no census, and reported zero lines
//! with nothing wrong anywhere.
//!
//! # Why the subscriber is global
//!
//! The census runs on the store's own multi-threaded runtime, so a thread-local subscriber
//! (`set_default`) installed by the test thread would never see it — the test would pass on a
//! `Vec` that stayed empty because nothing was listening, which is the worst way for an
//! instrument's test to be wrong. `set_global_default` reaches every thread, and nextest gives
//! each test its own process, so one test in this file installs it and the rest assert on what it
//! caught.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use esker_store::PeerAddress;
use esker_store::server::{RaftOptions, Store, StoreOptions};
use tempfile::TempDir;

/// The census period this test drives, and the window it watches.
const EVERY: Duration = Duration::from_millis(100);
const WATCH_FOR: Duration = Duration::from_secs(1);

/// What a `WATCH_FOR` window of `EVERY` should produce for one region.
///
/// A band and not a number: the lower bound is what makes this a test of a cadence rather than of
/// a single line, and **the upper bound is what makes it a test of a cadence rather than of an
/// event stream** — a census that fired on every Raft tick or every election would blow through it.
const AT_LEAST: usize = 6;
const AT_MOST: usize = 16;

#[derive(Clone)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for Captured {
    type Writer = Self;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

/// One store, replicating region 1 with itself, with the census on.
///
/// One peer because the census is about what *a* peer believes and a single-peer group elects
/// itself in a tick — the same shape `esker cluster start` gives node 1 before the others join.
fn store_with_census(dir: &TempDir, every: Option<Duration>) -> Arc<Store> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut raft = RaftOptions::new(vec![PeerAddress::new(1, 1, address)], 20_260_910);
    raft.tick = Duration::from_millis(25);
    let options = StoreOptions {
        store_id: 1,
        peer_id: 1,
        region_id: 1,
        raft: Some(raft),
        region_census: every,
        ..StoreOptions::new()
    };
    Store::open(dir.path(), options).unwrap()
}

fn said(captured: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8(
        captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_store_takes_a_census_every_period_and_says_what_it_believes() {
    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(Captured(Arc::clone(&captured)))
        // Without this the fields arrive wrapped in escape codes and every assertion below fails
        // against a line that is perfectly correct.
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("one subscriber for this process");

    let dir = TempDir::new().unwrap();
    let store = store_with_census(&dir, Some(EVERY));
    tokio::time::sleep(WATCH_FOR).await;
    let seen = said(&captured);
    store.stop();

    let lines: Vec<&str> = seen
        .lines()
        .filter(|line| line.contains("region census"))
        .collect();
    assert!(
        lines.len() >= AT_LEAST,
        "a census every {EVERY:?} for {WATCH_FOR:?} produced {} lines, not {AT_LEAST} or more. \
         The whole log was:\n{seen}",
        lines.len()
    );
    assert!(
        lines.len() <= AT_MOST,
        "a census every {EVERY:?} for {WATCH_FOR:?} produced {} lines, which is more than a \
         cadence can explain — this instrument must never become one event per election. The \
         whole log was:\n{seen}",
        lines.len()
    );

    // **Every field the six candidates are told apart by**, on the line, in one place. A census
    // that emitted half of them would still pass a "did it fire" test, and would be useless on
    // the run it exists for.
    let last = lines.last().expect("a census line");
    for field in [
        "region=1",
        "handle_peer=1",
        "answered_by=1",
        "term=",
        "role=Leader",
        "is_leader=true",
        "believes_leader=1",
        "voted_for=1",
        "applied=",
        "commit=",
        "last_index=",
        "core_voters=[1]",
        "core_learners=[]",
        "record_peers=[1]",
        "campaigns_pre=",
        "campaigns_real=",
        "vote_responses_ignored=",
        "check_quorum_step_downs=",
    ] {
        assert!(
            last.contains(field),
            "the census line is missing `{field}`, so a reader of a stalled run could not use \
             it. The line was:\n{last}"
        );
    }

    // The counters are the half that needs the driver, and a line that reported them as absent
    // would be a line that could not see a term ladder.
    assert!(
        !last.contains("unanswered"),
        "the driver answered, so the line must not carry an `unanswered` reason:\n{last}"
    );
}

/// Off unless asked for, which is the other half of the switch being a switch.
///
/// Runs in its own process under nextest, so the global subscriber above is not installed here;
/// what it asserts is the absence of the ticker rather than the absence of a log line, and the
/// only way to see that from outside is that nothing is emitted at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_with_no_census_period_takes_none() {
    let captured = Arc::new(Mutex::new(Vec::<u8>::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(Captured(Arc::clone(&captured)))
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("one subscriber for this process");

    let dir = TempDir::new().unwrap();
    let store = store_with_census(&dir, None);
    tokio::time::sleep(WATCH_FOR).await;
    let seen = said(&captured);
    store.stop();

    assert!(
        !seen.contains("region census"),
        "a store with no census period took one anyway:\n{seen}"
    );
}
