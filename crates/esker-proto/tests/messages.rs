//! Message bodies, pinned byte for byte and fuzzed.
//!
//! The golden file was produced by an encoder written separately from the one under test, so
//! it checks the implementation rather than agreeing with it. Forty message bodies is a lot to
//! pin, and that is the point: a wire format drifts one field at a time, and the field nobody
//! wrote a golden for is the one that moves.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use esker_proto::fragment::{
    AggregateKind, Body, FragmentReq, FragmentResp, Group, Partial, RefusalReason, ScanStats,
    Value, ValueType,
};
use esker_proto::messages::{DEFAULT_SCAN_LIMIT, Hello, HelloAck, RawKvReq, RawKvResp};
use esker_proto::pd::{
    ColumnarWish, Operator, PdMemberInfo, PdMembership, PdRaftBatch, PdReq, PdResp, StoreInfo,
};
use esker_proto::schema::{SchemaReq, SchemaResp};
use esker_proto::txn::{LockInfo, TxnKvReq, TxnKvResp, TxnMutation, TxnStatus};
use esker_proto::{
    Epoch, MAX_FRAME_SIZE, Method, Peer, PeerRole, ProtoError, RaftBatch, RaftMessage, Region,
    Request, RequestHeader, Response, WIRE_VERSION,
};
use esker_raft::{Entry, Message};
use proptest::prelude::*;

const GOLDEN: &str = include_str!("golden/messages.hex");

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn golden(kind: &str, name: &str) -> Vec<u8> {
    let prefix = format!("{kind} {name} ");
    let line = GOLDEN
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in the golden file"));
    let text = line.trim();
    assert!(text.len() % 2 == 0, "odd-length hex for `{prefix}`");
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("bad hex"))
        .collect()
}

/// The timestamps the transaction goldens use. Named rather than repeated, because a golden
/// whose numbers drift between two cases pins nothing about the relationship between them.
const TXN_TS: u64 = 42;
/// The commit timestamp of the transaction goldens; above [`TXN_TS`], as every commit is.
const TXN_COMMIT_TS: u64 = 50;
/// The lock TTL of the transaction goldens: the default of `docs/DESIGN.md` §14.
const TXN_TTL_MS: u64 = 3_000;
/// A safepoint big enough that its varint is not one byte.
const TXN_SAFEPOINT: u64 = 1 << 41;

/// The header every golden request carries: region 1, epoch (2, 3), peer 4.
fn header() -> RequestHeader {
    RequestHeader::new(1, Epoch::new(2, 3), 4)
}

/// The region the `Pd` goldens carry: bounded below, running to the end of the key space
/// above, and with two peers — so the golden pins the `+infinity` end key as well as the
/// ordinary fields.
fn pd_region() -> Region {
    Region {
        id: 7,
        start_key: Bytes::from_static(b"a"),
        end_key: Bytes::new(),
        peers: vec![Peer::voter(1, 10), Peer::voter(2, 11)],
        epoch: Epoch::new(2, 3),
    }
}

/// The cluster id every `Pd` golden but `Bootstrap` carries.
const PD_CLUSTER: u64 = 0xABCD;

/// The placement-driver group id the `Pd::Raft` goldens carry.
///
/// Every byte distinct, because the field is a **fixed** eight little-endian bytes rather than a
/// varint — a group id is a hash, so it is large in the ordinary case — and a value with repeated
/// bytes would not catch a byte order that had been reversed.
const PD_GROUP: u64 = 0x0123_4567_89AB_CDEF;

/// The `Pd` goldens, in their own function: `docs/DESIGN.md` §9 gives the service six methods,
/// and a corpus function holding every message of every service is one nobody reads.
/// The columnar report's two wishes: one bounded range and one running to the end of the key
/// space, so the golden pins the empty end key as `+infinity` and not as an empty range.
fn columnar_wishes() -> Vec<ColumnarWish> {
    vec![
        ColumnarWish {
            start_key: Bytes::from_static(b"t\x01"),
            end_key: Bytes::from_static(b"t\x02"),
            replicas: 1,
        },
        ColumnarWish {
            start_key: Bytes::from_static(b"t\x09"),
            end_key: Bytes::new(),
            replicas: 2,
        },
    ]
}

/// The two `Pd::Raft` goldens, apart from the rest.
///
/// Not to satisfy a line count: these are the only PD requests sent **between two placement
/// drivers** rather than by a client to one, they carry no cluster id that means anything, and
/// their payload is `esker-raft`'s codec rather than this crate's. Reading them beside a
/// heartbeat would suggest they are the same kind of thing.
fn golden_pd_raft_requests() -> Vec<(&'static str, Request)> {
    vec![
        (
            "pd-raft",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::Raft(PdRaftBatch::new(
                    PD_GROUP,
                    2,
                    vec![Message::TimeoutNow {
                        from: 2,
                        to: 3,
                        term: 9,
                    }],
                )),
            },
        ),
        // An empty batch is legal — "nothing to say" — and it is a different shape on the wire
        // from a batch of one, so it gets a golden rather than an assumption.
        (
            "pd-raft-empty",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::Raft(PdRaftBatch::new(PD_GROUP, 2, Vec::new())),
            },
        ),
    ]
}

/// The two heartbeats, apart from the rest.
///
/// Their field sets are a **cross-lane contract** — exactly the ones `docs/plans/phase-4.md` §3.2
/// pins for the store's `PdClient` (ADR 0011) — so they are worth reading as a pair rather than
/// scattered among the methods a client calls.
fn golden_pd_heartbeat_requests() -> Vec<(&'static str, Request)> {
    vec![
        (
            "pd-store-heartbeat",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::StoreHeartbeat {
                    store_id: 1,
                    capacity: 1 << 40,
                    available: 1 << 39,
                    region_count: 3,
                    leader_count: 1,
                    applied_bytes: 99,
                },
            },
        ),
        (
            "pd-region-heartbeat",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::RegionHeartbeat {
                    region: pd_region(),
                    leader_peer_id: 10,
                    term: 4,
                    approximate_size: 1 << 20,
                    applied_index: 77,
                },
            },
        ),
    ]
}

fn golden_pd_requests() -> Vec<(&'static str, Request)> {
    let mut requests = golden_pd_raft_requests();
    requests.extend(golden_pd_heartbeat_requests());
    requests.extend(vec![
        (
            "pd-report-columnar",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::ReportColumnar {
                    wishes: columnar_wishes(),
                },
            },
        ),
        (
            "pd-members",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::Members,
            },
        ),
        (
            "pd-bootstrap",
            Request::Pd {
                // Zero: the caller does not know the cluster id yet, which is the whole
                // reason it is asking.
                cluster_id: 0,
                request: PdReq::Bootstrap {
                    store: StoreInfo::new(1, "127.0.0.1:20160"),
                },
            },
        ),
        (
            "pd-get-region",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::GetRegion {
                    key: Bytes::from_static(b"key"),
                },
            },
        ),
        (
            "pd-alloc-id",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::AllocId { count: 2 },
            },
        ),
        (
            "pd-tso",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::Tso { count: 16 },
            },
        ),
        (
            "pd-schema-lease",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::SchemaLease,
            },
        ),
        (
            "pd-status",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::Status,
            },
        ),
        (
            "pd-scan-regions",
            Request::Pd {
                cluster_id: PD_CLUSTER,
                request: PdReq::ScanRegions {
                    start_key: Bytes::from_static(b"m"),
                    limit: 64,
                },
            },
        ),
    ]);
    requests
}

#[allow(clippy::too_many_lines)]
fn golden_requests() -> Vec<(&'static str, Request)> {
    let h = header();
    let mut requests = vec![
        ("hello", Request::Hello(Hello::current())),
        ("get", Request::raw_kv(h, RawKvReq::get(&b"key"[..]))),
        (
            "batch-get",
            Request::raw_kv(
                h,
                RawKvReq::BatchGet {
                    keys: vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")],
                },
            ),
        ),
        (
            "put",
            Request::raw_kv(h, RawKvReq::put(&b"k"[..], &b"v"[..])),
        ),
        (
            "admin-split",
            Request::Admin(esker_proto::AdminReq::Split {
                region_id: 3,
                split_key: Bytes::from_static(b"m"),
            }),
        ),
        (
            "admin-transfer-leader",
            Request::Admin(esker_proto::AdminReq::TransferLeader {
                region_id: 3,
                to_peer_id: 7,
            }),
        ),
        (
            "admin-regions",
            Request::Admin(esker_proto::AdminReq::Regions),
        ),
        (
            "raft-snapshot",
            Request::Snapshot(esker_proto::SnapshotRequest {
                region_id: 3,
                index: 42,
                peer_id: 7,
            }),
        ),
        (
            "raft-timeout-now",
            Request::Raft(RaftBatch::new(vec![RaftMessage::new(
                1,
                Epoch::new(2, 3),
                8,
                Message::TimeoutNow {
                    from: 5,
                    to: 6,
                    term: 7,
                },
            )])),
        ),
        (
            "raft-append",
            Request::Raft(RaftBatch::new(vec![RaftMessage::new(
                1,
                Epoch::new(2, 3),
                8,
                Message::AppendEntries {
                    from: 1,
                    to: 2,
                    term: 3,
                    prev_log_index: 4,
                    prev_log_term: 3,
                    entries: vec![Entry::empty(3, 5)],
                    leader_commit: 4,
                    context: Bytes::new(),
                },
            )])),
        ),
        (
            "put-unsynced",
            Request::raw_kv(h, RawKvReq::put(&b"k"[..], &b"v"[..]).unsynced()),
        ),
        (
            "batch-put",
            Request::raw_kv(
                h,
                RawKvReq::batch_put(vec![
                    (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
                    (Bytes::from_static(b"b"), Bytes::from_static(b"2")),
                ]),
            ),
        ),
        ("delete", Request::raw_kv(h, RawKvReq::delete(&b"k"[..]))),
        (
            "delete-range",
            Request::raw_kv(h, RawKvReq::delete_range(&b"a"[..], &b"m"[..])),
        ),
        (
            "scan",
            Request::raw_kv(h, RawKvReq::scan(&b"a"[..], &b"z"[..], 100)),
        ),
        (
            "scan-reverse",
            Request::raw_kv(
                h,
                RawKvReq::Scan {
                    start: Bytes::from_static(b"z"),
                    end: Bytes::new(),
                    limit: 0,
                    reverse: true,
                },
            ),
        ),
        (
            "cas",
            Request::raw_kv(
                h,
                RawKvReq::compare_and_swap(
                    &b"k"[..],
                    Some(Bytes::from_static(b"old")),
                    Some(Bytes::from_static(b"new")),
                ),
            ),
        ),
        (
            "cas-absent",
            Request::raw_kv(h, RawKvReq::compare_and_swap(&b"k"[..], None, None)),
        ),
    ];
    requests.extend(golden_pd_requests());
    requests.extend(golden_txn_requests());
    requests.extend(golden_fragment_requests());
    requests.extend(golden_schema_requests());
    requests
}

/// The transaction service's ten request goldens (`docs/txn-spec.md`, `docs/DESIGN.md` §8).
///
/// `ResolveLock` appears twice, because its `commit_ts` of zero is the whole "roll back"
/// signal and a golden for the commit case alone would not pin it.
fn golden_txn_requests() -> Vec<(&'static str, Request)> {
    let mut requests = golden_txn_read_requests();
    requests.extend(golden_txn_write_requests());
    requests
}

/// `Get` and `Scan`: the two methods that take a snapshot and change nothing.
fn golden_txn_read_requests() -> Vec<(&'static str, Request)> {
    let h = header();
    vec![
        (
            "txn-get",
            Request::txn_kv(
                h,
                TxnKvReq::Get {
                    key: Bytes::from_static(b"key"),
                    ts: TXN_TS,
                },
            ),
        ),
        (
            "txn-scan",
            Request::txn_kv(
                h,
                TxnKvReq::Scan {
                    start: Bytes::from_static(b"a"),
                    end: Bytes::from_static(b"z"),
                    limit: 100,
                    ts: TXN_TS,
                    reverse: false,
                },
            ),
        ),
        (
            "txn-scan-reverse",
            Request::txn_kv(
                h,
                TxnKvReq::Scan {
                    start: Bytes::from_static(b"a"),
                    end: Bytes::from_static(b"z"),
                    limit: 100,
                    ts: TXN_TS,
                    reverse: true,
                },
            ),
        ),
    ]
}

/// The six methods that write: two phases, two ways to end, and the two pieces of
/// housekeeping (a lock's TTL and the collection safepoint).
fn golden_txn_write_requests() -> Vec<(&'static str, Request)> {
    let h = header();
    vec![
        (
            "txn-prewrite",
            Request::txn_kv(
                h,
                TxnKvReq::Prewrite {
                    start_ts: TXN_TS,
                    primary: Bytes::from_static(b"p"),
                    ttl_ms: TXN_TTL_MS,
                    mutations: vec![
                        TxnMutation::Put {
                            key: Bytes::from_static(b"a"),
                            value: Bytes::from_static(b"1"),
                            read_ts: None,
                        },
                        TxnMutation::Delete {
                            key: Bytes::from_static(b"b"),
                            read_ts: None,
                        },
                    ],
                },
            ),
        ),
        (
            "txn-commit",
            Request::txn_kv(
                h,
                TxnKvReq::Commit {
                    start_ts: TXN_TS,
                    commit_ts: TXN_COMMIT_TS,
                    keys: vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")],
                },
            ),
        ),
        (
            "txn-rollback",
            Request::txn_kv(
                h,
                TxnKvReq::Rollback {
                    start_ts: TXN_TS,
                    keys: vec![Bytes::from_static(b"a")],
                },
            ),
        ),
        (
            "txn-resolve-commit",
            Request::txn_kv(
                h,
                TxnKvReq::ResolveLock {
                    start_ts: TXN_TS,
                    commit_ts: TXN_COMMIT_TS,
                    keys: vec![],
                },
            ),
        ),
        (
            "txn-resolve-rollback",
            Request::txn_kv(
                h,
                TxnKvReq::ResolveLock {
                    start_ts: TXN_TS,
                    commit_ts: 0,
                    keys: vec![Bytes::from_static(b"a")],
                },
            ),
        ),
        (
            "txn-heartbeat",
            Request::txn_kv(
                h,
                TxnKvReq::Heartbeat {
                    start_ts: TXN_TS,
                    primary: Bytes::from_static(b"p"),
                    ttl_ms: TXN_TTL_MS,
                },
            ),
        ),
        (
            "txn-gc-safepoint",
            Request::txn_kv(
                h,
                TxnKvReq::GcSafepoint {
                    safepoint: TXN_SAFEPOINT,
                },
            ),
        ),
    ]
}

/// The transaction service's response goldens. `Get` appears twice: an absent value and a
/// present one are different bytes, and a format that confused them would turn a deleted key
/// into an empty one.
fn golden_txn_responses() -> Vec<(&'static str, Response)> {
    vec![
        (
            "txn-get",
            Response::TxnKv(TxnKvResp::Get {
                value: Some(Bytes::from_static(b"v")),
            }),
        ),
        (
            "txn-get-absent",
            Response::TxnKv(TxnKvResp::Get { value: None }),
        ),
        (
            "txn-scan",
            Response::TxnKv(TxnKvResp::Scan {
                pairs: vec![(Bytes::from_static(b"a"), Bytes::from_static(b"1"))],
            }),
        ),
        // Every status has a golden. Four of the five are terminal for a transaction, and a
        // sentinel with a golden for only its happy case is a sentinel nobody has tested.
        // A `Prewrite` answers per key, so its goldens pin the *list*: two keys locked, a
        // batch that met two locks at once — the case first-only reporting could not express —
        // the two terminal shapes, and the empty batch.
        ("txn-prewrite", Response::TxnKv(TxnKvResp::prewrite_ok(2))),
        (
            "txn-prewrite-locked",
            Response::TxnKv(TxnKvResp::Prewrite {
                keys: vec![
                    TxnStatus::Ok,
                    TxnStatus::Locked(LockInfo {
                        key: Bytes::from_static(b"account/1"),
                        primary: Bytes::from_static(b"account/0"),
                        start_ts: TXN_SAFEPOINT,
                        ttl_ms: TXN_TTL_MS,
                    }),
                    TxnStatus::Locked(LockInfo {
                        key: Bytes::from_static(b"account/2"),
                        primary: Bytes::from_static(b"account/0"),
                        start_ts: TXN_SAFEPOINT,
                        ttl_ms: TXN_TTL_MS,
                    }),
                ],
            }),
        ),
        (
            "txn-prewrite-conflict",
            Response::TxnKv(TxnKvResp::Prewrite {
                keys: vec![TxnStatus::Conflict {
                    commit_ts: TXN_COMMIT_TS,
                }],
            }),
        ),
        (
            "txn-prewrite-rolledback",
            Response::TxnKv(TxnKvResp::Prewrite {
                keys: vec![TxnStatus::RolledBack],
            }),
        ),
        (
            "txn-prewrite-empty",
            Response::TxnKv(TxnKvResp::Prewrite { keys: vec![] }),
        ),
        (
            "txn-commit",
            Response::TxnKv(TxnKvResp::Commit {
                status: TxnStatus::Ok,
            }),
        ),
        (
            "txn-commit-lock-lost",
            Response::TxnKv(TxnKvResp::Commit {
                status: TxnStatus::LockNotFound,
            }),
        ),
        (
            "txn-rollback",
            Response::TxnKv(TxnKvResp::Rollback {
                status: TxnStatus::Ok,
            }),
        ),
        (
            "txn-rollback-committed",
            Response::TxnKv(TxnKvResp::Rollback {
                status: TxnStatus::Committed {
                    commit_ts: TXN_COMMIT_TS,
                },
            }),
        ),
        (
            "txn-resolve-lock",
            Response::TxnKv(TxnKvResp::ResolveLock { resolved: 3 }),
        ),
        (
            "txn-heartbeat",
            Response::TxnKv(TxnKvResp::Heartbeat { ttl_ms: TXN_TTL_MS }),
        ),
        (
            "txn-gc-safepoint",
            Response::TxnKv(TxnKvResp::GcSafepoint { safepoint: 1 << 41 }),
        ),
    ]
}

/// The `RegionHeartbeat` answers, one per operator kind, in their own function: with them
/// inline `golden_pd_responses` runs past the line budget, and a corpus function holding
/// every message of every shape is one nobody reads.
fn golden_operator_responses() -> Vec<(&'static str, Response)> {
    vec![
        (
            "pd-region-heartbeat",
            Response::Pd(PdResp::RegionHeartbeat { operator: None }),
        ),
        (
            "pd-region-heartbeat-add-peer",
            Response::Pd(PdResp::RegionHeartbeat {
                operator: Some(Operator::AddPeer {
                    region_id: 7,
                    epoch: Epoch::new(2, 3),
                    store_id: 4,
                    peer_id: 41,
                }),
            }),
        ),
        (
            "pd-region-heartbeat-remove-peer",
            Response::Pd(PdResp::RegionHeartbeat {
                operator: Some(Operator::RemovePeer {
                    region_id: 7,
                    epoch: Epoch::new(2, 3),
                    peer_id: 11,
                }),
            }),
        ),
        (
            // Reserved for 4d and pinned now, so that leader balance does not change the
            // operator encoding when it arrives.
            "pd-region-heartbeat-transfer-leader",
            Response::Pd(PdResp::RegionHeartbeat {
                operator: Some(Operator::TransferLeader {
                    region_id: 7,
                    epoch: Epoch::new(2, 3),
                    to_peer_id: 10,
                }),
            }),
        ),
        (
            // A learner that is never promoted (ADR 0022 Decision 1). The same fields as
            // `AddPeer` behind a different kind byte, because what differs is what *done* means
            // and not what is asked of Raft -- so the bytes are deliberately near-identical and
            // the golden is what stops the two being confused.
            "pd-region-heartbeat-add-learner",
            Response::Pd(PdResp::RegionHeartbeat {
                operator: Some(Operator::AddLearner {
                    region_id: 7,
                    epoch: Epoch::new(2, 3),
                    store_id: 5,
                    peer_id: 42,
                }),
            }),
        ),
    ]
}

#[allow(clippy::too_many_lines)]
fn golden_pd_responses() -> Vec<(&'static str, Response)> {
    vec![
        (
            "pd-bootstrap",
            Response::Pd(PdResp::Bootstrap {
                cluster_id: PD_CLUSTER,
                region: Some(pd_region()),
            }),
        ),
        (
            // The answer to every `Bootstrap` but the first in the life of a cluster: the
            // cluster is there and this caller did not create it.
            "pd-bootstrap-registered",
            Response::Pd(PdResp::Bootstrap {
                cluster_id: PD_CLUSTER,
                region: None,
            }),
        ),
        ("pd-store-heartbeat", Response::Pd(PdResp::StoreHeartbeat)),
        // Empty on purpose: Raft answers Raft, so a follower's reply is a message in a later
        // batch and not a value here.
        ("pd-raft", Response::Pd(PdResp::Raft)),
        (
            "pd-members",
            Response::Pd(PdResp::Members(PdMembership {
                group_id: PD_GROUP,
                this_id: 2,
                leader_id: 2,
                term: 7,
                members: vec![
                    PdMemberInfo {
                        id: 1,
                        address: "127.0.0.1:2379".to_owned(),
                    },
                    PdMemberInfo {
                        id: 2,
                        address: "127.0.0.1:2380".to_owned(),
                    },
                    PdMemberInfo {
                        id: 3,
                        address: "127.0.0.1:2381".to_owned(),
                    },
                ],
            })),
        ),
        // "Nobody leads yet" is a different answer from "the leader is member zero", and only a
        // golden for both pins the difference — the same reason `not-leader-blind` has one.
        (
            "pd-members-electing",
            Response::Pd(PdResp::Members(PdMembership {
                group_id: PD_GROUP,
                this_id: 3,
                leader_id: 0,
                term: 0,
                members: vec![PdMemberInfo {
                    id: 3,
                    address: "127.0.0.1:2381".to_owned(),
                }],
            })),
        ),
        (
            "pd-get-region",
            Response::Pd(PdResp::GetRegion {
                region: Some(pd_region()),
                leader_peer_id: 10,
                stores: vec![StoreInfo::new(1, "127.0.0.1:20160")],
            }),
        ),
        (
            "pd-get-region-none",
            Response::Pd(PdResp::GetRegion {
                region: None,
                leader_peer_id: 0,
                stores: Vec::new(),
            }),
        ),
        (
            "pd-alloc-id",
            Response::Pd(PdResp::AllocId {
                start: 1_000,
                count: 2,
            }),
        ),
        (
            "pd-tso",
            Response::Pd(PdResp::Tso {
                start_ts: 0x1234_5678_9ABC,
                count: 16,
            }),
        ),
        ("pd-report-columnar", Response::Pd(PdResp::ReportColumnar)),
        (
            "pd-schema-lease",
            Response::Pd(PdResp::SchemaLease {
                lease_ms: 5_000,
                step_interval_ms: 8_000,
                removal_extra_ms: 3_600_000,
            }),
        ),
        (
            // Two operators of different kinds and different progress, because a status with one
            // of anything pins neither the repeat nor the tag it repeats.
            "pd-status",
            Response::Pd(PdResp::Status {
                now_ms: 1_700_000_000_000,
                operators: vec![
                    esker_proto::OperatorStatus {
                        operator: Operator::AddPeer {
                            region_id: 1,
                            epoch: Epoch::new(2, 3),
                            store_id: 4,
                            peer_id: 5,
                        },
                        progress: esker_proto::OperatorProgress::Issued,
                        issued_ms: 1_699_999_000_000,
                        since_ms: 1_699_999_000_000,
                        sends: 3,
                    },
                    esker_proto::OperatorStatus {
                        operator: Operator::RemovePeer {
                            region_id: 7,
                            epoch: Epoch::new(9, 11),
                            peer_id: 13,
                        },
                        progress: esker_proto::OperatorProgress::Started,
                        issued_ms: 1_699_998_000_000,
                        since_ms: 1_699_999_500_000,
                        sends: 1,
                    },
                ],
            }),
        ),
        (
            // Two regions that meet, so the golden pins the contiguity the caller checks, and one
            // store shared by both — which is the deduplication this shape exists for.
            "pd-scan-regions",
            Response::Pd(PdResp::ScanRegions {
                regions: vec![
                    esker_proto::ScannedRegion {
                        region: Region {
                            id: 1,
                            start_key: Bytes::from_static(b"a"),
                            end_key: Bytes::from_static(b"m"),
                            peers: vec![Peer::voter(1, 10)],
                            epoch: Epoch::new(1, 2),
                        },
                        leader_peer_id: 10,
                    },
                    esker_proto::ScannedRegion {
                        region: Region {
                            id: 2,
                            start_key: Bytes::from_static(b"m"),
                            end_key: Bytes::new(),
                            peers: vec![Peer::voter(1, 20)],
                            epoch: Epoch::new(1, 3),
                        },
                        // No leader PD has heard from, which is zero and not an absent field.
                        leader_peer_id: 0,
                    },
                ],
                stores: vec![StoreInfo::new(1, "127.0.0.1:20160")],
            }),
        ),
    ]
}

fn golden_responses() -> Vec<(&'static str, Response)> {
    let region = |id: u64| Region {
        id,
        start_key: Bytes::from_static(b"a"),
        end_key: Bytes::from_static(b"m"),
        peers: vec![Peer::voter(1, 10)],
        epoch: Epoch::new(1, 2),
    };
    let mut responses = vec![
        (
            "admin-split",
            Response::Admin(esker_proto::AdminResp::Split {
                left: region(1),
                right: region(2),
            }),
        ),
        (
            "admin-transfer-leader",
            Response::Admin(esker_proto::AdminResp::TransferLeader),
        ),
        (
            "admin-regions",
            Response::Admin(esker_proto::AdminResp::Regions {
                regions: vec![esker_proto::RegionStatus {
                    region: region(1),
                    leader_peer_id: 10,
                    is_leader: true,
                    approximate_size: 4096,
                    applied_index: 42,
                }],
            }),
        ),
        (
            "hello",
            Response::Hello(HelloAck {
                version: WIRE_VERSION,
                store_id: 7,
                max_frame_size: MAX_FRAME_SIZE as u64,
            }),
        ),
        (
            "get",
            Response::RawKv(RawKvResp::Get {
                value: Some(Bytes::from_static(b"v")),
            }),
        ),
        (
            "get-absent",
            Response::RawKv(RawKvResp::Get { value: None }),
        ),
        (
            "get-empty",
            Response::RawKv(RawKvResp::Get {
                value: Some(Bytes::new()),
            }),
        ),
        (
            "batch-get",
            Response::RawKv(RawKvResp::BatchGet {
                values: vec![Some(Bytes::from_static(b"a")), None],
            }),
        ),
        ("put", Response::RawKv(RawKvResp::Put)),
        ("batch-put", Response::RawKv(RawKvResp::BatchPut)),
        ("delete", Response::RawKv(RawKvResp::Delete)),
        (
            "delete-range",
            Response::RawKv(RawKvResp::DeleteRange { deleted: 17 }),
        ),
        (
            "scan",
            Response::RawKv(RawKvResp::Scan {
                pairs: vec![
                    (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
                    (Bytes::from_static(b"b"), Bytes::from_static(b"2")),
                ],
            }),
        ),
        (
            "cas-failed",
            Response::RawKv(RawKvResp::CompareAndSwap {
                swapped: false,
                previous: Some(Bytes::from_static(b"actual")),
            }),
        ),
        (
            "cas-swapped",
            Response::RawKv(RawKvResp::CompareAndSwap {
                swapped: true,
                previous: None,
            }),
        ),
        ("raft-ack", Response::Raft),
    ];
    responses.extend(golden_pd_responses());
    responses.extend(golden_txn_responses());
    responses.extend(golden_operator_responses());
    responses.extend(golden_fragment_responses());
    responses.extend(golden_schema_responses());
    responses
}

// One literal per error code, and two apiece for the two that carry an optional hint. Splitting
// it to satisfy a line count would make it harder to check against the golden file, which is the
// whole job of this function.
#[allow(clippy::too_many_lines)]
fn golden_errors() -> Vec<(&'static str, ProtoError)> {
    vec![
        (
            "not-leader",
            ProtoError::NotLeader {
                region_id: 1,
                leader_hint: Some(7),
            },
        ),
        (
            "not-leader-blind",
            ProtoError::NotLeader {
                region_id: 1,
                leader_hint: None,
            },
        ),
        (
            "epoch-not-match",
            ProtoError::EpochNotMatch {
                current_regions: vec![
                    Region {
                        id: 1,
                        start_key: Bytes::new(),
                        end_key: Bytes::from_static(b"m"),
                        peers: vec![Peer::voter(1, 2)],
                        epoch: Epoch::new(1, 2),
                    },
                    Region {
                        id: 2,
                        start_key: Bytes::from_static(b"m"),
                        end_key: Bytes::new(),
                        peers: vec![Peer {
                            store_id: 1,
                            peer_id: 3,
                            role: PeerRole::Learner,
                        }],
                        epoch: Epoch::new(1, 2),
                    },
                ],
            },
        ),
        (
            "key-not-in-region",
            ProtoError::KeyNotInRegion {
                key: Bytes::from_static(b"z"),
                region_id: 3,
                start_key: Bytes::from_static(b"a"),
                end_key: Bytes::from_static(b"m"),
            },
        ),
        (
            "server-is-busy",
            ProtoError::ServerIsBusy {
                reason: "write stall".to_owned(),
            },
        ),
        (
            "locked",
            ProtoError::Locked {
                lock_info: Bytes::from_static(&[1, 2, 3]),
            },
        ),
        (
            "region-not-found",
            ProtoError::RegionNotFound { region_id: 9 },
        ),
        (
            "wire-version",
            ProtoError::WireVersion {
                expected: 1,
                actual: 2,
            },
        ),
        (
            "invalid-request",
            ProtoError::InvalidRequest {
                detail: "unknown method 0x0999".to_owned(),
            },
        ),
        (
            "unsupported",
            ProtoError::Unsupported {
                detail: "DeleteRange over 10000 keys".to_owned(),
            },
        ),
        (
            "corrupt",
            ProtoError::Corrupt {
                context: "frame".to_owned(),
                detail: "checksum mismatch".to_owned(),
            },
        ),
        (
            "io",
            ProtoError::Io {
                detail: "broken pipe".to_owned(),
            },
        ),
        (
            "closed",
            ProtoError::Closed {
                detail: "peer went away".to_owned(),
            },
        ),
        (
            "duplicate-id",
            ProtoError::DuplicateRequestId { request_id: 42 },
        ),
        (
            "internal",
            ProtoError::Internal {
                detail: "poisoned lock".to_owned(),
            },
        ),
        ("not-sent", ProtoError::not_sent("connection refused")),
        (
            "timeout",
            ProtoError::Timeout {
                detail: "no answer in 30s".to_owned(),
            },
        ),
        ("not-bootstrapped", ProtoError::NotBootstrapped),
        (
            "cluster-mismatch",
            ProtoError::ClusterMismatch {
                expected: 0xDEAD_BEEF,
                actual: 1,
            },
        ),
        (
            "pd-not-leader",
            ProtoError::PdNotLeader {
                leader_id: 2,
                leader_address: "127.0.0.1:2380".to_owned(),
            },
        ),
        // The blind case is its own golden for the reason `not-leader-blind` is: "I do not know
        // who leads" is a different answer from "the leader is at the empty address", and only a
        // golden for both pins the difference.
        (
            "pd-not-leader-blind",
            ProtoError::PdNotLeader {
                leader_id: 0,
                leader_address: String::new(),
            },
        ),
    ]
}

/// The lock that travels inside `ProtoError::Locked`.
///
/// The error golden pins the *frame*; those bytes are opaque to it, so without this the one
/// payload a client has to decode to make progress would be pinned nowhere.
#[test]
fn golden_lock_info() {
    let lock = LockInfo {
        key: Bytes::from_static(b"account/1"),
        primary: Bytes::from_static(b"account/0"),
        start_ts: 1 << 41,
        ttl_ms: 3_000,
    };
    assert_eq!(hex(&lock.encode()), hex(&golden("lockinfo", "account")));
    assert_eq!(
        LockInfo::decode(&golden("lockinfo", "account")).unwrap(),
        lock
    );
    // And it survives the error it rides in, which is the only way a client ever sees it.
    assert_eq!(
        LockInfo::from_error(&lock.into_error()).unwrap().unwrap(),
        lock
    );
}

/// The fragment goldens' opaque payload. **Not a real fragment**: this crate never parses one,
/// and a golden that carried a valid `esker_columnar` fragment would suggest it did.
const FRAGMENT_BYTES: &[u8] = &[0x01, 0xAA, 0xBB];
/// Big enough that its varint is three bytes, so the golden pins a multi-byte one.
const MIN_APPLY_INDEX: u64 = 1 << 20;

/// The tenant and table the schema goldens name. Both above 127 so their varints are two bytes:
/// a golden built from single-byte numbers pins nothing about the encoding it is there to pin.
const SCHEMA_TENANT: u64 = 300;
const SCHEMA_TABLE: u64 = 4_000;

/// A stand-in for a stored record. Its **bytes** are the golden's subject: `esker-proto` does not
/// decode this and must not learn how (see [`esker_proto::schema`]), so what is pinned is that the
/// envelope carries an opaque run of bytes back unchanged.
const SCHEMA_RECORD: &[u8] = b"\x01published";

fn golden_schema_requests() -> Vec<(&'static str, Request)> {
    vec![(
        "schema-fetch",
        Request::Schema(SchemaReq {
            tenant: SCHEMA_TENANT,
            table_id: SCHEMA_TABLE,
        }),
    )]
}

/// Both answers, because the difference between them is one byte and is the whole point: an empty
/// record and an absent one must not encode alike.
fn golden_schema_responses() -> Vec<(&'static str, Response)> {
    vec![
        (
            "schema-record",
            Response::Schema(SchemaResp {
                record: Some(Bytes::from_static(SCHEMA_RECORD)),
            }),
        ),
        ("schema-absent", Response::Schema(SchemaResp::absent())),
    ]
}

fn golden_fragment_requests() -> Vec<(&'static str, Request)> {
    vec![(
        "fragment-evaluate",
        Request::Fragment {
            header: header(),
            request: FragmentReq {
                fragment: Bytes::from_static(FRAGMENT_BYTES),
                ts: TXN_TS,
                min_apply_index: MIN_APPLY_INDEX,
            },
        },
    )]
}

/// The rows result the response golden carries, and a golden of the result format in its own
/// right. Two columns and two rows, the second row's text NULL, so the golden pins a declared
/// type list, a value of every shape it names, and the NULL that costs one byte.
fn golden_result_rows() -> Body {
    Body::Rows {
        types: vec![ValueType::Int8, ValueType::Text],
        rows: vec![
            vec![Value::Int8(7), Value::Text("ok".to_owned())],
            vec![Value::Int8(-1), Value::Null],
        ],
    }
}

/// A groups result: one key column, a `count` and a `sum`, and a group whose sum is NULL over no
/// rows — which is the same byte an absent value is, deliberately.
fn golden_result_groups() -> Body {
    Body::Groups {
        key_types: vec![ValueType::Text],
        aggregates: vec![
            (AggregateKind::Count, None),
            (AggregateKind::Sum, Some(ValueType::Int8)),
        ],
        groups: vec![
            Group {
                key: vec![Value::Text("a".to_owned())],
                partials: vec![Partial::Count(3), Partial::Sum(Some(Value::Int8(10)))],
            },
            Group {
                key: vec![Value::Text("b".to_owned())],
                partials: vec![Partial::Count(0), Partial::Sum(None)],
            },
        ],
    }
}

fn golden_fragment_responses() -> Vec<(&'static str, Response)> {
    vec![
        (
            "fragment-result",
            Response::Fragment(FragmentResp::Result {
                result: Bytes::from(
                    esker_proto::fragment::encode_result(&golden_result_rows()).unwrap(),
                ),
                stats: ScanStats {
                    stripes_considered: 9,
                    stripes_read: 4,
                    chunks_decoded: 12,
                    rows_scanned: 5_000,
                    rows_matched: 37,
                },
            }),
        ),
        (
            "fragment-refused",
            Response::Fragment(FragmentResp::Refused {
                reason: RefusalReason::Unsupported,
                detail: "no such aggregate: stddev".to_owned(),
            }),
        ),
    ]
}

/// The result format is pinned on its own, not only inside the response that carries it: it has
/// its own version and its own checksum because it survives being cached, spilled or forwarded
/// without a frame around it, and a format with its own version needs its own golden.
#[test]
fn golden_fragment_results() {
    for (name, body) in [
        ("rows", golden_result_rows()),
        ("groups", golden_result_groups()),
    ] {
        let encoded = esker_proto::fragment::encode_result(&body).unwrap();
        assert_eq!(
            hex(&encoded),
            hex(&golden("result", name)),
            "result `{name}` drifted"
        );
        assert_eq!(
            esker_proto::fragment::decode_result(&golden("result", name)).unwrap(),
            body
        );
    }
}

#[test]
fn golden_request_bodies() {
    for (name, request) in golden_requests() {
        assert_eq!(
            hex(&request.encode()),
            hex(&golden("request", name)),
            "request `{name}` drifted"
        );
        assert_eq!(Request::decode(&golden("request", name)).unwrap(), request);
    }
}

#[test]
fn golden_response_bodies() {
    for (name, response) in golden_responses() {
        assert_eq!(
            hex(&response.encode()),
            hex(&golden("response", name)),
            "response `{name}` drifted"
        );
        assert_eq!(
            Response::decode(&golden("response", name)).unwrap(),
            response
        );
    }
}

#[test]
fn golden_error_bodies() {
    for (name, error) in golden_errors() {
        assert_eq!(
            hex(&error.encode()),
            hex(&golden("error", name)),
            "error `{name}` drifted"
        );
        assert_eq!(ProtoError::decode(&golden("error", name)).unwrap(), error);
    }
}

/// Every method and every error code has a golden. A format is pinned by the case nobody
/// wrote, so the sweep is what makes the file above trustworthy.
#[test]
fn the_goldens_cover_every_method_and_every_error_code() {
    let pinned_requests: std::collections::BTreeSet<Method> = golden_requests()
        .iter()
        .map(|(_, request)| request.method())
        .collect();
    let pinned_responses: std::collections::BTreeSet<Method> = golden_responses()
        .iter()
        .map(|(_, response)| response.method())
        .collect();
    let all: std::collections::BTreeSet<Method> = Method::ALL.into_iter().collect();
    assert_eq!(pinned_requests, all, "a method has no golden request");

    // A streamed method has no `Response` frame to pin: its answer is a run of `Stream` frames,
    // whose chunk format is `esker-store`'s and is golden-tested there. The exclusion is taken
    // from the method itself rather than a list here, so a second streamed method cannot be added
    // without this test noticing.
    let unary: std::collections::BTreeSet<Method> = Method::ALL
        .into_iter()
        .filter(|method| !method.is_streamed())
        .collect();
    assert_eq!(pinned_responses, unary, "a method has no golden response");
    assert!(
        pinned_responses.iter().all(|method| !method.is_streamed()),
        "a streamed method has a golden response, which it cannot have"
    );

    let pinned_codes: std::collections::BTreeSet<u16> = golden_errors()
        .iter()
        .map(|(_, error)| error.code())
        .collect();
    let all_codes: std::collections::BTreeSet<u16> =
        esker_proto::error::code::ALL.into_iter().collect();
    assert_eq!(pinned_codes, all_codes, "an error code has no golden");
}

proptest! {
    /// Anything this build can build, it can read back — including the shapes a hand-written
    /// golden would not think of: empty keys, huge values, absurd limits.
    #[test]
    fn any_raw_kv_request_round_trips(
        region_id: u64,
        conf_ver: u64,
        version: u64,
        peer: u64,
        key in proptest::collection::vec(any::<u8>(), 0..64),
        value in proptest::collection::vec(any::<u8>(), 0..512),
        limit: u32,
        reverse: bool,
        sync: bool,
        which in 0usize..8,
    ) {
        let header = RequestHeader::new(region_id, Epoch::new(conf_ver, version), peer);
        let key = Bytes::from(key);
        let value = Bytes::from(value);
        let request = match which {
            0 => RawKvReq::Get { key },
            1 => RawKvReq::BatchGet { keys: vec![key, value] },
            2 => RawKvReq::Put { key, value, sync },
            3 => RawKvReq::BatchPut { pairs: vec![(key, value)], sync },
            4 => RawKvReq::Delete { key, sync },
            5 => RawKvReq::DeleteRange { start: key, end: value, sync },
            6 => RawKvReq::Scan { start: key, end: value, limit, reverse },
            _ => RawKvReq::CompareAndSwap {
                key,
                expected: reverse.then_some(value.clone()),
                value: sync.then_some(value),
                sync,
            },
        };
        let message = Request::raw_kv(header, request);
        prop_assert_eq!(Request::decode(&message.encode()).unwrap(), message);
    }

    /// The same sweep for the transaction service, including the shapes a golden would not
    /// think of: an empty key, an empty batch, a `commit_ts` below its `start_ts`.
    #[test]
    fn any_txn_kv_request_round_trips(
        region_id: u64,
        conf_ver: u64,
        version: u64,
        peer: u64,
        key in proptest::collection::vec(any::<u8>(), 0..64),
        value in proptest::collection::vec(any::<u8>(), 0..512),
        start_ts: u64,
        commit_ts: u64,
        ttl_ms: u64,
        limit: u32,
        reverse: bool,
        which in 0usize..8,
    ) {
        let header = RequestHeader::new(region_id, Epoch::new(conf_ver, version), peer);
        let key = Bytes::from(key);
        let value = Bytes::from(value);
        let request = match which {
            0 => TxnKvReq::Get { key, ts: start_ts },
            1 => TxnKvReq::Scan { start: key, end: value, limit, ts: start_ts, reverse },
            2 => TxnKvReq::Prewrite {
                start_ts,
                primary: key.clone(),
                ttl_ms,
                mutations: vec![
                    TxnMutation::Put { key: key.clone(), value, read_ts: None },
                    TxnMutation::Delete { key, read_ts: None },
                ],
            },
            3 => TxnKvReq::Commit { start_ts, commit_ts, keys: vec![key, value] },
            4 => TxnKvReq::Rollback { start_ts, keys: vec![key] },
            5 => TxnKvReq::ResolveLock { start_ts, commit_ts, keys: vec![] },
            6 => TxnKvReq::Heartbeat { start_ts, primary: key, ttl_ms },
            _ => TxnKvReq::GcSafepoint { safepoint: start_ts },
        };
        let message = Request::txn_kv(header, request);
        prop_assert_eq!(Request::decode(&message.encode()).unwrap(), message);
    }

    /// A lock is the one Percolator shape `esker-proto` carries, and it goes out through an
    /// error whose payload nothing else validates.
    #[test]
    fn any_lock_info_round_trips(
        key in proptest::collection::vec(any::<u8>(), 0..64),
        primary in proptest::collection::vec(any::<u8>(), 0..64),
        start_ts: u64,
        ttl_ms: u64,
    ) {
        let lock = LockInfo {
            key: Bytes::from(key),
            primary: Bytes::from(primary),
            start_ts,
            ttl_ms,
        };
        prop_assert_eq!(LockInfo::decode(&lock.encode()).unwrap(), lock.clone());
        prop_assert_eq!(LockInfo::from_error(&lock.into_error()).unwrap().unwrap(), lock);
    }

    /// Arbitrary bytes in a lock payload are an error, never a panic and never a lock made up
    /// out of noise.
    #[test]
    fn random_lock_payloads_never_panic(body in proptest::collection::vec(any::<u8>(), 0..256)) {
        let _ = LockInfo::decode(&body);
    }

    /// The fuzz case for the body decoder: whatever bytes a frame carried, decoding is a
    /// value or an error, never a panic (`CLAUDE.md` invariant 9).
    #[test]
    fn random_bodies_never_panic(body in proptest::collection::vec(any::<u8>(), 0..1024)) {
        let _ = Request::decode(&body);
        let _ = Response::decode(&body);
        let _ = ProtoError::decode(&body);
    }

    /// A plausible body is the harder case: a real message with a few bytes changed is where
    /// a decoder is most tempted to trust a length or a count.
    #[test]
    fn damaged_bodies_never_panic(
        which: prop::sample::Index,
        damage in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..8),
    ) {
        // Indexed against the list rather than a constant: a bound written by hand stops
        // covering the cases added after it, silently, which is how this sweep came to be
        // fuzzing twelve of thirty-one messages.
        let mut requests = golden_requests();
        let (_, request) = requests.swap_remove(which.index(requests.len()));
        let mut bytes = request.encode();
        for (at, value) in damage {
            let at = at % bytes.len();
            bytes[at] = value;
        }
        let _ = Request::decode(&bytes);
        let _ = Response::decode(&bytes);
        let _ = ProtoError::decode(&bytes);
    }
}

/// A scan with no limit is still a bounded answer, or one region's worth of keys would have to
/// fit in one frame.
#[test]
fn an_unlimited_scan_still_has_a_default_bound() {
    assert!(DEFAULT_SCAN_LIMIT > 0);
    let request = RawKvReq::scan(&b""[..], &b""[..], 0);
    match request {
        RawKvReq::Scan { limit, .. } => assert_eq!(limit, 0, "zero on the wire means the default"),
        other => panic!("{other:?}"),
    }
}
