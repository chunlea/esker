//! **A read that meets a lock still inside its lease sleeps out the lease** — debt #115.
//!
//! The wait is here, in the client, and not in the SQL node: `Transaction::resolve` classifies the
//! lock, finds its owner still inside its lease, and sleeps — on the last attempt for the whole
//! remaining lease (`crates/esker-client/src/txn.rs`, the `Classified::Alive` arm). Nothing in this
//! crate knows what a `statement_timeout` is: the deadline that bounds a wait is checked in
//! `esker-sql`'s `wait_for_the_lock`, which is the **write** path's row-lock loop, reached through
//! `Txn::lock`. A `SELECT` never goes there.
//!
//! So a statement that meets a live lock waits for the lease however long the lease is, and a user
//! who asked for ten seconds gets the lease instead. This test pins the client half of that, which
//! is the half that can be shown without a cluster and without waiting: the clock is a fake and
//! records what it was asked to sleep.
//!
//! **What this does not show** is the user-visible half — that `statement_timeout` is accepted and
//! then not honoured. That needs a SQL session and a bounded thread, and it belongs with the fix
//! rather than in the gate, because a test that hangs to prove a hang is a test that hangs.
//!
//! # This test passes today, and the day it fails is the day it has done its job
//!
//! It asserts the behaviour that **exists**, not the behaviour that is wanted: a read meeting a
//! live lock sleeps the remaining lease. So it is green while #115 is open and goes **red when
//! #115 is fixed** — and that red is the signal, not a regression. Whoever makes it fail should
//! update this test to the new bound, or delete it with the debt; what they should not do is
//! weaken it back to green, because then nothing in the suite would notice this wait again.
//!
//! It is deliberately **not** `#[ignore]`d. It runs in a second, needs no cluster, and the one
//! moment it is worth hearing from is the moment somebody changes this path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_client::clock::FakeClock;
use esker_client::region_cache::{Route, StaticRegion};
use esker_client::router::{ClientOptions, Router};
use esker_client::testing::{FakeTransport, Matcher, Outcome, Rule};
use esker_client::wire::{Epoch, LockInfo, Method, Peer, Region};
use esker_client::{CountingOracle, Error, TxnClient};

/// A timestamp shaped the way the oracle mints them, so that a lease means something: it is judged
/// in the physical half alone.
const fn at_ms(physical_ms: u64) -> u64 {
    physical_ms << esker_client::TSO_LOGICAL_BITS
}

/// The lock's own lease. A day, which is the shape that made a scan never return.
const LEASE_MS: u64 = 24 * 60 * 60 * 1000;

/// Far beyond any statement timeout a user would set, and the point of the assertion: the client
/// does not sleep "a while", it sleeps the lease.
const AN_HOUR_MS: u64 = 60 * 60 * 1000;

fn one_region() -> Arc<StaticRegion> {
    Arc::new(StaticRegion::new(Route {
        region: Region {
            id: 1,
            start_key: Bytes::new(),
            end_key: Bytes::new(),
            epoch: Epoch {
                conf_ver: 1,
                version: 1,
            },
            peers: vec![Peer::voter(1, 1)],
        },
        leader: Some(Peer::voter(1, 1)),
    }))
}

/// The lock a store reports for a key a live transaction holds.
fn a_live_lock() -> LockInfo {
    LockInfo {
        start_ts: at_ms(1_000),
        ttl_ms: LEASE_MS,
        key: Bytes::from_static(b"k"),
        primary: Bytes::from_static(b"k"),
    }
}

/// **The read sleeps the lease out, and the budget is what ends it — not any deadline.**
///
/// One resolution attempt, so the first look is also the last and the sleep is the whole remaining
/// lease. The read then fails with `LockNotCleared`, which is the honest outcome for a lock that
/// never cleared; what this test is about is the **hour** the client spent before saying so.
#[test]
fn a_read_that_meets_a_live_lock_sleeps_out_its_lease() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(
        Rule::new(
            Matcher::Method(Method::TxnGet),
            Outcome::locked(&a_live_lock()),
        )
        .forever(),
    );
    let clock = Arc::new(FakeClock::new());
    let options = ClientOptions {
        jitter_seed: Some(7),
        ..ClientOptions::default()
    };
    let router = Router::with_options(
        Arc::clone(&transport) as _,
        one_region() as Arc<dyn esker_client::RegionResolver>,
        options,
    )
    .with_clock(Arc::clone(&clock) as _);
    let client = TxnClient::on_router(
        Arc::new(router),
        Arc::new(CountingOracle::starting_at(at_ms(1_000))),
    )
    .with_max_lock_resolutions(1);

    let txn = client.begin().unwrap();
    let answer = txn.get(b"k");

    assert!(
        matches!(answer, Err(Error::LockNotCleared { .. })),
        "a lock that never clears should end the read with LockNotCleared: {answer:?}"
    );

    let slept = clock.sleeps_ms();
    let longest = slept.iter().copied().max().unwrap_or(0);
    // **Printed on the way past, not only on failure.** What this test is worth is the number it
    // saw; a green run that says nothing leaves the next reader to take the lease on trust.
    println!(
        "#115: one live lock, one read — the client slept {slept:?} ms, longest {longest} ms \
         against a lease of {LEASE_MS} ms"
    );
    assert!(
        longest > AN_HOUR_MS,
        "#115: a read that met a lock inside its lease should have slept the lease out, which is \
         far more than an hour — it slept {slept:?}. If this assertion is what fails, the wait has \
         been bounded by something and the debt may be paid; if the read returns without sleeping \
         at all, the fixture stopped reaching the resolution path and the test is measuring \
         nothing."
    );
}
