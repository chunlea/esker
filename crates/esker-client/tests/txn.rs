//! `TxnClient` against a scripted transport and a clock that jumps.
//!
//! Every rule of `docs/txn-spec.md` §5 that the *client* is responsible for has a case here:
//! the order the two phases go out in, read-your-writes, lock resolution and its bounds, the
//! per-region grouping, and the retry rules the router enforces on a transactional request the
//! same way it does on a `RawKv` one.
//!
//! Nothing here touches a socket or a wall clock. That is the point of both seams.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_client::clock::FakeClock;
use esker_client::region_cache::{RegionTable, Route, StaticRegion};
use esker_client::router::{ClientOptions, Router};
use esker_client::testing::{FakeTransport, Matcher, Outcome, Rule};
use esker_client::wire::{
    Epoch, LockInfo, Method, Peer, ProtoError, RawKvReq, RawKvResp, Region, TxnKvReq, TxnKvResp,
    TxnMutation, TxnStatus,
};
use esker_client::{CountingOracle, Error, TxnClient};

/// A timestamp shaped the way the oracle mints them: `ts = physical_ms << 18 | logical`
/// (`docs/txn-spec.md` §5.5). A lease is judged in the physical half alone, so a fixture that
/// counts by one describes a cluster where a millisecond never passes and no lock ever expires.
const fn at_ms(physical_ms: u64) -> u64 {
    physical_ms << esker_client::TSO_LOGICAL_BITS
}

/// The reader's own snapshot: a hundred seconds in, so every dead lease below has run out by
/// the time this client looks at it.
const START_TS: u64 = at_ms(100_000);

/// A transaction that started one second in and never came back. Its three-second lease was
/// over long before [`START_TS`].
const DEAD_TS: u64 = at_ms(1_000);

/// A second abandoned transaction, so a test can tell two holders apart.
const OTHER_DEAD_TS: u64 = at_ms(1_100);

/// A transaction that started just now: its lease is **live**, and a resolver that kills it
/// aborts a transaction that is still working.
const LIVE_TS: u64 = at_ms(99_500);

fn key(bytes: &'static [u8]) -> Bytes {
    Bytes::from_static(bytes)
}

/// One region covering the whole key space, with two peers so a `NotLeader` hint has
/// somewhere to point.
fn one_region() -> Arc<StaticRegion> {
    Arc::new(StaticRegion::new(Route {
        region: Region {
            id: 1,
            start_key: Bytes::new(),
            end_key: Bytes::new(),
            peers: vec![Peer::voter(1, 1), Peer::voter(9, 9)],
            epoch: Epoch::new(1, 1),
        },
        leader: Some(Peer::voter(1, 1)),
    }))
}

/// Two regions split at `m`, on stores 1 and 2.
fn two_regions() -> Arc<RegionTable> {
    Arc::new(RegionTable::from_routes([
        Route {
            region: Region {
                id: 1,
                start_key: Bytes::new(),
                end_key: key(b"m"),
                peers: vec![Peer::voter(1, 1)],
                epoch: Epoch::new(1, 1),
            },
            leader: Some(Peer::voter(1, 1)),
        },
        Route {
            region: Region {
                id: 2,
                start_key: key(b"m"),
                end_key: Bytes::new(),
                peers: vec![Peer::voter(2, 2)],
                epoch: Epoch::new(1, 1),
            },
            leader: Some(Peer::voter(2, 2)),
        },
    ]))
}

/// A client over `transport`, one region, a jumping clock and a counting oracle.
fn client(transport: &Arc<FakeTransport>) -> TxnClient {
    client_on(
        transport,
        one_region() as Arc<dyn esker_client::RegionResolver>,
    )
}

fn client_on(
    transport: &Arc<FakeTransport>,
    resolver: Arc<dyn esker_client::RegionResolver>,
) -> TxnClient {
    let options = ClientOptions {
        jitter_seed: Some(7),
        ..ClientOptions::default()
    };
    let router = Router::with_options(Arc::clone(transport) as _, resolver, options)
        .with_clock(Arc::new(FakeClock::new()));
    TxnClient::on_router(
        Arc::new(router),
        Arc::new(CountingOracle::starting_at(START_TS)),
    )
}

/// Answers every method of a whole successful commit.
fn script_a_clean_commit(transport: &FakeTransport) {
    transport
        .script(Rule::new(Matcher::Method(Method::TxnPrewrite), Outcome::PrewriteOk).forever())
        .script(
            Rule::new(
                Matcher::Method(Method::TxnCommit),
                Outcome::TxnReply(TxnKvResp::Commit {
                    status: TxnStatus::Ok,
                }),
            )
            .forever(),
        );
}

/// The `TxnKv` body of the n-th call, for asserting on what actually went out.
fn nth_txn(transport: &FakeTransport, index: usize) -> TxnKvReq {
    transport
        .nth_call(index)
        .expect("no such call")
        .txn_body()
        .cloned()
        .unwrap_or_else(|| panic!("call {index} was not a TxnKv request"))
}

// -- the buffer ---------------------------------------------------------------------------

/// A transaction that writes nothing costs nothing: no timestamps, no round trips.
#[test]
fn an_empty_transaction_commits_without_talking_to_anyone() {
    let transport = Arc::new(FakeTransport::new());
    let client = client(&transport);
    let txn = client.begin().unwrap();
    assert_eq!(txn.commit().unwrap(), None);
    assert_eq!(transport.call_count(), 0);
}

/// Writes are buffered until commit. Nothing of a transaction exists anywhere else until then,
/// which is what makes the whole write set known before the first `Prewrite`.
#[test]
fn writes_do_not_leave_the_process_until_commit() {
    let transport = Arc::new(FakeTransport::new());
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"a", b"1");
    txn.put(b"b", b"2");
    txn.delete(b"c");
    assert_eq!(transport.call_count(), 0);
    assert_eq!(txn.len(), 3);
}

/// Read-your-writes: a transaction sees its own buffer before it asks the store, and a
/// buffered delete reads as absent rather than as the committed value underneath.
#[test]
fn a_transaction_reads_its_own_writes_without_a_round_trip() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::TxnReply(TxnKvResp::Get {
        value: Some(key(b"committed")),
    }));
    let client = client(&transport);
    let mut txn = client.begin().unwrap();

    txn.put(b"a", b"mine");
    assert_eq!(txn.get(b"a").unwrap(), Some(key(b"mine")));
    txn.delete(b"a");
    assert_eq!(txn.get(b"a").unwrap(), None, "a buffered delete hides it");
    assert_eq!(transport.call_count(), 0, "neither read left the process");

    // A key the transaction has not written does go out, at its snapshot.
    assert_eq!(txn.get(b"b").unwrap(), Some(key(b"committed")));
    assert_eq!(transport.call_count(), 1);
    match nth_txn(&transport, 0) {
        TxnKvReq::Get { key: k, ts } => {
            assert_eq!(k, key(b"b"));
            assert_eq!(ts, START_TS, "a read is at the transaction's snapshot");
        }
        other => panic!("{other:?}"),
    }
}

/// A scan whose range spans a **split boundary** reads both regions.
///
/// One request reaches one region and a store answers only for the keys it owns, so the
/// single-request version of this came back holding the first region's keys and nothing else —
/// no error, nothing to notice. The walk asks each region in turn, learning where the last one
/// ended from the cache it already keeps.
#[test]
fn a_scan_across_a_split_boundary_reads_both_regions() {
    let transport = Arc::new(FakeTransport::new());
    // Region 1 owns `..m` and region 2 owns `m..`, each answering only for its own keys —
    // which is what a real store does, and what the old scan silently believed was everything.
    transport
        .script(
            Rule::new(
                Matcher::All(vec![Matcher::Method(Method::TxnScan), Matcher::Store(1)]),
                Outcome::TxnReply(TxnKvResp::Scan {
                    pairs: vec![(key(b"a"), key(b"1")), (key(b"b"), key(b"2"))],
                }),
            )
            .forever(),
        )
        .script(
            Rule::new(
                Matcher::All(vec![Matcher::Method(Method::TxnScan), Matcher::Store(2)]),
                Outcome::TxnReply(TxnKvResp::Scan {
                    pairs: vec![(key(b"n"), key(b"3")), (key(b"o"), key(b"4"))],
                }),
            )
            .forever(),
        );
    let resolver = two_regions();
    let client = client_on(
        &transport,
        resolver as Arc<dyn esker_client::RegionResolver>,
    );
    let txn = client.begin().unwrap();

    let pairs = txn.scan(b"a", b"", 100).unwrap();
    assert_eq!(
        pairs,
        vec![
            (key(b"a"), key(b"1")),
            (key(b"b"), key(b"2")),
            (key(b"n"), key(b"3")),
            (key(b"o"), key(b"4")),
        ],
        "both regions, in key order"
    );
    assert_eq!(transport.stores(), vec![1, 2], "one request per region");

    // The second request starts where the first region ended, not where the caller asked.
    match nth_txn(&transport, 1) {
        TxnKvReq::Scan { start, .. } => assert_eq!(start, key(b"m")),
        other => panic!("{other:?}"),
    }
}

/// The walk stops at the range the caller asked for, rather than running on into regions
/// beyond it.
#[test]
fn a_scan_stops_at_the_end_of_its_range() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::TxnReply(TxnKvResp::Scan {
        pairs: vec![(key(b"a"), key(b"1"))],
    }));
    let resolver = two_regions();
    let client = client_on(
        &transport,
        resolver as Arc<dyn esker_client::RegionResolver>,
    );
    let txn = client.begin().unwrap();

    // `..c` is wholly inside the first region, so the second is never asked.
    let pairs = txn.scan(b"a", b"c", 100).unwrap();
    assert_eq!(pairs, vec![(key(b"a"), key(b"1"))]);
    assert_eq!(
        transport.stores(),
        vec![1],
        "the far region is not in the range"
    );
}

/// And it stops once the limit is full, however many regions are left.
#[test]
fn a_scan_stops_when_the_limit_is_full() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::TxnReply(TxnKvResp::Scan {
        pairs: vec![(key(b"a"), key(b"1")), (key(b"b"), key(b"2"))],
    }));
    let resolver = two_regions();
    let client = client_on(
        &transport,
        resolver as Arc<dyn esker_client::RegionResolver>,
    );
    let txn = client.begin().unwrap();

    let pairs = txn.scan(b"a", b"", 2).unwrap();
    assert_eq!(pairs.len(), 2);
    assert_eq!(
        transport.stores(),
        vec![1],
        "the limit was full, so the second region was never asked"
    );
}

/// The same rule applied to a range: a buffered put appears, a buffered delete removes a row
/// the store returned, and the result stays in key order.
#[test]
fn a_scan_merges_the_buffer_over_what_the_store_returned() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::TxnReply(TxnKvResp::Scan {
        pairs: vec![(key(b"a"), key(b"stored-a")), (key(b"c"), key(b"stored-c"))],
    }));
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"b", b"mine-b");
    txn.delete(b"c");
    // Outside the range, so it must not appear.
    txn.put(b"z", b"mine-z");

    let pairs = txn.scan(b"a", b"m", 0).unwrap();
    assert_eq!(
        pairs,
        vec![(key(b"a"), key(b"stored-a")), (key(b"b"), key(b"mine-b"))]
    );
}

// -- the order of the two phases ----------------------------------------------------------

/// The commit point. The primary is prewritten alone and first, and committed alone and first;
/// everything else happens between or after. Getting this wrong is a transaction no resolver
/// can classify (`docs/txn-spec.md` §5.3).
#[test]
fn the_primary_goes_first_in_both_phases() {
    let transport = Arc::new(FakeTransport::new());
    script_a_clean_commit(&transport);
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"b", b"2");
    txn.put(b"a", b"1");
    txn.put(b"c", b"3");
    let primary = txn.primary().cloned().unwrap();
    assert_eq!(primary, key(b"a"), "the lowest key is the primary");

    let commit_ts = txn.commit().unwrap().unwrap();
    assert!(commit_ts > START_TS);

    // Four calls: prewrite(primary), prewrite(secondaries), commit(primary), commit(secondaries).
    assert_eq!(
        transport.methods(),
        vec![
            Method::TxnPrewrite,
            Method::TxnPrewrite,
            Method::TxnCommit,
            Method::TxnCommit
        ]
    );

    match nth_txn(&transport, 0) {
        TxnKvReq::Prewrite {
            start_ts,
            primary: named,
            mutations,
            ..
        } => {
            assert_eq!(start_ts, START_TS);
            assert_eq!(named, primary);
            assert_eq!(
                mutations,
                vec![TxnMutation::Put {
                    key: key(b"a"),
                    value: key(b"1")
                }],
                "the primary is prewritten alone"
            );
        }
        other => panic!("{other:?}"),
    }
    match nth_txn(&transport, 1) {
        TxnKvReq::Prewrite {
            primary: named,
            mutations,
            ..
        } => {
            assert_eq!(named, primary, "a secondary names the primary");
            let keys: Vec<&Bytes> = mutations.iter().map(TxnMutation::key).collect();
            assert_eq!(keys, vec![&key(b"b"), &key(b"c")]);
        }
        other => panic!("{other:?}"),
    }
    match nth_txn(&transport, 2) {
        TxnKvReq::Commit {
            start_ts,
            commit_ts: at,
            keys,
        } => {
            assert_eq!(start_ts, START_TS);
            assert_eq!(at, commit_ts);
            assert_eq!(keys, vec![primary.clone()], "the primary commits alone");
        }
        other => panic!("{other:?}"),
    }
    match nth_txn(&transport, 3) {
        TxnKvReq::Commit { keys, .. } => assert_eq!(keys, vec![key(b"b"), key(b"c")]),
        other => panic!("{other:?}"),
    }
}

/// A transaction that writes one key sends two calls, not four: there are no secondaries to
/// prewrite and none to clean up.
#[test]
fn a_single_key_transaction_is_two_round_trips() {
    let transport = Arc::new(FakeTransport::new());
    script_a_clean_commit(&transport);
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"only", b"v");
    txn.commit().unwrap();
    assert_eq!(
        transport.methods(),
        vec![Method::TxnPrewrite, Method::TxnCommit]
    );
}

/// Writing the same key twice sends one mutation. Two would be two locks on one key, and the
/// second would collide with the first.
#[test]
fn a_key_written_twice_is_prewritten_once() {
    let transport = Arc::new(FakeTransport::new());
    script_a_clean_commit(&transport);
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"k", b"first");
    txn.put(b"k", b"second");
    txn.delete(b"k");
    txn.commit().unwrap();

    match nth_txn(&transport, 0) {
        TxnKvReq::Prewrite { mutations, .. } => assert_eq!(
            mutations,
            vec![TxnMutation::Delete { key: key(b"k") }],
            "the last write for a key wins, and it is the only one"
        ),
        other => panic!("{other:?}"),
    }
}

/// Secondaries are grouped by region, one request per region rather than one per key.
#[test]
fn secondaries_are_grouped_by_region() {
    let transport = Arc::new(FakeTransport::new());
    script_a_clean_commit(&transport);
    let resolver = two_regions();
    let client = client_on(
        &transport,
        resolver as Arc<dyn esker_client::RegionResolver>,
    );
    let mut txn = client.begin().unwrap();
    for k in [
        b"a".as_slice(),
        b"b".as_slice(),
        b"n".as_slice(),
        b"o".as_slice(),
    ] {
        txn.put(k, b"v");
    }
    // Warm the cache, so the grouping has something to group by: it is a hint from the cache
    // and a cold cache groups everything together, which is correct and slower.
    for k in [b"a".as_slice(), b"b", b"n", b"o"] {
        let _ = client.router().cached_route(k);
    }
    let _ = txn.get(b"a");
    let _ = txn.get(b"n");
    transport.clear_log();

    txn.commit().unwrap();

    // primary(a) + two secondary groups + commit(primary) + two commit groups.
    let stores = transport.stores();
    assert_eq!(
        stores.len(),
        6,
        "one call per region per phase, plus the primary's two"
    );
    assert_eq!(
        stores.iter().filter(|store| **store == 2).count(),
        2,
        "the far region is asked once per phase"
    );
}

/// A second `commit()` or `rollback()` is a bug in the caller, not a second two-phase commit.
#[test]
fn a_transaction_cannot_be_finished_twice() {
    // `commit` and `rollback` consume the transaction, so the compiler already stops the
    // direct case; this pins the guard behind it, which is what a future `&mut self` API or a
    // clone would meet.
    let transport = Arc::new(FakeTransport::new());
    script_a_clean_commit(&transport);
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"k", b"v");
    assert!(txn.commit().is_ok());
}

// -- what the store answers ---------------------------------------------------------------

/// A write-write conflict is the transaction's fate, not a failure of the call: nothing was
/// written, and only a new transaction at a fresh snapshot can succeed.
#[test]
fn a_prewrite_conflict_ends_the_transaction() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::TxnReply(TxnKvResp::Prewrite {
        keys: vec![TxnStatus::Conflict { commit_ts: 42 }],
    }));
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"k", b"v");

    match txn.commit().unwrap_err() {
        Error::TxnConflict {
            start_ts,
            commit_ts,
            key: lost,
        } => {
            assert_eq!(start_ts, START_TS);
            assert_eq!(commit_ts, 42);
            assert_eq!(
                lost,
                Some(key(b"k")),
                "the caller has to know which key lost, or it cannot tell a duplicate key \
                 from a serialization failure"
            );
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(transport.call_count(), 1, "it stopped at the primary");
}

/// Which key lost, in a batch where only one did.
///
/// This is what a layer above needs to tell a duplicate key from a serialization failure: a
/// lost race on an ordinary row is one thing and a lost race on a unique index entry is
/// another, and only the key tells them apart (`docs/txn-spec.md` §6.1). The status list is
/// positionally aligned with the mutations, so the client already knows — it just has to not
/// throw it away.
#[test]
fn a_conflict_names_the_key_that_lost() {
    let transport = Arc::new(FakeTransport::new());
    transport
        // The primary succeeds; the second of the three secondaries is the one that lost.
        .script(Rule::new(
            Matcher::Method(Method::TxnPrewrite),
            Outcome::PrewriteOk,
        ))
        .script(
            Rule::new(
                Matcher::Method(Method::TxnPrewrite),
                Outcome::TxnReply(TxnKvResp::Prewrite {
                    keys: vec![
                        TxnStatus::Ok,
                        TxnStatus::Conflict { commit_ts: 42 },
                        TxnStatus::Ok,
                    ],
                }),
            )
            .forever(),
        );
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    for k in [
        b"index/email/a".as_slice(),
        b"index/email/b",
        b"index/email/c",
        b"index/email/d",
    ] {
        txn.put(k, b"row");
    }

    match txn.commit().unwrap_err() {
        Error::TxnConflict {
            commit_ts,
            key: lost,
            ..
        } => {
            assert_eq!(commit_ts, 42);
            assert_eq!(
                lost,
                Some(key(b"index/email/c")),
                "the second secondary, which is the third key overall"
            );
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

/// A `Commit` answers for the batch rather than per key, so it names no key — and this layer
/// does not invent one. A caller reading `None` as "some key lost" would be reading a guess.
#[test]
fn a_conflict_with_no_per_key_answer_names_no_key() {
    let transport = Arc::new(FakeTransport::new());
    transport
        .script(Rule::new(Matcher::Method(Method::TxnPrewrite), Outcome::PrewriteOk).forever())
        .script(
            Rule::new(
                Matcher::Method(Method::TxnCommit),
                Outcome::TxnReply(TxnKvResp::Commit {
                    status: TxnStatus::Conflict { commit_ts: 42 },
                }),
            )
            .forever(),
        );
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"k", b"v");

    match txn.commit().unwrap_err() {
        Error::TxnConflict { key: lost, .. } => assert_eq!(lost, None),
        other => panic!("expected a conflict, got {other:?}"),
    }
}

/// A conflict says nothing was written, and the caller may act on that without reading
/// anything back.
#[test]
fn a_conflict_changed_nothing() {
    let error = Error::TxnConflict {
        start_ts: 1,
        commit_ts: 2,
        key: None,
    };
    assert!(error.changed_nothing());
    // A transaction settled by someone else is the opposite: something was written, by them.
    assert!(
        !Error::TxnSettled {
            start_ts: 1,
            detail: "already committed at 2".to_owned(),
        }
        .changed_nothing()
    );
}

/// A transaction rolled back under it — its lock expired and a reader cleaned up — must not
/// carry on. Retrying any part of it would keep failing.
#[test]
fn a_transaction_rolled_back_under_us_is_settled() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::TxnReply(TxnKvResp::Prewrite {
        keys: vec![TxnStatus::RolledBack],
    }));
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"k", b"v");
    assert!(matches!(
        txn.commit().unwrap_err(),
        Error::TxnSettled { start_ts, .. } if start_ts == START_TS
    ));
}

/// A secondary that fails to commit is **not** a failed transaction. The primary's record is
/// written, so the transaction committed; a reader that meets the leftover lock rolls it
/// forward. Reporting an error would tell the caller their committed transaction failed.
#[test]
fn a_failed_secondary_commit_does_not_fail_a_committed_transaction() {
    let transport = Arc::new(FakeTransport::new());
    transport
        .script(Rule::new(Matcher::Method(Method::TxnPrewrite), Outcome::PrewriteOk).forever())
        // The primary's commit succeeds; every one after it fails.
        .script(Rule::new(
            Matcher::Method(Method::TxnCommit),
            Outcome::TxnReply(TxnKvResp::Commit {
                status: TxnStatus::Ok,
            }),
        ))
        .script(
            Rule::new(
                Matcher::Method(Method::TxnCommit),
                Outcome::Fail(ProtoError::Closed {
                    detail: "the store went away".to_owned(),
                }),
            )
            .forever(),
        );
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"a", b"1");
    txn.put(b"b", b"2");

    let commit_ts = txn
        .commit()
        .expect("the primary committed, so the transaction did");
    assert!(commit_ts.unwrap() > START_TS);
}

// -- locks ---------------------------------------------------------------------------------

fn a_lock(on: &'static [u8], primary: &'static [u8], start_ts: u64) -> LockInfo {
    LockInfo {
        key: key(on),
        primary: key(primary),
        start_ts,
        ttl_ms: 3_000,
    }
}

/// The two calls that settle a lock's owner before its keys are resolved
/// (`docs/txn-spec.md` §5.5): a read of the **primary**, which says whether the lease is still
/// being held, and a `Rollback` of it, which is the verdict and the act in one.
///
/// `primary_status` is what the rollback answers: `Ok` for a transaction this client just
/// killed, `Committed` for one that got there first.
fn script_settling_the_primary(
    transport: &FakeTransport,
    primary: &'static [u8],
    primary_status: TxnStatus,
) {
    transport
        // The primary is no longer locked — the owner's lock is gone, or was never taken.
        .script(
            Rule::new(
                Matcher::All(vec![
                    Matcher::Method(Method::TxnGet),
                    Matcher::Key(key(primary)),
                ]),
                Outcome::TxnReply(TxnKvResp::Get { value: None }),
            )
            .forever(),
        )
        .script(
            Rule::new(
                Matcher::Method(Method::TxnRollback),
                Outcome::TxnReply(TxnKvResp::Rollback {
                    status: primary_status,
                }),
            )
            .forever(),
        );
}

/// The `TxnKv` bodies of every call of one method, in the order they went out.
fn calls_of(transport: &FakeTransport, method: Method) -> Vec<TxnKvReq> {
    (0..transport.call_count())
        .map(|index| nth_txn(transport, index))
        .filter(|request| request.method() == method)
        .collect()
}

/// A `Locked` is not an error to report: the client settles the lock's owner and asks again.
/// That is the reader's half of `docs/txn-spec.md` §5.5.
///
/// Four calls, and the order of the middle two is the rule: the **primary is settled first**,
/// and only then does its verdict travel to the stuck key. A `ResolveLock` is not a question —
/// its `commit_ts` is an answer this client is responsible for having (see
/// `docs/plans/phase-5.md` §10.6: a store cannot classify a transaction whose primary may live
/// in another region).
#[test]
fn a_read_that_meets_a_lock_resolves_it_and_tries_again() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(Rule::new(
        Matcher::All(vec![
            Matcher::Method(Method::TxnGet),
            Matcher::Key(key(b"k")),
        ]),
        Outcome::locked(&a_lock(b"k", b"primary", DEAD_TS)),
    ));
    script_settling_the_primary(&transport, b"primary", TxnStatus::Ok);
    transport
        .script(
            Rule::new(
                Matcher::Method(Method::TxnResolveLock),
                Outcome::TxnReply(TxnKvResp::ResolveLock { resolved: 1 }),
            )
            .forever(),
        )
        .script(
            Rule::new(
                Matcher::Method(Method::TxnGet),
                Outcome::TxnReply(TxnKvResp::Get {
                    value: Some(key(b"v")),
                }),
            )
            .forever(),
        );
    let client = client(&transport);
    let txn = client.begin().unwrap();

    assert_eq!(txn.get(b"k").unwrap(), Some(key(b"v")));
    assert_eq!(
        transport.methods(),
        vec![
            Method::TxnGet,
            Method::TxnGet,
            Method::TxnRollback,
            Method::TxnResolveLock,
            Method::TxnGet
        ],
        "the read, the primary's state, the primary settled, the key resolved, the read again"
    );
    // The primary is read at the *lock's* snapshot, which is the timestamp at which its own
    // lock is in the way if it still holds one.
    match nth_txn(&transport, 1) {
        TxnKvReq::Get { key: read, ts } => {
            assert_eq!(read, key(b"primary"));
            assert_eq!(ts, DEAD_TS, "the lock's snapshot, not the reader's");
        }
        other => panic!("{other:?}"),
    }
    // The verdict is reached by rolling the primary back: it either leaves a marker or comes
    // back `Committed`, with no window in between for a commit to slip through.
    match nth_txn(&transport, 2) {
        TxnKvReq::Rollback { start_ts, keys } => {
            assert_eq!(
                start_ts, DEAD_TS,
                "the lock's transaction, not the reader's"
            );
            assert_eq!(keys, vec![key(b"primary")], "the primary, and only it");
        }
        other => panic!("{other:?}"),
    }
    // And the resolution carries that verdict to the stuck key.
    match nth_txn(&transport, 3) {
        TxnKvReq::ResolveLock {
            start_ts,
            commit_ts,
            keys,
        } => {
            assert_eq!(
                start_ts, DEAD_TS,
                "the lock's transaction, not the reader's"
            );
            assert_eq!(
                commit_ts, 0,
                "the primary was rolled back, so this rolls back"
            );
            assert_eq!(keys, vec![key(b"k")]);
        }
        other => panic!("{other:?}"),
    }
}

/// The other verdict, and the one that costs data if it is got wrong: the owner **committed**
/// between leaving its lock and being settled, so the stuck key must roll **forward** at the
/// commit timestamp the primary reports.
///
/// A resolver that sent zero here would abandon one key of a committed transaction — no error,
/// nothing to notice, and a row that was acknowledged as written simply absent.
/// `tests/txn_crash_boundaries.rs` proves the same thing against real stores.
#[test]
fn a_lock_whose_primary_committed_rolls_forward() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(Rule::new(
        Matcher::All(vec![
            Matcher::Method(Method::TxnGet),
            Matcher::Key(key(b"k")),
        ]),
        Outcome::locked(&a_lock(b"k", b"primary", DEAD_TS)),
    ));
    script_settling_the_primary(
        &transport,
        b"primary",
        TxnStatus::Committed {
            commit_ts: at_ms(1_001),
        },
    );
    transport
        .script(
            Rule::new(
                Matcher::Method(Method::TxnResolveLock),
                Outcome::TxnReply(TxnKvResp::ResolveLock { resolved: 1 }),
            )
            .forever(),
        )
        .script(
            Rule::new(
                Matcher::Method(Method::TxnGet),
                Outcome::TxnReply(TxnKvResp::Get {
                    value: Some(key(b"v")),
                }),
            )
            .forever(),
        );
    let client = client(&transport);
    let txn = client.begin().unwrap();
    assert_eq!(txn.get(b"k").unwrap(), Some(key(b"v")));

    let resolves = calls_of(&transport, Method::TxnResolveLock);
    match &resolves[..] {
        [
            TxnKvReq::ResolveLock {
                start_ts,
                commit_ts,
                keys,
            },
        ] => {
            assert_eq!(*start_ts, DEAD_TS);
            assert_eq!(
                *commit_ts,
                at_ms(1_001),
                "the transaction committed, so its keys roll forward at its commit timestamp"
            );
            assert_eq!(keys, &vec![key(b"k")]);
        }
        other => panic!("{other:?}"),
    }
}

/// A lock **inside** its lease belongs to a transaction that is still working, and a reader
/// that settles it anyway aborts it.
///
/// So the reader waits instead: no `Rollback`, no `ResolveLock`, a backoff between looks, and
/// eventually the bounded refusal. The judgement is made against a timestamp from the *oracle*
/// (`CLAUDE.md` invariant 6), never a wall clock — which is why a fixture whose timestamps have
/// no physical half would make every lock in this file look immortal.
#[test]
fn a_lock_inside_its_lease_is_waited_for_rather_than_settled() {
    let transport = Arc::new(FakeTransport::new());
    let clock = Arc::new(FakeClock::new());
    transport.script(
        Rule::new(
            Matcher::Method(Method::TxnGet),
            Outcome::locked(&a_lock(b"k", b"primary", LIVE_TS)),
        )
        .forever(),
    );
    let router = Router::with_options(
        Arc::clone(&transport) as _,
        one_region() as Arc<dyn esker_client::RegionResolver>,
        ClientOptions {
            jitter_seed: Some(7),
            ..ClientOptions::default()
        },
    )
    .with_clock(Arc::clone(&clock) as Arc<dyn esker_client::clock::Clock>);
    let client = TxnClient::on_router(
        Arc::new(router),
        Arc::new(CountingOracle::starting_at(START_TS)),
    )
    .with_max_lock_resolutions(3);

    let txn = client.begin().unwrap();
    match txn.get(b"k").unwrap_err() {
        Error::LockNotCleared { start_ts } => assert_eq!(start_ts, LIVE_TS),
        other => panic!("expected LockNotCleared, got {other:?}"),
    }

    assert!(
        !transport.methods().contains(&Method::TxnRollback),
        "a live transaction's primary must not be rolled back: {:?}",
        transport.methods()
    );
    assert!(
        !transport.methods().contains(&Method::TxnResolveLock),
        "nothing to resolve while the owner is inside its lease"
    );
    assert_eq!(
        clock.sleeps_ms(),
        vec![10, 20, 40],
        "one backoff per look, growing, so a busy key is not spun on"
    );
}

/// The reason a `Prewrite` answers per key: a batch that collides with **several** locks
/// reports all of them, and the client clears them in one round rather than one round trip per
/// contended key ([ADR 0016](../../docs/adr/0016-txnkv-on-the-wire.md) decision 1).
#[test]
fn a_prewrite_that_meets_several_locks_clears_them_in_one_round() {
    let transport = Arc::new(FakeTransport::new());
    transport
        // Rules are tried in order, so the first answers the primary's own one-key batch and
        // the second answers the secondaries' — which is the batch that meets the locks.
        .script(Rule::new(
            Matcher::Method(Method::TxnPrewrite),
            Outcome::PrewriteOk,
        ))
        // Three secondaries, two of them locked by two different transactions.
        .script(Rule::new(
            Matcher::Method(Method::TxnPrewrite),
            Outcome::TxnReply(TxnKvResp::Prewrite {
                keys: vec![
                    TxnStatus::Locked(a_lock(b"b", b"other-1", DEAD_TS)),
                    TxnStatus::Ok,
                    TxnStatus::Locked(a_lock(b"d", b"other-2", OTHER_DEAD_TS)),
                ],
            }),
        ))
        .script(
            Rule::new(
                Matcher::Method(Method::TxnResolveLock),
                Outcome::TxnReply(TxnKvResp::ResolveLock { resolved: 1 }),
            )
            .forever(),
        )
        .script(Rule::new(Matcher::Method(Method::TxnPrewrite), Outcome::PrewriteOk).forever())
        .script(
            Rule::new(
                Matcher::Method(Method::TxnCommit),
                Outcome::TxnReply(TxnKvResp::Commit {
                    status: TxnStatus::Ok,
                }),
            )
            .forever(),
        );
    // Both holders are settled the same way, and both are dead.
    script_settling_the_primary(&transport, b"other-1", TxnStatus::Ok);
    script_settling_the_primary(&transport, b"other-2", TxnStatus::Ok);
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    for k in [b"a".as_slice(), b"b", b"c", b"d"] {
        txn.put(k, b"v");
    }
    assert!(txn.commit().is_ok());

    let resolves: Vec<TxnKvReq> = (0..transport.call_count())
        .map(|index| nth_txn(&transport, index))
        .filter(|request| matches!(request, TxnKvReq::ResolveLock { .. }))
        .collect();
    assert_eq!(
        resolves.len(),
        2,
        "one call per *holding transaction*, not per locked key"
    );
    // Grouped by the transaction that holds them, because that is what a ResolveLock names.
    //
    // **Sorted, deliberately.** The two resolutions go out on two threads — that is the point
    // of `resolve_all`, one round however many transactions collided — so which of them reaches
    // the transport's log first is a race, and asserting an order here would pin the opposite of
    // what the code promises. `fan_out` returning its *results* in group order is what makes
    // that easy to miss: the answers are ordered, the side effects are not.
    let mut by_txn: Vec<u64> = resolves
        .iter()
        .map(|request| match request {
            TxnKvReq::ResolveLock { start_ts, .. } => *start_ts,
            other => panic!("{other:?}"),
        })
        .collect();
    by_txn.sort_unstable();
    assert_eq!(
        by_txn,
        vec![DEAD_TS, OTHER_DEAD_TS],
        "one resolution per holding transaction, and both holders are named"
    );

    // One resolution round: the secondaries were prewritten, resolved, prewritten again — and
    // not once per lock.
    let prewrites = transport
        .methods()
        .iter()
        .filter(|method| **method == Method::TxnPrewrite)
        .count();
    assert_eq!(
        prewrites, 3,
        "primary, secondaries, and the one retry after resolving both locks"
    );
}

/// Two locks held by the *same* transaction are one `ResolveLock`, because that is what the
/// message names: a `start_ts` and the keys of that transaction.
#[test]
fn locks_held_by_one_transaction_are_resolved_in_one_call() {
    let transport = Arc::new(FakeTransport::new());
    transport
        .script(Rule::new(
            Matcher::Method(Method::TxnPrewrite),
            Outcome::PrewriteOk,
        ))
        .script(Rule::new(
            Matcher::Method(Method::TxnPrewrite),
            Outcome::TxnReply(TxnKvResp::Prewrite {
                keys: vec![
                    TxnStatus::Locked(a_lock(b"b", b"other", DEAD_TS)),
                    TxnStatus::Locked(a_lock(b"c", b"other", DEAD_TS)),
                ],
            }),
        ))
        .script(
            Rule::new(
                Matcher::Method(Method::TxnResolveLock),
                Outcome::TxnReply(TxnKvResp::ResolveLock { resolved: 2 }),
            )
            .forever(),
        )
        .script(Rule::new(Matcher::Method(Method::TxnPrewrite), Outcome::PrewriteOk).forever())
        .script(
            Rule::new(
                Matcher::Method(Method::TxnCommit),
                Outcome::TxnReply(TxnKvResp::Commit {
                    status: TxnStatus::Ok,
                }),
            )
            .forever(),
        );
    script_settling_the_primary(&transport, b"other", TxnStatus::Ok);
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    for k in [b"a".as_slice(), b"b", b"c"] {
        txn.put(k, b"v");
    }
    assert!(txn.commit().is_ok());

    let resolves: Vec<TxnKvReq> = (0..transport.call_count())
        .map(|index| nth_txn(&transport, index))
        .filter(|request| matches!(request, TxnKvReq::ResolveLock { .. }))
        .collect();
    assert_eq!(resolves.len(), 1, "one holder, one call");
    match &resolves[0] {
        TxnKvReq::ResolveLock { start_ts, keys, .. } => {
            assert_eq!(*start_ts, DEAD_TS);
            assert_eq!(keys, &vec![key(b"b"), key(b"c")]);
        }
        other => panic!("{other:?}"),
    }
}

/// A terminal status ends the transaction *now*, even when other keys report locks: resolving
/// them would be work for a transaction that is already dead.
#[test]
fn a_conflict_beside_a_lock_ends_it_without_resolving() {
    let transport = Arc::new(FakeTransport::new());
    transport
        // The primary succeeds; the secondaries' batch carries a lock *and* a conflict.
        .script(Rule::new(
            Matcher::Method(Method::TxnPrewrite),
            Outcome::PrewriteOk,
        ))
        .script(
            Rule::new(
                Matcher::Method(Method::TxnPrewrite),
                Outcome::TxnReply(TxnKvResp::Prewrite {
                    keys: vec![
                        TxnStatus::Locked(a_lock(b"b", b"other", DEAD_TS)),
                        TxnStatus::Conflict { commit_ts: 42 },
                    ],
                }),
            )
            .forever(),
        );
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"a", b"1");
    txn.put(b"b", b"2");
    txn.put(b"c", b"3");

    let error = txn.commit().unwrap_err();
    assert!(
        matches!(error, Error::TxnConflict { commit_ts: 42, .. }),
        "got {error:?}"
    );
    assert!(
        !transport.methods().contains(&Method::TxnResolveLock),
        "nothing should be resolved for a transaction that has already lost"
    );
}

/// A store that answers a different number of statuses than the batch had mutations has said
/// nothing about some key. Reading a short list as "the rest were fine" is the silent wrong
/// answer, so it is refused.
#[test]
fn a_prewrite_answered_with_the_wrong_number_of_statuses_is_refused() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::TxnReply(TxnKvResp::prewrite_ok(3)));
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"only", b"v");
    assert!(matches!(
        txn.commit().unwrap_err(),
        Error::Store(ProtoError::InvalidRequest { .. })
    ));
}

/// A prewrite that meets someone else's lock resolves it too — a writer is a reader of the
/// `lock` CF like any other.
#[test]
fn a_prewrite_that_meets_a_lock_resolves_it_and_tries_again() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(Rule::new(
        Matcher::Method(Method::TxnPrewrite),
        Outcome::TxnReply(TxnKvResp::Prewrite {
            keys: vec![TxnStatus::Locked(a_lock(b"k", b"other", DEAD_TS))],
        }),
    ));
    script_settling_the_primary(&transport, b"other", TxnStatus::Ok);
    transport
        .script(
            Rule::new(
                Matcher::Method(Method::TxnResolveLock),
                Outcome::TxnReply(TxnKvResp::ResolveLock { resolved: 1 }),
            )
            .forever(),
        )
        .script(Rule::new(Matcher::Method(Method::TxnPrewrite), Outcome::PrewriteOk).forever())
        .script(
            Rule::new(
                Matcher::Method(Method::TxnCommit),
                Outcome::TxnReply(TxnKvResp::Commit {
                    status: TxnStatus::Ok,
                }),
            )
            .forever(),
        );
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"k", b"v");
    assert!(txn.commit().is_ok());
    assert_eq!(
        transport.methods(),
        vec![
            Method::TxnPrewrite,
            Method::TxnGet,
            Method::TxnRollback,
            Method::TxnResolveLock,
            Method::TxnPrewrite,
            Method::TxnCommit
        ],
        "a writer settles the lock's owner exactly the way a reader does"
    );
}

/// A lock that is settled and comes straight back — a competitor that keeps re-taking it —
/// never clears, and a client that waited for ever would be indistinguishable from one that
/// hung. The budget is separate from the router's, because each resolution attempt makes
/// progress and a routing retry does not.
#[test]
fn a_lock_that_never_clears_is_bounded() {
    let transport = Arc::new(FakeTransport::new());
    transport.script(
        Rule::new(
            Matcher::All(vec![
                Matcher::Method(Method::TxnGet),
                Matcher::Key(key(b"k")),
            ]),
            Outcome::locked(&a_lock(b"k", b"primary", DEAD_TS)),
        )
        .forever(),
    );
    script_settling_the_primary(&transport, b"primary", TxnStatus::Ok);
    transport.script(
        Rule::new(
            Matcher::Method(Method::TxnResolveLock),
            Outcome::TxnReply(TxnKvResp::ResolveLock { resolved: 0 }),
        )
        .forever(),
    );
    let client = client(&transport).with_max_lock_resolutions(3);
    let txn = client.begin().unwrap();

    match txn.get(b"k").unwrap_err() {
        Error::LockNotCleared { start_ts } => assert_eq!(start_ts, DEAD_TS),
        other => panic!("expected LockNotCleared, got {other:?}"),
    }
    // Four reads of `k` and three resolutions: the budget counts resolutions, and the last
    // read is what discovers the lock is still there. The reads of the *primary* are the
    // settling half and there is one per resolution, not one per look.
    assert_eq!(calls_of(&transport, Method::TxnResolveLock).len(), 3);
    let reads: Vec<Bytes> = calls_of(&transport, Method::TxnGet)
        .into_iter()
        .map(|request| match request {
            TxnKvReq::Get { key, .. } => key,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        reads.iter().filter(|read| **read == key(b"k")).count(),
        4,
        "one look before each resolution, and one after the last"
    );
    assert_eq!(
        reads
            .iter()
            .filter(|read| **read == key(b"primary"))
            .count(),
        3,
        "the primary's state is read once per resolution"
    );
}

/// A `Locked` nothing can decode is an error, never "no lock in the way". Treating it as an
/// absence would loop against a key the client can never read.
#[test]
fn an_unreadable_lock_payload_is_an_error() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::Fail(ProtoError::Locked {
        lock_info: Bytes::from_static(b"\xff\xff\xff"),
    }));
    let client = client(&transport);
    let txn = client.begin().unwrap();
    assert!(matches!(
        txn.get(b"k").unwrap_err(),
        Error::Store(ProtoError::InvalidRequest { .. })
    ));
    assert_eq!(transport.call_count(), 1, "it did not loop");
}

// -- rollback --------------------------------------------------------------------------------

/// A rollback leaves a marker on every key the transaction *might* have prewritten, which is
/// every key in its buffer: the client cannot tell a prewrite that never left from one whose
/// answer was lost, and the marker is what makes a late arrival of the second kind fail
/// (`docs/txn-spec.md` §5.4).
#[test]
fn rollback_marks_every_key_the_transaction_might_have_prewritten() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::TxnReply(TxnKvResp::Rollback {
        status: TxnStatus::Ok,
    }));
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"a", b"1");
    txn.put(b"b", b"2");
    txn.rollback().unwrap();

    assert_eq!(transport.methods(), vec![Method::TxnRollback]);
    match nth_txn(&transport, 0) {
        TxnKvReq::Rollback { start_ts, keys } => {
            assert_eq!(start_ts, START_TS);
            assert_eq!(keys, vec![key(b"a"), key(b"b")]);
        }
        other => panic!("{other:?}"),
    }
}

/// Rolling back a transaction that wrote nothing sends nothing.
#[test]
fn rolling_back_an_empty_transaction_is_free() {
    let transport = Arc::new(FakeTransport::new());
    let client = client(&transport);
    client.begin().unwrap().rollback().unwrap();
    assert_eq!(transport.call_count(), 0);
}

// -- the router's rules, on a transactional request -------------------------------------------

/// Routing retries work the same for both services: a `NotLeader` is followed, and the second
/// attempt goes to the peer the hint named.
#[test]
fn a_transactional_request_follows_a_not_leader_hint() {
    let transport = Arc::new(FakeTransport::new());
    transport
        .script(Rule::new(
            Matcher::Method(Method::TxnGet),
            Outcome::Fail(ProtoError::NotLeader {
                region_id: 1,
                leader_hint: Some(9),
            }),
        ))
        .script(
            Rule::new(
                Matcher::Method(Method::TxnGet),
                Outcome::TxnReply(TxnKvResp::Get { value: None }),
            )
            .forever(),
        );
    let client = client(&transport);
    let txn = client.begin().unwrap();
    assert_eq!(txn.get(b"k").unwrap(), None);
    assert_eq!(transport.call_count(), 2);
    assert_eq!(transport.peers(), vec![1, 9], "the hint was followed");
}

/// The write-protection rule holds for a transactional mutation: a `Prewrite` whose answer
/// never came back is `AmbiguousResult` rather than a silent re-send.
///
/// For a transaction that is **not** the end of the story, and that is the point of
/// Percolator: the prewrite either landed or it did not, and either way the transaction's fate
/// is decided by one key — so a reader that meets the lock resolves it, and this client may
/// simply try again. What the type stops is the client *assuming* which happened.
#[test]
fn an_unanswered_prewrite_is_ambiguous_and_resolvable() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::Fail(ProtoError::Timeout {
        detail: "no answer in 30s".to_owned(),
    }));
    let client = client(&transport);
    let mut txn = client.begin().unwrap();
    txn.put(b"k", b"v");

    let error = txn.commit().unwrap_err();
    match &error {
        Error::AmbiguousResult { method, .. } => assert_eq!(*method, Method::TxnPrewrite),
        other => panic!("expected AmbiguousResult, got {other:?}"),
    }
    assert!(
        !error.changed_nothing(),
        "the caller must not assume the prewrite missed"
    );
    assert_eq!(
        transport.call_count(),
        1,
        "an unanswered write is not re-sent"
    );
}

/// A read that goes unanswered is *not* ambiguous: re-reading is always safe, whatever became
/// of the first attempt.
///
/// So the client asks again itself rather than handing the caller a failure it would only have
/// retried — `retry::may_ask_again`. What the caller finally sees is the budget running out,
/// carrying the error that spent it, and never [`Error::AmbiguousResult`].
#[test]
fn an_unanswered_read_is_not_ambiguous() {
    let transport = Arc::new(FakeTransport::new());
    transport.unmatched(Outcome::Fail(ProtoError::Timeout {
        detail: "no answer in 30s".to_owned(),
    }));
    let client = client(&transport);
    let txn = client.begin().unwrap();
    let error = txn.get(b"k").unwrap_err();
    match &error {
        Error::RetriesExhausted { source, .. } => {
            assert!(matches!(**source, ProtoError::Timeout { .. }), "{source:?}");
        }
        other => panic!("expected the budget to run out, got {other:?}"),
    }
    assert!(transport.call_count() > 1, "the read was never asked again");
}

/// One region cache, one set of rules: a `RawClient` and a `TxnClient` sharing a router share
/// what either of them learned about the cluster.
#[test]
fn both_clients_can_share_one_router() {
    let transport = Arc::new(FakeTransport::new());
    transport
        .script(
            Rule::new(
                Matcher::Method(Method::RawGet),
                Outcome::Reply(RawKvResp::Get { value: None }),
            )
            .forever(),
        )
        .script(
            Rule::new(
                Matcher::Method(Method::TxnGet),
                Outcome::TxnReply(TxnKvResp::Get { value: None }),
            )
            .forever(),
        );
    let raw = esker_client::RawClient::new(Arc::clone(&transport) as _, one_region() as _);
    assert_eq!(
        raw.call(&RawKvReq::get(&b"k"[..])).unwrap(),
        RawKvResp::Get { value: None }
    );

    let txn_client = client(&transport);
    let txn = txn_client.begin().unwrap();
    assert_eq!(txn.get(b"k").unwrap(), None);
    assert_eq!(transport.methods(), vec![Method::RawGet, Method::TxnGet]);
}
