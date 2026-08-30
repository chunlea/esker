//! PD's records and keys, pinned byte for byte.
//!
//! These are **on-disk formats** (`docs/plans/phase-4-pd.md` §3): a change to any byte here
//! makes every existing PD database unreadable, so it needs a version bump, an ADR and a
//! migration — never a quiet edit. The golden file was written from the format description by
//! a separate encoder, so this checks the implementation rather than agreeing with it.
//!
//! The keys are pinned too, and for a sharper reason than the records: a key layout that
//! drifts does not fail to decode, it simply stops finding what is already there.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use esker_pd::keys;
use esker_pd::record::{
    AllocRecord, ClusterRecord, RegionRecord, StoreRecord, StoreStats, TsoRecord,
    decode_range_entry, encode_range_entry,
};
use esker_proto::{Epoch, Peer, PeerRole, Region};

const GOLDEN: &str = include_str!("golden/records.hex");

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn golden(kind: &str, name: &str) -> String {
    let prefix = format!("{kind} {name} ");
    GOLDEN
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` line in the golden file"))
        .trim()
        .to_owned()
}

fn store() -> StoreRecord {
    StoreRecord {
        store_id: 3,
        address: "127.0.0.1:20160".to_owned(),
        started_ms: 1_700_000_000_000,
        last_heartbeat_ms: 1_700_000_010_000,
        stats: StoreStats {
            capacity: 1 << 40,
            available: 1 << 39,
            region_count: 12,
            leader_count: 4,
            applied_bytes: 999,
        },
    }
}

fn region_full() -> RegionRecord {
    RegionRecord {
        region: Region {
            id: 7,
            start_key: Bytes::from_static(b"aaa"),
            end_key: Bytes::from_static(b"mmm"),
            peers: vec![
                Peer::voter(1, 10),
                Peer {
                    store_id: 2,
                    peer_id: 11,
                    role: PeerRole::Learner,
                },
            ],
            epoch: Epoch::new(3, 4),
        },
        leader_peer_id: 10,
        term: 5,
        approximate_size: 96 << 20,
        applied_index: 4242,
        last_heartbeat_ms: 1_700_000_020_000,
    }
}

#[test]
fn every_record_matches_its_golden_bytes() {
    let cluster = ClusterRecord {
        cluster_id: 0x0123_4567_89AB_CDEF,
        first_region_id: 1,
        created_ms: 1_700_000_000_000,
    };
    assert_eq!(hex(&cluster.encode()), golden("record", "cluster"));

    let alloc = AllocRecord {
        allocated_end: 1_000,
    };
    assert_eq!(hex(&alloc.encode()), golden("record", "alloc"));

    let tso = TsoRecord {
        high_water_ms: 1_700_000_003_000,
    };
    assert_eq!(hex(&tso.encode()), golden("record", "tso"));

    assert_eq!(hex(&store().encode()), golden("record", "store"));

    let bootstrap = RegionRecord::new(Region::bootstrap(1, 1, 1), 0);
    assert_eq!(
        hex(&bootstrap.encode()),
        golden("record", "region-bootstrap")
    );
    assert_eq!(
        hex(&region_full().encode()),
        golden("record", "region-full")
    );
    assert_eq!(hex(&encode_range_entry(7)), golden("record", "range-entry"));
}

/// The bytes in the file must also *decode*, or the golden would only prove that two encoders
/// agree — not that anything can read what they wrote.
#[test]
fn the_golden_bytes_decode_to_the_records_that_made_them() {
    fn bytes(name: &str) -> Vec<u8> {
        let text = golden("record", name);
        assert!(text.len() % 2 == 0, "odd-length hex for `{name}`");
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("bad hex"))
            .collect()
    }

    assert_eq!(
        ClusterRecord::decode(&bytes("cluster")).unwrap().cluster_id,
        0x0123_4567_89AB_CDEF
    );
    assert_eq!(
        AllocRecord::decode(&bytes("alloc")).unwrap().allocated_end,
        1_000
    );
    assert_eq!(
        TsoRecord::decode(&bytes("tso")).unwrap().high_water_ms,
        1_700_000_003_000
    );
    assert_eq!(StoreRecord::decode(&bytes("store")).unwrap(), store());
    assert_eq!(
        RegionRecord::decode(&bytes("region-full")).unwrap(),
        region_full()
    );
    assert_eq!(
        RegionRecord::decode(&bytes("region-bootstrap")).unwrap(),
        RegionRecord::new(Region::bootstrap(1, 1, 1), 0)
    );
    assert_eq!(decode_range_entry(&bytes("range-entry")).unwrap(), 7);
}

/// A key layout that drifts does not fail loudly: it stops finding records that are already
/// there, and PD comes up looking like an empty cluster.
#[test]
fn every_key_matches_its_golden_bytes() {
    assert_eq!(hex(&keys::cluster_key()), golden("key", "cluster"));
    assert_eq!(hex(&keys::alloc_key()), golden("key", "alloc"));
    assert_eq!(hex(&keys::tso_key()), golden("key", "tso"));
    assert_eq!(hex(&keys::store_key(3)), golden("key", "store-3"));
    assert_eq!(hex(&keys::region_key(7)), golden("key", "region-7"));
    assert_eq!(
        hex(&keys::range_key(b"mmm")),
        golden("key", "range-bounded-mmm")
    );
    assert_eq!(hex(&keys::range_key(b"")), golden("key", "range-unbounded"));
    assert_eq!(
        hex(&keys::range_seek_key(b"mmm")),
        golden("key", "range-seek-mmm")
    );
}
