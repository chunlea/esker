//! A raw scan whose range outlived the region map it was planned against.
//!
//! `RawClient::scan` plans its pieces **once**, from the cache, before it sends anything
//! (`raw.rs::regions_of`) — and the cache is a hint (`docs/DESIGN.md` §10). Both tests here are
//! about what happens when the plan turns out to be wrong, and both were silent wrong answers
//! of exactly the species #79 exists to abolish: a caller handed part of a range with no way to
//! tell it was part.
//!
//! Against `FakeTransport`, for the reason `retry.rs` gives: these are claims about an exact
//! sequence of requests, and no real cluster can be made to produce one on demand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_client::clock::FakeClock;
use esker_client::region_cache::{RegionResolver, RegionTable, Route};
use esker_client::testing::{FakeTransport, Matcher, Outcome, Rule};
use esker_client::wire::{Epoch, Peer, ProtoError, RawKvReq, RawKvResp, Region};
use esker_client::{ClientOptions, Error, RawClient};

fn region(id: u64, start: &[u8], end: &[u8]) -> Region {
    Region {
        id,
        start_key: Bytes::copy_from_slice(start),
        end_key: Bytes::copy_from_slice(end),
        peers: vec![Peer::voter(id, id * 10)],
        epoch: Epoch::INITIAL,
    }
}

fn route(region: Region) -> Route {
    let leader = region.peers.first().copied();
    Route { region, leader }
}

fn client_over(resolver: Arc<dyn RegionResolver>) -> (RawClient, Arc<FakeTransport>) {
    let transport = Arc::new(FakeTransport::new());
    let client = RawClient::with_options(
        transport.clone(),
        resolver,
        ClientOptions {
            jitter_seed: Some(0x5151),
            ..ClientOptions::default()
        },
    )
    .with_clock(Arc::new(FakeClock::new()));
    (client, transport)
}

fn pair(key: &'static [u8], value: &'static [u8]) -> (Bytes, Bytes) {
    (Bytes::from_static(key), Bytes::from_static(value))
}

fn scan_reply(pairs: Vec<(Bytes, Bytes)>) -> Outcome {
    Outcome::Reply(RawKvResp::Scan { pairs })
}

/// What a store says when the range it was handed is not the range it owns.
fn not_in_region(key: &'static [u8], owns: (&'static [u8], &'static [u8])) -> Outcome {
    Outcome::Fail(ProtoError::KeyNotInRegion {
        key: Bytes::from_static(key),
        region_id: 1,
        start_key: Bytes::from_static(owns.0),
        end_key: Bytes::from_static(owns.1),
    })
}

/// The `start` field of every `Scan` the client sent, in order.
fn scan_starts(transport: &FakeTransport) -> Vec<Bytes> {
    transport
        .calls()
        .iter()
        .filter_map(|call| match call.body() {
            Some(RawKvReq::Scan { start, .. }) => Some(start.clone()),
            _ => None,
        })
        .collect()
}

/// **A split the plan did not have costs the top of the piece, silently.**
///
/// The cache says one region owns everything; the store has since split at `m` and says so. The
/// client believes the store — `scan_region` narrows the range it asks for — but it narrows a
/// *local* copy, so the piece the outer walk is still working on keeps the old, wider end. When
/// `[a, m)` runs out the walk reads the empty batch as "this piece is done" and steps to the next
/// piece, which starts at the **old** boundary. Nothing ever asks for `[m, z)`.
///
/// `Transaction::scan_region` does not have this hole: it answers the boundary it actually used
/// and its caller carries on from that (`txn.rs`, "Empty, so this region's share of the range is
/// done"). This is the raw path catching up with it.
#[test]
fn a_split_under_a_scan_does_not_lose_the_half_above_it() {
    let resolver = Arc::new(RegionTable::from_routes([route(region(1, b"", b""))]));
    let (client, transport) = client_over(resolver);

    transport.script_all([
        // The plan asks for the whole range; the store owns only up to `m` now.
        Rule::new(
            Matcher::Key(Bytes::from_static(b"a")),
            not_in_region(b"a", (b"", b"m")),
        ),
        Rule::new(
            Matcher::Key(Bytes::from_static(b"a")),
            scan_reply(vec![pair(b"b", b"1")]),
        ),
        // The resume is still sent with the old end, so it is refused once more.
        Rule::new(
            Matcher::Key(Bytes::from_static(b"b\0")),
            not_in_region(b"b\0", (b"", b"m")),
        ),
        Rule::new(Matcher::Key(Bytes::from_static(b"b\0")), scan_reply(vec![])),
        // The half above the split. Reached only by a walk that knows it is still owed it.
        Rule::new(
            Matcher::Key(Bytes::from_static(b"m")),
            scan_reply(vec![pair(b"n", b"2")]),
        )
        .forever(),
    ]);
    transport.unmatched(scan_reply(vec![]));

    let pairs = client.scan(b"a", b"z", 0).expect("the scan answers");

    assert!(
        scan_starts(&transport)
            .iter()
            .any(|start| start.as_ref() == b"m"),
        "the walk never asked for the half above the split: {:?}",
        scan_starts(&transport)
    );
    assert_eq!(
        pairs,
        vec![pair(b"b", b"1"), pair(b"n", b"2")],
        "a key above the split is a key the caller was never told about"
    );
}

/// **A range wider than the piece budget is refused, not quietly cut down to it.**
///
/// `regions_of` enumerates at most [`esker_client::MAX_SCAN_REGIONS`] pieces and used to simply
/// stop there, answering with a plan that covered a prefix of the range — so the scan returned
/// the keys of the first 64 regions and no sign that there were more. The transactional walk
/// calls the same bound "a scan crossed more than 64 regions" and raises it as an error, for the
/// reason its own header gives: exhausting a budget is not a short answer.
#[test]
fn a_range_past_the_region_budget_is_refused_and_not_cut() {
    // Seventy regions, one byte apart, so the range needs more pieces than the budget allows.
    let count = 70u8;
    let mut routes = Vec::new();
    for id in 1..=count {
        let start = vec![id];
        let end = if id == count {
            Vec::new()
        } else {
            vec![id + 1]
        };
        routes.push(route(region(u64::from(id), &start, &end)));
    }
    routes.insert(0, route(region(1000, b"", b"\x01")));
    let resolver = Arc::new(RegionTable::from_routes(routes));
    let (client, transport) = client_over(resolver);

    // Only the last region holds anything, so a plan that stops early loses it and says nothing.
    transport.script(
        Rule::new(
            Matcher::Key(Bytes::from_static(b"\x46")),
            scan_reply(vec![pair(b"\x46zz", b"last")]),
        )
        .forever(),
    );
    transport.unmatched(scan_reply(vec![]));

    let refusal = client
        .scan(b"\x01", b"", 0)
        .expect_err("a plan that cannot cover the range is not an answer");

    assert!(
        matches!(&refusal, Error::Internal(detail) if detail.contains("regions")),
        "want a loud failure naming the budget, got {refusal:?}"
    );
}
