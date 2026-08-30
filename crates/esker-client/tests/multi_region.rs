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
