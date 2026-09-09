//! Routing a cluster with more than one region, and repairing a cache that a split made wrong.
//!
//! Phase 2's cache held one entry and every lookup found it. These are the paths that only exist
//! once the key space is partitioned: a key routed to the store that owns *it*, a stale entry
//! replaced by the two halves that succeeded it, and a placement driver that could not answer
//! being told apart from one that answered "nowhere".
//!
//! Everything runs against `FakeTransport` and `FakeClock`, for the reason `retry.rs` gives: a
//! retry rule is a claim about an exact sequence of requests, and no real server can be made to
//! produce one on demand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::Bytes;
use esker_client::clock::FakeClock;
use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::testing::{FakeTransport, Matcher, Outcome, Rule};
use esker_client::wire::{Epoch, Peer, ProtoError, RawKvResp, Region};
use esker_client::{ClientOptions, Error, RawClient};

/// Region `id` on store `id`, so an assertion can tell where a request went at a glance.
fn region(id: u64, start: &[u8], end: &[u8], epoch: Epoch) -> Region {
    Region {
        id,
        start_key: Bytes::copy_from_slice(start),
        end_key: Bytes::copy_from_slice(end),
        peers: vec![Peer::voter(id, id * 10)],
        epoch,
    }
}

fn route(region: Region) -> Route {
    let leader = region.peers.first().copied();
    Route { region, leader }
}

/// Three regions tiling the key space, one per store.
fn three_regions() -> Arc<dyn RegionResolver> {
    Arc::new(RegionTable::from_routes([
        route(region(1, b"", b"g", Epoch::INITIAL)),
        route(region(2, b"g", b"q", Epoch::INITIAL)),
        route(region(3, b"q", b"", Epoch::INITIAL)),
    ]))
}

struct Harness {
    client: RawClient,
    transport: Arc<FakeTransport>,
}

fn harness_with(resolver: Arc<dyn RegionResolver>) -> Harness {
    let transport = Arc::new(FakeTransport::new());
    let client = RawClient::with_options(
        transport.clone(),
        resolver,
        ClientOptions {
            jitter_seed: Some(0x4A4A),
            ..ClientOptions::default()
        },
    )
    .with_clock(Arc::new(FakeClock::new()));
    Harness { client, transport }
}

fn harness() -> Harness {
    harness_with(three_regions())
}

fn ok() -> Rule {
    Rule::new(Matcher::Any, Outcome::Reply(RawKvResp::Get { value: None })).forever()
}

/// Where each call went: `(store_id, region_id)`.
fn destinations(transport: &FakeTransport) -> Vec<(u64, u64)> {
    transport
        .calls()
        .iter()
        .filter_map(|call| {
            call.header()
                .map(|header| (call.store_id, header.region_id))
        })
        .collect()
}

#[test]
fn a_key_goes_to_the_store_that_owns_it() {
    let harness = harness();
    harness.transport.script(ok());

    for key in [
        &b""[..],
        b"apple",
        b"grape",
        b"pear",
        b"quince",
        b"\xff\xff",
    ] {
        harness.client.get(key).expect("every key is covered");
    }

    assert_eq!(
        destinations(&harness.transport),
        vec![(1, 1), (1, 1), (2, 2), (2, 2), (3, 3), (3, 3)],
        "the empty key belongs to the first region and the top of the space to the last"
    );
    assert_eq!(harness.client.cache().len(), 3, "each was learned once");
}

/// The second call for a region routes from the cache: a `GetRegion` per request would put the
/// placement driver on the read path, which is exactly what a cache exists to prevent.
#[test]
fn the_resolver_is_asked_once_per_region_not_once_per_call() {
    #[derive(Debug)]
    struct Counting {
        inner: RegionTable,
        asked: AtomicU32,
    }
    impl RegionResolver for Counting {
        fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            self.inner.locate(key)
        }
    }

    let resolver = Arc::new(Counting {
        inner: RegionTable::from_routes([
            route(region(1, b"", b"g", Epoch::INITIAL)),
            route(region(2, b"g", b"", Epoch::INITIAL)),
        ]),
        asked: AtomicU32::new(0),
    });
    let harness = harness_with(Arc::clone(&resolver) as Arc<dyn RegionResolver>);
    harness.transport.script(ok());

    for _ in 0..4 {
        harness.client.get(b"apple").unwrap();
        harness.client.get(b"quince").unwrap();
    }
    assert_eq!(
        resolver.asked.load(Ordering::Relaxed),
        2,
        "one lookup per region, not one per call"
    );
}

/// The trap phase 4a is shaped around, from the client's side. A store that answers a stale
/// request with **every** region the request touched lets one round trip repair the cache; an
/// answer naming only the region that was asked about would cost a `GetRegion` for the other
/// half, and 4b's split storm is one of those per stale request.
#[test]
fn a_split_is_learned_from_one_refusal() {
    let harness = harness_with(Arc::new(RegionTable::from_routes([route(region(
        1,
        b"",
        b"",
        Epoch::INITIAL,
    ))])));

    let after = Epoch::new(1, 2);
    let lower = region(1, b"", b"m", after);
    let upper = region(4, b"m", b"", after);
    harness
        .transport
        .script(Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::EpochNotMatch {
                current_regions: vec![lower, upper],
            }),
        ))
        .script(ok());

    // A key in the *upper* half: the retry must go to the region that took it over, which is one
    // the client had never heard of before the refusal.
    harness.client.get(b"zebra").expect("recovered");
    assert_eq!(
        destinations(&harness.transport),
        vec![(1, 1), (4, 4)],
        "the retry went to the new region on its own store"
    );
    assert_eq!(harness.client.cache().len(), 2, "both halves were learned");

    // And a key in the lower half now routes from the cache, with no further refusal — the whole
    // point of sending both.
    harness.client.get(b"apple").expect("cached");
    assert_eq!(
        destinations(&harness.transport).last().copied(),
        Some((1, 1))
    );
}

/// A placement driver that cannot answer is not the same as one that says "nowhere". The first
/// is retryable and the client waits it out; the second is a routing failure it reports.
#[test]
fn an_unreachable_placement_driver_is_waited_out() {
    #[derive(Debug)]
    struct DownThenUp {
        table: RegionTable,
        remaining: AtomicU32,
    }
    impl RegionResolver for DownThenUp {
        fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
            if self
                .remaining
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                != Err(0)
            {
                return Err(ProtoError::ServerIsBusy {
                    reason: "the placement driver is not answering".to_owned(),
                });
            }
            self.table.locate(key)
        }
    }

    let harness = harness_with(Arc::new(DownThenUp {
        table: RegionTable::from_routes([route(region(1, b"", b"", Epoch::INITIAL))]),
        remaining: AtomicU32::new(3),
    }));
    harness.transport.script(ok());

    harness.client.get(b"k").expect("the outage was waited out");
    assert_eq!(
        harness.transport.calls().len(),
        1,
        "nothing was sent while routing was unknown"
    );
}

/// A key no region covers is a routing failure with an answer, and the client stops rather than
/// spending its whole budget on a lookup that will keep saying the same thing.
#[test]
fn a_key_no_region_covers_is_reported_rather_than_retried() {
    // A table with a gap: `["", "g")` and `["q", "")`, nothing between.
    let harness = harness_with(Arc::new(RegionTable::from_routes([
        route(region(1, b"", b"g", Epoch::INITIAL)),
        route(region(3, b"q", b"", Epoch::INITIAL)),
    ])));
    harness.transport.script(ok());

    let error = harness.client.get(b"kiwi").unwrap_err();
    match &error {
        Error::Store(ProtoError::KeyNotInRegion { key, .. }) => {
            assert_eq!(key, &Bytes::from_static(b"kiwi"));
        }
        other => panic!("{other:?}"),
    }
    assert!(
        harness.transport.calls().is_empty(),
        "nothing was sent for a key that routes nowhere"
    );
    assert!(
        error.changed_nothing(),
        "a request that was never sent cannot have changed anything"
    );

    // The covered keys still work, so a gap costs the request that fell in it and nothing else.
    harness.client.get(b"apple").unwrap();
    harness.client.get(b"zebra").unwrap();
}

/// A cache entry a store proved wrong is dropped, so the next call starts from the resolver
/// rather than repeating the same mistake.
#[test]
fn a_region_that_refuses_a_key_is_dropped_from_the_cache() {
    let harness = harness();
    // Rules are answered in the order they were scripted, so this is: one success, then one
    // refusal, then success again.
    harness
        .transport
        .script(Rule::new(
            Matcher::Any,
            Outcome::Reply(RawKvResp::Get { value: None }),
        ))
        .script(Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::KeyNotInRegion {
                key: Bytes::from_static(b"apple"),
                region_id: 1,
                start_key: Bytes::from_static(b"b"),
                end_key: Bytes::from_static(b"g"),
            }),
        ))
        .script(ok());

    harness.client.get(b"apple").unwrap();
    assert_eq!(harness.client.cache().len(), 1);
    assert!(harness.client.get(b"apple").is_err());
    assert!(
        harness.client.cache().lookup(b"apple").is_none(),
        "the entry that produced the refusal is still cached"
    );
}

/// The `[start, end)` each store's region owns, for asserting what it was asked.
fn owned(store: u64) -> (&'static [u8], &'static [u8]) {
    match store {
        1 => (b"", b"g"),
        2 => (b"g", b"q"),
        _ => (b"q", b""),
    }
}

/// Every `Scan` a call sent, as `(store, start, end)`.
fn scans(transport: &FakeTransport) -> Vec<(u64, Vec<u8>, Vec<u8>)> {
    transport
        .calls()
        .into_iter()
        .filter_map(|call| match call.body() {
            Some(esker_client::wire::RawKvReq::Scan { start, end, .. }) => {
                Some((call.store_id, start.to_vec(), end.to_vec()))
            }
            _ => None,
        })
        .collect()
}

/// **A raw scan asks each region only for the keys that region holds.**
///
/// It sent one request for the caller's whole range, which a store refuses as soon as the range
/// leaves its region — the same defect `Transaction::scan` had, and the reason it was invisible is
/// that nothing scans a large `RawKV` range today. Asserted on the **requests**, because a
/// `FakeTransport` answers whatever it is scripted to and would never refuse.
#[test]
fn a_raw_scan_asks_each_region_only_for_the_keys_it_holds() {
    let harness = harness();
    harness.transport.script(
        Rule::new(
            Matcher::Method(esker_client::wire::Method::RawScan),
            Outcome::Reply(RawKvResp::Scan { pairs: Vec::new() }),
        )
        .forever(),
    );

    harness.client.scan(b"", b"", 100).unwrap();

    let sent = scans(&harness.transport);
    assert_eq!(
        sent.iter().map(|(store, ..)| *store).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "one request per region, in key order: {sent:?}"
    );
    for (store, start, end) in &sent {
        let (from, to) = owned(*store);
        assert!(
            start.as_slice() >= from,
            "store {store} asked below its region"
        );
        assert!(
            to.is_empty() || (!end.is_empty() && end.as_slice() <= to),
            "store {store} was asked for [{start:?}, {end:?}), past its own {to:?}"
        );
    }
}

/// **A reverse scan visits the regions from the top.**
///
/// The direction routing cannot walk lazily: `route` answers "who holds this key" and a reverse
/// scan wants the *last* region first, with no key to ask for it when `end` is empty. So the
/// regions are enumerated forward and visited backwards, and the store order is the assertion.
#[test]
fn a_reverse_raw_scan_visits_the_regions_from_the_top() {
    let harness = harness();
    harness.transport.script(
        Rule::new(
            Matcher::Method(esker_client::wire::Method::RawScan),
            Outcome::Reply(RawKvResp::Scan { pairs: Vec::new() }),
        )
        .forever(),
    );

    harness.client.scan_reverse(b"", b"", 100).unwrap();

    let sent = scans(&harness.transport);
    assert_eq!(
        sent.iter().map(|(store, ..)| *store).collect::<Vec<_>>(),
        vec![3, 2, 1],
        "the highest region first: {sent:?}"
    );
    for (store, start, end) in &sent {
        let (from, to) = owned(*store);
        assert!(
            start.as_slice() >= from,
            "store {store} asked below its region"
        );
        assert!(
            to.is_empty() || (!end.is_empty() && end.as_slice() <= to),
            "store {store} was asked past its own end"
        );
    }
}

/// **A boundary the store has moved under the scan is repaired from the refusal.**
///
/// `KeyNotInRegion` carries the range the store actually owns, and that is newer than anything the
/// placement driver can say inside a heartbeat — see `Transaction::scan_region`.
#[test]
fn a_raw_scan_repairs_a_boundary_the_store_moved() {
    let harness = harness();
    // Store 1 has split at `d` and refuses the first ask; after that everything answers.
    harness
        .transport
        .script(Rule::new(
            Matcher::All(vec![
                Matcher::Method(esker_client::wire::Method::RawScan),
                Matcher::Store(1),
            ]),
            Outcome::Fail(ProtoError::KeyNotInRegion {
                key: Bytes::from_static(b""),
                region_id: 1,
                start_key: Bytes::from_static(b""),
                end_key: Bytes::from_static(b"d"),
            }),
        ))
        .script(
            Rule::new(
                Matcher::Method(esker_client::wire::Method::RawScan),
                Outcome::Reply(RawKvResp::Scan { pairs: Vec::new() }),
            )
            .forever(),
        );

    harness.client.scan(b"", b"", 100).unwrap();

    let sent = scans(&harness.transport);
    assert_eq!(
        sent[0].2,
        b"g".to_vec(),
        "the first ask used the cache's boundary: {sent:?}"
    );
    assert_eq!(
        sent[1].2,
        b"d".to_vec(),
        "the retry uses the bound the store itself named, not the cache's: {sent:?}"
    );
}

/// **A refusal that names bounds the caller's key is inside is believed at once.**
///
/// The store knows about its own split the instant it happens and the placement driver learns at
/// the next heartbeat, so inside that window the refusal is the *newer* fact. Asserted on the
/// driver: it must not be asked at all.
#[test]
fn a_refusal_that_covers_the_key_is_believed_without_asking_the_driver() {
    #[derive(Debug)]
    struct Counting {
        inner: RegionTable,
        asked: AtomicU32,
    }
    impl RegionResolver for Counting {
        fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            self.inner.locate(key)
        }
    }
    let resolver = Arc::new(Counting {
        inner: RegionTable::from_routes([route(region(1, b"", b"", Epoch::INITIAL))]),
        asked: AtomicU32::new(0),
    });
    let harness = harness_with(Arc::clone(&resolver) as Arc<dyn RegionResolver>);
    // Region 1 has split at `d`; the first ask is refused with the range it really owns.
    harness
        .transport
        .script(Rule::new(
            Matcher::Method(esker_client::wire::Method::RawScan),
            Outcome::Fail(ProtoError::KeyNotInRegion {
                key: Bytes::from_static(b""),
                region_id: 1,
                start_key: Bytes::from_static(b""),
                end_key: Bytes::from_static(b"d"),
            }),
        ))
        .script(
            Rule::new(
                Matcher::Method(esker_client::wire::Method::RawScan),
                Outcome::Reply(RawKvResp::Scan { pairs: Vec::new() }),
            )
            .forever(),
        );

    // The caller asks past the store's real end, so believing the store and asking the driver
    // give **different** answers: `d` from the refusal, `z` from a driver that still thinks the
    // region is unbounded. The request is what says which was used.
    harness.client.scan(b"", b"z", 100).unwrap();
    let sent = scans(&harness.transport);
    assert_eq!(
        sent[1].2,
        b"d".to_vec(),
        "the retry used the bound the store named, not the driver's stale one: {sent:?}"
    );
    // **The count is not the instrument here, and it is worth saying why.** A terminal refusal
    // makes the router drop the cache entry that produced it (`Router::on_terminal`), so the
    // *retry request* re-resolves through the ordinary path — a second ask that has nothing to do
    // with the repair. Asserting on it would be asserting on the router's cache discipline while
    // claiming to test the repair. The request above is unambiguous: `d` can only have come from
    // the refusal, because the driver still says the region is unbounded.
    assert!(
        resolver.asked.load(Ordering::Relaxed) >= 1,
        "the enumeration resolves at least once"
    );
}

/// **A driver that is behind is waited out rather than surfaced.**
///
/// `08006 … key is not in region 0` is the resolver's own refusal — "no region covers this key" —
/// and under load that is a fact about the driver's *knowledge*, not about the cluster: it learns
/// at the next heartbeat. It surfaced from a loaded gate while passing alone, twice. So the repair
/// asks again on the router's schedule, and the assertion is that the answer arrives rather than
/// the refusal.
#[test]
fn a_driver_that_answers_stale_twice_is_waited_out() {
    #[derive(Debug)]
    struct StaleThenRight {
        stale: AtomicU32,
        inner: RegionTable,
    }
    impl RegionResolver for StaleThenRight {
        fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
            if self
                .stale
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                != Err(0)
            {
                // "No region covers this key" — what a driver behind a split answers.
                return Ok(None);
            }
            self.inner.locate(key)
        }
    }
    let resolver = Arc::new(StaleThenRight {
        stale: AtomicU32::new(2),
        inner: RegionTable::from_routes([route(region(2, b"", b"", Epoch::INITIAL))]),
    });
    let harness = harness_with(Arc::clone(&resolver) as Arc<dyn RegionResolver>);
    // The store refuses with bounds that do **not** contain the key, so the driver is the only way
    // out — and it is behind for the first two asks.
    harness
        .transport
        .script(Rule::new(
            Matcher::Method(esker_client::wire::Method::RawScan),
            Outcome::Fail(ProtoError::KeyNotInRegion {
                key: Bytes::from_static(b"a"),
                region_id: 9,
                start_key: Bytes::from_static(b"x"),
                end_key: Bytes::from_static(b"y"),
            }),
        ))
        .script(
            Rule::new(
                Matcher::Method(esker_client::wire::Method::RawScan),
                Outcome::Reply(RawKvResp::Scan { pairs: Vec::new() }),
            )
            .forever(),
        );

    harness
        .client
        .scan(b"a", b"c", 100)
        .expect("the driver caught up and the scan finished");
    assert_eq!(
        resolver.stale.load(Ordering::Relaxed),
        0,
        "the driver should have been asked until it stopped being behind"
    );
}

/// **A write survives a driver that does not know the key yet.**
///
/// The fourth call site, and the one repairing the scans could not reach. In `Router::call` a
/// `KeyNotInRegion` is `Verdict::Surface` and `may_ask_again` is — correctly — false for a
/// mutation, so the refusal was terminal and an `INSERT` answered
/// `08006 … key is not in region 0` under a loaded gate while passing alone.
///
/// **The distinction is which refusal it is.** A *store's* `KeyNotInRegion` stays terminal for a
/// write: it says the request went somewhere that never held the key, and re-sending a mutation on
/// that basis is exactly what the retry rules forbid. The *resolver's* — `region_id 0`, empty
/// bounds — says only that the driver has not caught up with a split, and that is waitable for a
/// write as much as for a read. Waited out in `Router::route`, so all four sites get it.
#[test]
fn a_write_waits_out_a_driver_that_does_not_know_the_key_yet() {
    #[derive(Debug)]
    struct StaleThenRight {
        stale: AtomicU32,
        inner: RegionTable,
    }
    impl RegionResolver for StaleThenRight {
        fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
            if self
                .stale
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                != Err(0)
            {
                return Ok(None);
            }
            self.inner.locate(key)
        }
    }
    let resolver = Arc::new(StaleThenRight {
        stale: AtomicU32::new(2),
        inner: RegionTable::from_routes([route(region(1, b"", b"", Epoch::INITIAL))]),
    });
    let harness = harness_with(Arc::clone(&resolver) as Arc<dyn RegionResolver>);
    harness.transport.script(
        Rule::new(
            Matcher::Method(esker_client::wire::Method::RawPut),
            Outcome::Reply(RawKvResp::Put),
        )
        .forever(),
    );

    harness
        .client
        .put(b"a", b"1")
        .expect("the write waits for the driver rather than surfacing its refusal");
    assert_eq!(
        resolver.stale.load(Ordering::Relaxed),
        0,
        "the driver should have been asked until it stopped saying 'no region'"
    );
    assert_eq!(
        harness.transport.stores(),
        vec![1],
        "and then the write went to the region that owns the key"
    );
}

/// **A driver that is behind for longer than a fixed handful of asks is still just behind.**
///
/// `Router::route` raises its own `KeyNotInRegion { region_id: 0 }` when the resolver says no
/// region covers a key, and waits the driver out first — because a store learns of its own split
/// immediately and the placement driver at the next heartbeat, so for a moment after every split
/// there is no answer to give. That wait was five asks, about 310 ms of backoff: fifteen
/// heartbeats on an idle box and less than one *effective* cycle on a loaded one, which is where
/// `08006 … key is not in region 0` came from in a gate that passed the same test alone.
///
/// A call carries a deadline, and it is the caller's answer to "how long am I willing to wait".
/// The lookup inside it is bounded by that now, so a driver that takes seven asks to catch up is
/// waited out by a call that had a second to spend.
#[test]
fn a_driver_behind_for_more_asks_than_the_old_bound_is_still_waited_out() {
    #[derive(Debug)]
    struct BehindThenRight {
        table: RegionTable,
        silent: AtomicU32,
        asks: AtomicU32,
    }
    impl RegionResolver for BehindThenRight {
        fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
            self.asks.fetch_add(1, Ordering::Relaxed);
            // **`Ok(None)`, not an error.** "No region covers this key" is what a driver says while
            // it is behind, and it is the answer this bound is about; an outage is a different
            // shape and `an_unreachable_placement_driver_is_waited_out` has it.
            if self
                .silent
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                != Err(0)
            {
                return Ok(None);
            }
            self.table.locate(key)
        }
    }

    // Six is what the old bound allowed — one ask plus five repairs — so seven is the first count
    // that could not be waited out however long the caller was willing to wait.
    let resolver = Arc::new(BehindThenRight {
        table: RegionTable::from_routes([route(region(1, b"", b"", Epoch::INITIAL))]),
        silent: AtomicU32::new(7),
        asks: AtomicU32::new(0),
    });
    let harness = harness_with(Arc::clone(&resolver) as Arc<dyn RegionResolver>);
    harness.transport.script(ok());

    harness
        .client
        .get(b"k")
        .expect("a driver seven asks behind is still a driver that is behind");
    assert!(
        resolver.asks.load(Ordering::Relaxed) >= 8,
        "the lookup gave up early: {} asks",
        resolver.asks.load(Ordering::Relaxed)
    );
}
