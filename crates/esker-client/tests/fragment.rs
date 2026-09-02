//! The client's fragment path: where it sends one, what it does with a refusal, and what it will
//! not retry.
//!
//! `docs/plans/phase-10-routing.md` U1. Everything here runs against
//! [`esker_client::testing::FakeTransport`] and a [`FakeClock`], so the rules are asserted against
//! a script rather than against a cluster — including the two that cannot be produced on demand by
//! a real one: a store that refuses, and a region that splits mid-call.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use esker_client::clock::FakeClock;
use esker_client::fragment::{FragmentAnswer, FragmentClient, FragmentReq};
use esker_client::region_cache::{RegionTable, Route};
use esker_client::router::{ClientOptions, Router};
use esker_client::testing::{FakeTransport, Matcher, Outcome, Rule};
use esker_client::wire::{Epoch, Method, Peer, PeerRole, ProtoError, Region};
use esker_proto::fragment::{FragmentResp, RefusalReason, ScanStats};

/// A region `[start, end)` on three voters, with `columnar` on a store of its own when asked for.
fn region(id: u64, start: &[u8], end: &[u8], voters: &[u64], columnar: Option<u64>) -> Route {
    let mut peers: Vec<Peer> = voters.iter().map(|s| Peer::voter(*s, *s)).collect();
    if let Some(store) = columnar {
        peers.push(Peer {
            store_id: store,
            peer_id: store,
            role: PeerRole::ColumnarLearner,
        });
    }
    Route {
        region: Region {
            id,
            start_key: Bytes::copy_from_slice(start),
            end_key: Bytes::copy_from_slice(end),
            peers,
            epoch: Epoch::INITIAL,
        },
        // No opinion about the leader, which is the state a fresh cache is in — and the state in
        // which `Route::target()` answers with the *first voter*, so a fragment that went to
        // `target()` would go to a store holding rows.
        leader: None,
    }
}

/// A client over a routing table, with a clock that never really sleeps.
fn client(transport: &Arc<FakeTransport>, table: Vec<Route>) -> FragmentClient {
    let router = Router::with_options(
        Arc::clone(transport) as Arc<dyn esker_client::StoreTransport>,
        Arc::new(RegionTable::from_routes(table)),
        ClientOptions {
            jitter_seed: Some(7),
            call_timeout: Duration::from_secs(10),
            ..ClientOptions::default()
        },
    )
    .with_clock(Arc::new(FakeClock::new()));
    FragmentClient::new(Arc::new(router))
}

fn request() -> FragmentReq {
    FragmentReq {
        fragment: Bytes::from_static(b"opaque to this crate"),
        ts: 42,
        min_apply_index: 0,
    }
}

fn answered() -> Outcome {
    Outcome::FragmentReply(FragmentResp::Result {
        result: Bytes::from_static(b"an answer"),
        stats: ScanStats {
            stripes_considered: 4,
            stripes_read: 1,
            chunks_decoded: 2,
            rows_scanned: 100,
            rows_matched: 3,
        },
    })
}

/// The whole point of a fragment path of its own: it goes to the peer that holds *columns*.
///
/// `Router::call` would send this to `Route::target()`, which is the believed leader and, with no
/// leader believed, the first voter. Both are stores that hold rows and would answer
/// `NotColumnar`. Asserted on the store id **and** on the peer id in the header, because the store
/// checks the second.
#[test]
fn a_fragment_goes_to_the_columnar_learner_and_not_to_a_voter() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(Rule::new(Matcher::Any, answered()).forever());
    let client = client(&transport, vec![region(1, b"", b"", &[1, 2, 3], Some(9))]);

    let shards = client.shards(b"t", b"u").unwrap();
    assert_eq!(shards.len(), 1);
    let answer = client.evaluate(&shards[0], &request()).unwrap();

    assert!(matches!(answer, FragmentAnswer::Answered { .. }));
    assert_eq!(transport.stores(), vec![9], "a voter was asked for columns");
    assert_eq!(transport.peers(), vec![9]);
    assert_eq!(transport.methods(), vec![Method::FragmentEvaluate]);
}

/// `ScanStats` survives the round trip, because `EXPLAIN ANALYZE` is what it is for.
#[test]
fn the_answer_carries_what_it_cost() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(Rule::new(Matcher::Any, answered()).forever());
    let client = client(&transport, vec![region(1, b"", b"", &[1], Some(9))]);
    let shards = client.shards(b"t", b"u").unwrap();

    let FragmentAnswer::Answered { result, stats } =
        client.evaluate(&shards[0], &request()).unwrap()
    else {
        panic!("the fragment was refused");
    };
    assert_eq!(result, Bytes::from_static(b"an answer"));
    assert_eq!(stats.rows_scanned, 100);
    assert_eq!(stats.rows_matched, 3);
    assert_eq!(stats.stripes_read, 1);
}

/// A refusal is `Ok`, and it is not retried.
///
/// Both halves matter. If a refusal were an `Err` the planner would meet a normal fallback on its
/// error path; if it were retried, a rolling upgrade would cost every query its whole retry budget
/// before falling back.
#[test]
fn a_refusal_is_an_answer_and_is_asked_once() {
    for reason in [
        RefusalReason::Unsupported,
        RefusalReason::TooFarBehind,
        RefusalReason::NotColumnar,
    ] {
        let transport = Arc::new(FakeTransport::new());
        transport.script(
            Rule::new(
                Matcher::Any,
                Outcome::FragmentReply(FragmentResp::Refused {
                    reason,
                    detail: "for a human".to_owned(),
                }),
            )
            .forever(),
        );
        let client = client(&transport, vec![region(1, b"", b"", &[1], Some(9))]);
        let shards = client.shards(b"t", b"u").unwrap();

        let answer = client.evaluate(&shards[0], &request()).unwrap();
        assert_eq!(answer.refusal(), Some(reason));
        assert_eq!(transport.call_count(), 1, "{reason:?} was retried");
    }
}

/// A region with no columnar learner is answered without a round trip.
///
/// `NotColumnar` is precisely what "this region has no columnar copy" means, and the client
/// already knows it from the routing answer. Asking a voter to say so would be a wire round trip
/// spent on a fact the caller was holding.
#[test]
fn a_region_with_no_learner_is_refused_without_being_asked() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(Rule::new(Matcher::Any, answered()).forever());
    let client = client(&transport, vec![region(1, b"", b"", &[1, 2, 3], None)]);

    let shards = client.shards(b"t", b"u").unwrap();
    assert!(!shards[0].is_columnar());
    let answer = client.evaluate(&shards[0], &request()).unwrap();

    assert_eq!(answer.refusal(), Some(RefusalReason::NotColumnar));
    assert_eq!(transport.call_count(), 0, "a voter was asked anyway");
}

/// Every region of a range, in key order, each with its own learner.
///
/// The walk is `GetRegion` repeated, because that is the only routing question PD answers. A
/// region without a learner is still a shard: the planner needs to know it is there, so that it
/// can decline to route the *whole* query rather than silently answer about part of it.
#[test]
fn shards_walk_the_range_region_by_region() {
    let transport = Arc::new(FakeTransport::new());
    let client = client(
        &transport,
        vec![
            region(1, b"", b"d", &[1, 2, 3], Some(9)),
            region(2, b"d", b"m", &[1, 2, 3], None),
            region(3, b"m", b"", &[1, 2, 3], Some(8)),
        ],
    );

    let all = client.shards(b"", b"").unwrap();
    assert_eq!(
        all.iter().map(|s| s.region_id).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        all.iter()
            .map(|s| s.columnar.map(|p| p.store_id))
            .collect::<Vec<_>>(),
        vec![Some(9), None, Some(8)]
    );

    // A range inside one region stops there rather than walking the cluster.
    let one = client.shards(b"e", b"f").unwrap();
    assert_eq!(one.iter().map(|s| s.region_id).collect::<Vec<_>>(), vec![2]);

    // A range spanning two stops after the second.
    let two = client.shards(b"e", b"n").unwrap();
    assert_eq!(
        two.iter().map(|s| s.region_id).collect::<Vec<_>>(),
        vec![2, 3]
    );
}

/// A learner that joined **after** the cache was filled is still found.
///
/// The cache is a hint repaired by the refusals it causes, and a missing learner causes none: a
/// columnar replica joins through a conf change, so a client holding an entry from before it
/// joined would plan on rows for ever and never be told otherwise. `shards` asks the authority
/// once when the cached route lists no learner, which is the only case that needs it.
#[test]
fn a_learner_that_joined_after_the_cache_was_filled_is_found() {
    /// A resolver whose answer changes, as a placement driver's does.
    #[derive(Debug)]
    struct Moving {
        route: Mutex<Route>,
    }

    impl esker_client::RegionResolver for Moving {
        fn locate(&self, key: &[u8]) -> Result<Option<Route>, ProtoError> {
            let route = self.route.lock().unwrap().clone();
            Ok(route.region.contains(key).then_some(route))
        }
    }

    let transport = Arc::new(FakeTransport::new());
    transport.script(Rule::new(Matcher::Any, answered()).forever());
    let resolver = Arc::new(Moving {
        route: Mutex::new(region(1, b"", b"", &[1, 2, 3], None)),
    });
    let router = Arc::new(
        Router::with_options(
            Arc::clone(&transport) as Arc<dyn esker_client::StoreTransport>,
            Arc::clone(&resolver) as Arc<dyn esker_client::RegionResolver>,
            ClientOptions {
                jitter_seed: Some(3),
                ..ClientOptions::default()
            },
        )
        .with_clock(Arc::new(FakeClock::new())),
    );
    let client = FragmentClient::new(Arc::clone(&router));

    // The cache is filled while the region has no learner.
    assert!(!client.shards(b"t", b"u").unwrap()[0].is_columnar());
    assert_eq!(router.cache().len(), 1, "the cache was not filled");

    // PD places one. Nothing invalidates the cache, because nothing refused anything.
    *resolver.route.lock().unwrap() = region(1, b"", b"", &[1, 2, 3], Some(9));

    let shards = client.shards(b"t", b"u").unwrap();
    assert_eq!(
        shards[0].columnar.map(|peer| peer.store_id),
        Some(9),
        "the learner PD placed was never seen"
    );
    let answer = client.evaluate(&shards[0], &request()).unwrap();
    assert!(matches!(answer, FragmentAnswer::Answered { .. }));
    assert_eq!(transport.stores(), vec![9]);
}

/// The shard's own epoch goes on the wire, not whatever the cache believes at send time.
///
/// This is what makes a split safe: the store checks the epoch, so a fragment planned against a
/// region that has since split is refused rather than answered about the half that is left.
#[test]
fn the_header_carries_the_shard_epoch() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(Rule::new(Matcher::Any, answered()).forever());
    let mut route = region(7, b"", b"", &[1], Some(9));
    route.region.epoch = Epoch {
        conf_ver: 3,
        version: 5,
    };
    let client = client(&transport, vec![route]);

    let shards = client.shards(b"t", b"u").unwrap();
    client.evaluate(&shards[0], &request()).unwrap();

    let header = transport.nth_call(0).unwrap().header().unwrap();
    assert_eq!(header.region_id, 7);
    assert_eq!(
        header.epoch,
        Epoch {
            conf_ver: 3,
            version: 5
        }
    );
}

/// An epoch change surfaces instead of being retried, and repairs the cache on the way out.
///
/// **The one rule here that differs from `Router::call`, and the reason is a wrong answer rather
/// than a preference.** A fragment covers the whole of a region's columnar copy, so re-routing
/// into the region that replaced it asks about a *different set of rows* and returns a partial
/// answer that looks complete. The caller falls back to a row scan in the same snapshot instead.
#[test]
fn an_epoch_change_is_not_retried() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(
        Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::EpochNotMatch {
                current_regions: vec![
                    region(1, b"", b"g", &[1], Some(9)).region,
                    region(4, b"g", b"", &[1], Some(9)).region,
                ],
            }),
        )
        .forever(),
    );
    let client = client(&transport, vec![region(1, b"", b"", &[1], Some(9))]);
    let shards = client.shards(b"", b"").unwrap();

    let error = client.evaluate(&shards[0], &request()).unwrap_err();
    assert!(
        matches!(
            error,
            esker_client::Error::Store(ProtoError::EpochNotMatch { .. })
        ),
        "{error:?}"
    );
    assert_eq!(transport.call_count(), 1, "the split was chased");
    // Repaired anyway, so the *next* statement plans against what is there now.
    assert_eq!(
        client.router().cache().lookup(b"z").map(|r| r.region.id),
        Some(4)
    );
}

/// An answer that never came back is asked for again: a fragment is a read at a fixed `ts`, so
/// asking twice cannot change what the first attempt did.
///
/// `docs/plans/phase-9-rails.md` §8 measured both shapes of this on this very call — a deadline,
/// and a leader stepping down with the request in flight, which arrives as a closed connection.
#[test]
fn a_lost_answer_is_asked_again() {
    for error in [
        ProtoError::Timeout {
            detail: "no answer in 30s".to_owned(),
        },
        ProtoError::Closed {
            detail: "region 1 stopped leading with this proposal in its log".to_owned(),
        },
        ProtoError::ServerIsBusy {
            reason: "shedding".to_owned(),
        },
    ] {
        let transport = Arc::new(FakeTransport::new());
        transport.script_all([
            Rule::new(Matcher::Any, Outcome::Fail(error.clone())).times(2),
            Rule::new(Matcher::Any, answered()).forever(),
        ]);
        let client = client(&transport, vec![region(1, b"", b"", &[1], Some(9))]);
        let shards = client.shards(b"t", b"u").unwrap();

        let answer = client.evaluate(&shards[0], &request()).unwrap();
        assert!(
            matches!(answer, FragmentAnswer::Answered { .. }),
            "{error:?}"
        );
        assert_eq!(transport.call_count(), 3, "{error:?} was not retried");
    }
}

/// A retry that never succeeds still ends, and says how many attempts it spent.
#[test]
fn a_retried_call_gives_up_inside_its_budget() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(
        Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::ServerIsBusy {
                reason: "shedding".to_owned(),
            }),
        )
        .forever(),
    );
    let client = client(&transport, vec![region(1, b"", b"", &[1], Some(9))]);
    let shards = client.shards(b"t", b"u").unwrap();

    let error = client.evaluate(&shards[0], &request()).unwrap_err();
    assert!(
        matches!(
            error,
            esker_client::Error::RetriesExhausted { .. }
                | esker_client::Error::DeadlineExceeded { .. }
        ),
        "{error:?}"
    );
    let budget = client.router().options().retry.max_retries as usize;
    assert!(
        transport.call_count() <= budget + 1,
        "spent {} attempts on a budget of {budget}",
        transport.call_count()
    );
}
