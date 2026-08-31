//! What the client does when the store says no.
//!
//! Every test here runs against `FakeTransport` and `FakeClock`: no socket, no wall clock, and
//! therefore no test that takes a real second to prove a two-second backoff. That is not only
//! about speed. A retry rule is a claim about an exact sequence of delays and an exact
//! sequence of requests, and neither can be asserted against a real server — you cannot make
//! one answer `NotLeader` four times and then succeed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use esker_client::clock::FakeClock;
use esker_client::region_cache::{RegionResolver, Route, StaticRegion};
use esker_client::retry::RetryPolicy;
use esker_client::testing::{Call, FakeTransport, Matcher, Outcome, Rule};
use esker_client::wire::{
    Epoch, Method, Peer, ProtoError, RawKvReq, RawKvResp, Region, RequestOutcome,
};
use esker_client::{ClientOptions, Error, RawClient};

/// Store `n` hosts peer `n * 10`, so an assertion can tell the two apart at a glance.
fn three_peers() -> Vec<Peer> {
    vec![Peer::voter(1, 10), Peer::voter(2, 20), Peer::voter(3, 30)]
}

fn one_region(peers: Vec<Peer>) -> Arc<dyn RegionResolver> {
    let leader = peers.first().copied();
    Arc::new(StaticRegion::new(Route {
        region: Region {
            id: 1,
            start_key: Bytes::new(),
            end_key: Bytes::new(),
            peers,
            epoch: Epoch::INITIAL,
        },
        leader,
    }))
}

struct Harness {
    client: RawClient,
    transport: Arc<FakeTransport>,
    clock: Arc<FakeClock>,
}

fn harness_with(options: ClientOptions, peers: Vec<Peer>) -> Harness {
    let transport = Arc::new(FakeTransport::new());
    let clock = Arc::new(FakeClock::new());
    let client = RawClient::with_options(transport.clone(), one_region(peers), options)
        .with_clock(clock.clone());
    Harness {
        client,
        transport,
        clock,
    }
}

fn harness() -> Harness {
    harness_with(
        ClientOptions {
            // Fixed so the jitter draws are the same every run.
            jitter_seed: Some(0xE5E5),
            ..ClientOptions::default()
        },
        three_peers(),
    )
}

fn busy() -> ProtoError {
    ProtoError::ServerIsBusy {
        reason: "l0 stall".to_owned(),
    }
}

fn always(outcome: Outcome) -> Rule {
    Rule::new(Matcher::Any, outcome).forever()
}

/// The limit a scan actually went out with, which is what the capping rules are about.
fn limit_of(call: &Call) -> u32 {
    match call.body() {
        Some(RawKvReq::Scan { limit, .. }) => *limit,
        other => panic!("expected a scan, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// The retry budget
// ---------------------------------------------------------------------------------------

/// The documented budget is `max_retries` retries *after* the first attempt, so a store that
/// never recovers is asked exactly `max_retries + 1` times and then the caller is told.
#[test]
fn a_redirectable_error_is_retried_exactly_the_documented_number_of_times() {
    for error in [
        ProtoError::NotLeader {
            region_id: 1,
            leader_hint: None,
        },
        ProtoError::EpochNotMatch {
            current_regions: vec![],
        },
        ProtoError::RegionNotFound { region_id: 1 },
        busy(),
    ] {
        let harness = harness();
        harness
            .transport
            .script(always(Outcome::Fail(error.clone())));

        let result = harness.client.get(b"k");
        let budget = harness.client.options().retry.max_retries;

        match result {
            Err(Error::RetriesExhausted { attempts, source }) => {
                assert_eq!(attempts, budget + 1, "{error:?}");
                assert_eq!(*source, error, "the last refusal is reported");
            }
            other => panic!("{error:?} produced {other:?}"),
        }
        assert_eq!(
            u32::try_from(harness.transport.call_count()).unwrap(),
            budget + 1
        );
        assert_eq!(harness.clock.sleeps().len(), budget as usize);
    }
}

/// The delays are the bounded exponential schedule with equal jitter: never less than half the
/// scheduled wait, never more than all of it. Asserted against the schedule itself rather than
/// against numbers copied out of a run, so a change to the policy shows up here.
#[test]
fn the_backoff_sequence_follows_the_schedule_it_documents() {
    let harness = harness();
    harness.transport.script(always(Outcome::Fail(busy())));
    let _unused = harness.client.get(b"k");

    let policy = RetryPolicy::default();
    let sleeps = harness.clock.sleeps();
    assert_eq!(sleeps.len(), policy.max_retries as usize);

    for (attempt, slept) in sleeps.iter().enumerate() {
        let scheduled = policy.backoff(u32::try_from(attempt).unwrap());
        assert!(
            *slept >= scheduled / 2 && *slept <= scheduled,
            "retry {attempt} slept {slept:?}, outside the jitter band of {scheduled:?}"
        );
    }

    // 10, 20, 40, 80, 160, 320, 640, 1280 ms before jitter — growing, and flattening at the
    // ceiling rather than growing without bound.
    assert_eq!(policy.backoff(0), Duration::from_millis(10));
    assert_eq!(policy.backoff(7), Duration::from_millis(1_280));
    assert_eq!(policy.backoff(20), policy.backoff_max);
    assert!(harness.clock.elapsed() < Duration::from_secs(3));
}

/// Two clients failing at the same instant must not retry in lockstep; that is how a cluster
/// that has just recovered gets knocked over by its own clients.
#[test]
fn two_clients_do_not_back_off_on_the_same_schedule() {
    let delays = |seed: u64| {
        let harness = harness_with(
            ClientOptions {
                jitter_seed: Some(seed),
                ..ClientOptions::default()
            },
            three_peers(),
        );
        harness.transport.script(always(Outcome::Fail(busy())));
        let _unused = harness.client.get(b"k");
        harness.clock.sleeps()
    };
    assert_ne!(delays(1), delays(2));
    assert_eq!(delays(1), delays(1), "a fixed seed is reproducible");
}

// ---------------------------------------------------------------------------------------
// What each redirect repairs
// ---------------------------------------------------------------------------------------

/// `NotLeader` carries a peer id, and the next attempt has to go to it. Without this the
/// client asks the same follower until its budget is gone.
#[test]
fn not_leader_sends_the_next_attempt_to_the_hinted_peer() {
    let harness = harness();
    harness
        .transport
        .script(Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::NotLeader {
                region_id: 1,
                leader_hint: Some(30),
            }),
        ))
        .script(
            Rule::new(
                Matcher::Peer(30),
                Outcome::Reply(RawKvResp::Get {
                    value: Some(Bytes::from_static(b"v")),
                }),
            )
            .forever(),
        );

    let value = harness.client.get(b"k").expect("the redirect is followed");
    assert_eq!(value, Some(Bytes::from_static(b"v")));
    assert_eq!(harness.transport.peers(), vec![10, 30]);
    assert_eq!(
        harness.transport.stores(),
        vec![1, 3],
        "the hinted peer's store is the one that gets the second call"
    );
    assert_eq!(harness.clock.sleeps().len(), 1);
}

/// A hint naming a peer the cached region does not have means the cache is stale. Routing to
/// it would send the request to an address nobody knows, so the leader is forgotten instead
/// and the next attempt asks a peer that does exist.
#[test]
fn a_hint_for_an_unknown_peer_is_dropped_rather_than_followed() {
    let harness = harness();
    harness
        .transport
        .script(Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::NotLeader {
                region_id: 1,
                leader_hint: Some(999),
            }),
        ))
        .script(always(Outcome::Reply(RawKvResp::Get { value: None })));

    assert_eq!(harness.client.get(b"k").expect("recovered"), None);
    assert_eq!(harness.transport.peers(), vec![10, 10]);
}

/// `EpochNotMatch` carries the regions that now cover the range, so one round trip repairs the
/// cache. The next attempt must go out with the *new* epoch, not the one that was refused.
#[test]
fn epoch_not_match_replaces_the_cached_region() {
    let harness = harness();
    // A real split: two halves that tile the parent's range. Overlapping halves would be a
    // malformed answer, and the cache would collapse them — see the region-cache tests.
    let lower = Region {
        id: 1,
        start_key: Bytes::new(),
        end_key: Bytes::from_static(b"m"),
        peers: three_peers(),
        epoch: Epoch::new(1, 2),
    };
    let upper = Region {
        id: 4,
        start_key: Bytes::from_static(b"m"),
        end_key: Bytes::new(),
        peers: three_peers(),
        epoch: Epoch::new(1, 2),
    };
    harness
        .transport
        .script(Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::EpochNotMatch {
                current_regions: vec![lower, upper],
            }),
        ))
        .script(always(Outcome::Reply(RawKvResp::Get { value: None })));

    assert_eq!(harness.client.get(b"k").expect("recovered"), None);

    let epochs: Vec<Epoch> = harness
        .transport
        .calls()
        .iter()
        .filter_map(|call| call.header().map(|header| header.epoch))
        .collect();
    assert_eq!(
        epochs,
        vec![Epoch::INITIAL, Epoch::new(1, 2)],
        "the retry must carry the epoch the store just taught us"
    );
    assert_eq!(harness.client.cache().len(), 2, "both halves were learned");
    // `k` sorts below `m`, so the retry went to the lower half and kept its region id.
    let regions: Vec<u64> = harness
        .transport
        .calls()
        .iter()
        .filter_map(|call| call.header().map(|header| header.region_id))
        .collect();
    assert_eq!(regions, vec![1, 1]);
}

/// With no replacement regions to learn from, the entry is dropped and the resolver answers
/// again — the `GetRegion` refresh of `docs/DESIGN.md` §7, stubbed for phase 4.
#[test]
fn an_epoch_error_with_nothing_to_learn_falls_back_to_the_resolver() {
    let harness = harness();
    harness
        .transport
        .script(Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::EpochNotMatch {
                current_regions: vec![],
            }),
        ))
        .script(always(Outcome::Reply(RawKvResp::Get { value: None })));

    assert_eq!(harness.client.get(b"k").expect("recovered"), None);
    assert_eq!(harness.transport.call_count(), 2);
    assert_eq!(harness.client.cache().len(), 1, "the cache was refilled");
}

/// `KeyNotInRegion` is not retryable — waiting cannot fix a routing mistake — but it does
/// prove the cached region is wrong, and leaving it there would make the next call fail the
/// same way.
#[test]
fn a_key_outside_the_region_surfaces_and_still_clears_the_cache() {
    let harness = harness();
    harness
        .transport
        .script(always(Outcome::Fail(ProtoError::KeyNotInRegion {
            key: Bytes::from_static(b"k"),
            region_id: 1,
            start_key: Bytes::from_static(b"m"),
            end_key: Bytes::new(),
        })));

    let error = harness.client.get(b"k").expect_err("must not be retried");
    assert!(matches!(
        error,
        Error::Store(ProtoError::KeyNotInRegion { .. })
    ));
    assert!(error.changed_nothing());
    assert_eq!(harness.transport.call_count(), 1, "no retry");
    assert!(harness.client.cache().is_empty(), "the stale entry stayed");
}

// ---------------------------------------------------------------------------------------
// What is never retried
// ---------------------------------------------------------------------------------------

/// Every error here is a **refusal**: the store answered, and its answer was no. There is
/// nothing to ask again for, whatever the method was.
///
/// An error whose outcome is `Unknown` is a different thing and is not in this list — no answer
/// came back at all, which for a read is worth asking again and for a write is not. The two
/// tests below that rule are what cover those.
#[test]
fn a_non_retryable_error_surfaces_on_the_first_attempt() {
    for error in [
        ProtoError::invalid("empty key"),
        ProtoError::Locked {
            lock_info: Bytes::from_static(b"lock"),
        },
        ProtoError::not_sent("connection refused"),
    ] {
        assert_eq!(
            error.outcome(),
            RequestOutcome::NotApplied,
            "{error:?} is not a refusal"
        );
        let harness = harness();
        harness
            .transport
            .script(always(Outcome::Fail(error.clone())));

        let got = harness.client.get(b"k").expect_err("must not be retried");
        assert!(matches!(got, Error::Store(_)), "{error:?} became {got:?}");
        assert_eq!(harness.transport.call_count(), 1, "{error:?} was retried");
        assert!(harness.clock.sleeps().is_empty(), "{error:?} backed off");
    }
}

/// The rule the phase-5 transaction layer depends on. A write that went out and was never
/// answered may be in the log; this client will not send it again, and says so in the type
/// rather than in a message.
#[test]
fn an_unanswered_write_is_ambiguous_and_is_never_re_sent() {
    let harness = harness();
    harness
        .transport
        .script(always(Outcome::Fail(ProtoError::Closed {
            detail: "connection reset".to_owned(),
        })));

    let error = harness
        .client
        .put(b"k", b"v")
        .expect_err("no answer came back");
    match &error {
        Error::AmbiguousResult { method, source } => {
            assert_eq!(*method, Method::RawPut);
            assert!(source.is_ambiguous());
        }
        other => panic!("expected an ambiguous result, got {other:?}"),
    }
    assert!(
        !error.changed_nothing(),
        "the caller must not be told the write did not happen"
    );
    assert_eq!(harness.transport.call_count(), 1, "the write was re-sent");
}

/// The other half of the same rule, and the reason it is a rule about the *method*: asking a
/// read again cannot change what the first attempt did, so a read whose answer was lost is
/// simply asked again — of a different peer, because the one that lost it is the one that
/// stopped.
///
/// This is what stops a dropped packet from being manufactured into a refusal. A cluster
/// losing a node is exactly when a client most needs its reads to work, and the region has two
/// other replicas that could have answered.
#[test]
fn a_read_whose_answer_was_lost_is_asked_again() {
    let harness = harness();
    harness.transport.script_all([
        Rule::new(
            Matcher::Any,
            Outcome::Fail(ProtoError::Closed {
                detail: "the Raft peer stopped".to_owned(),
            }),
        )
        .times(1),
        always(Outcome::Reply(RawKvResp::Get {
            value: Some(Bytes::from_static(b"v")),
        })),
    ]);

    let found = harness.client.get(b"k").expect("the second peer answered");
    assert_eq!(found.as_deref(), Some(&b"v"[..]));
    assert_eq!(
        harness.transport.call_count(),
        2,
        "the read was not re-asked"
    );
}

/// And it is bounded by the same budget as any other retry, and still surfaces the error that
/// caused it rather than inventing one.
///
/// It is **not** `AmbiguousResult`: nothing about a read is ambiguous, whatever happened to the
/// answer. That distinction is what `prompts/05-txn.md` is built on, and losing it here would
/// lose it everywhere.
#[test]
fn a_read_that_never_gets_an_answer_exhausts_the_budget_and_says_why() {
    let harness = harness();
    harness
        .transport
        .script(always(Outcome::Fail(ProtoError::Closed {
            detail: "connection reset".to_owned(),
        })));

    let error = harness
        .client
        .get(b"k")
        .expect_err("no answer ever came back");
    match &error {
        Error::RetriesExhausted { attempts, source } => {
            assert_eq!(*attempts, RetryPolicy::default().max_retries + 1);
            assert!(matches!(**source, ProtoError::Closed { .. }), "{source:?}");
        }
        other => panic!("expected the budget to run out, got {other:?}"),
    }
    assert!(
        !matches!(error, Error::AmbiguousResult { .. }),
        "a read was called ambiguous"
    );
}

/// A request that provably never left is safe to repeat, and the caller is told so — that is
/// what lets a caller retry it itself without risking a duplicate.
#[test]
fn a_request_that_never_left_says_the_database_is_untouched() {
    let harness = harness();
    harness
        .transport
        .script(always(Outcome::Fail(ProtoError::not_sent("refused"))));

    let error = harness.client.put(b"k", b"v").expect_err("never sent");
    assert!(error.changed_nothing());
}

// ---------------------------------------------------------------------------------------
// The bounds
// ---------------------------------------------------------------------------------------

/// A deadline has to cut a retry storm short: without it, a caller behind a stalled cluster
/// waits for the whole budget however long that is.
#[test]
fn the_deadline_stops_a_retry_storm_before_the_budget_does() {
    let harness = harness_with(
        ClientOptions {
            call_timeout: Duration::from_millis(100),
            jitter_seed: Some(7),
            ..ClientOptions::default()
        },
        three_peers(),
    );
    harness.transport.script(always(Outcome::Fail(busy())));

    let error = harness.client.get(b"k").expect_err("the deadline fires");
    let budget = harness.client.options().retry.max_retries;
    match &error {
        Error::DeadlineExceeded { attempts, source } => {
            assert!(
                *attempts < budget + 1,
                "the deadline let the whole budget run: {attempts} attempts"
            );
            assert!(*attempts >= 1);
            assert!(source.is_some(), "say what the caller was waiting on");
        }
        other => panic!("expected a deadline, got {other:?}"),
    }
    assert!(error.changed_nothing(), "every retried error is a refusal");
    assert!(
        harness.clock.elapsed() <= Duration::from_millis(100),
        "the client slept past its own deadline"
    );
}

/// Refused here rather than at the far end, where an oversized frame looks like a connection
/// failure and tells the caller nothing.
#[test]
fn a_request_too_large_for_a_frame_is_refused_before_it_is_sent() {
    let harness = harness();
    harness.transport.set_max_frame_size(1024);
    harness
        .transport
        .script(always(Outcome::Reply(RawKvResp::Put)));

    let value = vec![0u8; 4096];
    let error = harness.client.put(b"k", &value).expect_err("too large");
    match error {
        Error::RequestTooLarge { bytes, limit } => {
            assert!(bytes >= 4096);
            assert_eq!(limit, 1024);
        }
        other => panic!("expected a size refusal, got {other:?}"),
    }
    assert_eq!(harness.transport.call_count(), 0, "it went out anyway");
}

/// A scan whose limit is larger than a frame can hold would be answered with a response nobody
/// can send. Capping turns that into a smaller answer instead of a failed call.
#[test]
fn a_scan_limit_is_capped_and_zero_means_the_protocol_default() {
    let harness = harness();
    harness
        .transport
        .script(always(Outcome::Reply(RawKvResp::Scan { pairs: vec![] })));

    harness.client.scan(b"a", b"z", u32::MAX).expect("scan");
    assert_eq!(
        limit_of(&harness.transport.nth_call(0).unwrap()),
        harness.client.options().max_scan_limit
    );

    harness.transport.clear_log();
    harness.client.scan(b"a", b"z", 0).expect("scan");
    assert_eq!(
        limit_of(&harness.transport.nth_call(0).unwrap()),
        esker_client::wire::DEFAULT_SCAN_LIMIT,
        "zero must not mean unlimited"
    );

    harness.transport.clear_log();
    harness.client.scan(b"a", b"z", 7).expect("scan");
    assert_eq!(limit_of(&harness.transport.nth_call(0).unwrap()), 7);
}

// ---------------------------------------------------------------------------------------
// The negative the prompt insists on
// ---------------------------------------------------------------------------------------

/// The `'r'` namespace of `docs/DESIGN.md` §3 is the **store's** job. A client that added it
/// too would double-prefix, and nothing would look wrong until a scan came back full of keys
/// nobody wrote. The only way to check a negative like this is to look at what reached the
/// wire.
#[test]
fn the_client_sends_raw_user_bytes_and_never_namespaces_them() {
    let harness = harness();
    harness.transport.unmatched(Outcome::Reply(RawKvResp::Put));

    // Keys that would be indistinguishable from a prefixed key if anything were added.
    let awkward: [&[u8]; 4] = [b"r", b"rkey", b"", b"\x00\xff"];
    for key in awkward {
        harness.transport.clear_log();
        harness.client.put(key, b"v").expect("put");
        let call = harness.transport.nth_call(0).expect("one call");
        assert_eq!(
            call.key(),
            Some(key),
            "the client rewrote the key on its way out"
        );
        match call.body() {
            Some(RawKvReq::Put { key: sent, .. }) => assert_eq!(&sent[..], key),
            other => panic!("expected a put, got {other:?}"),
        }
    }

    // Range bounds are keys too, and are the easiest place to forget.
    harness.transport.clear_log();
    harness
        .transport
        .unmatched(Outcome::Reply(RawKvResp::Scan { pairs: vec![] }));
    harness.client.scan(b"lo", b"hi", 10).expect("scan");
    match harness
        .transport
        .nth_call(0)
        .and_then(|call| call.body().cloned())
    {
        Some(RawKvReq::Scan { start, end, .. }) => {
            assert_eq!(&start[..], b"lo");
            assert_eq!(&end[..], b"hi");
        }
        other => panic!("expected a scan, got {other:?}"),
    }

    harness.transport.clear_log();
    harness
        .transport
        .unmatched(Outcome::Reply(RawKvResp::DeleteRange { deleted: 0 }));
    harness
        .client
        .delete_range(b"lo", b"hi")
        .expect("delete_range");
    match harness
        .transport
        .nth_call(0)
        .and_then(|call| call.body().cloned())
    {
        Some(RawKvReq::DeleteRange { start, end, .. }) => {
            assert_eq!(&start[..], b"lo");
            assert_eq!(&end[..], b"hi");
        }
        other => panic!("expected a delete_range, got {other:?}"),
    }
}

/// A store that answers the wrong method proves it did *something*, so the caller must not be
/// told the database is untouched.
#[test]
fn an_answer_for_the_wrong_method_is_a_protocol_error() {
    let harness = harness();
    harness
        .transport
        .script(always(Outcome::Reply(RawKvResp::Delete)));

    let error = harness.client.put(b"k", b"v").expect_err("wrong method");
    match &error {
        Error::UnexpectedResponse { expected, actual } => {
            assert_eq!(*expected, Method::RawPut);
            assert_eq!(*actual, Method::RawDelete);
        }
        other => panic!("expected a protocol error, got {other:?}"),
    }
    assert!(!error.changed_nothing());
}

/// Durability is an opt-*out*, never a default (`CLAUDE.md` invariant 1).
#[test]
fn writes_ask_for_durability_unless_the_caller_says_otherwise() {
    let harness = harness();
    harness
        .transport
        .script(always(Outcome::Reply(RawKvResp::Put)));

    harness.client.put(b"k", b"v").expect("put");
    harness.client.put_with(b"k", b"v", false).expect("put");

    let syncs: Vec<bool> = harness
        .transport
        .calls()
        .iter()
        .filter_map(|call| match call.body() {
            Some(RawKvReq::Put { sync, .. }) => Some(*sync),
            _ => None,
        })
        .collect();
    assert_eq!(syncs, vec![true, false]);
}

/// A cache hit must not go back to the resolver, or the region cache is not a cache.
#[test]
fn the_second_call_routes_from_the_cache() {
    let harness = harness();
    harness
        .transport
        .script(always(Outcome::Reply(RawKvResp::Get { value: None })));

    assert!(harness.client.cache().is_empty());
    harness.client.get(b"a").expect("get");
    assert_eq!(harness.client.cache().len(), 1);
    harness.client.get(b"b").expect("get");
    assert_eq!(harness.client.cache().len(), 1);
    assert_eq!(harness.transport.call_count(), 2);
}
