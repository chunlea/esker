//! **#79 — a scan answers for the whole range, however the store had to cut its batches.**
//!
//! A store's answer to one `Scan` is bounded twice: by the page the client asked for, and by a
//! byte budget (`esker_store::txnkv::MAX_SCAN_BYTES`) that no caller can see. So "fewer pairs
//! than I asked for" is **not** "that is all there is", and a client that read it that way handed
//! back a prefix of the range with nothing to notice. That is what run 127 attempt 4 lost five
//! catalog table records to: `a name points at table 34755, which is not there`, about a table
//! record that was on disk and readable the whole time.
//!
//! The contract, and it is what these two tests are: **only an empty batch ends a range.** The
//! store's half is that it checks its byte budget *after* pushing a pair, so a range holding any
//! live key at all answers with at least one — which is what makes an empty batch mean something.
//!
//! Against real stores and a real wire, because both halves are what is being asserted: the
//! client's paging, and the store's promise that an empty answer is the only terminal one.
//! `esker-store`'s own `a_scan_answers_with_a_subset` is the other half — the store's ceiling
//! counting dead keys — and it does not need a client.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use bytes::Bytes;
use esker_client::TxnClient;

#[path = "txn_cluster/mod.rs"]
mod txn_cluster;

use txn_cluster::{Cluster, Topology};

/// More live keys than one page carries, and more than the store's own `MAX_SCAN_LIMIT` of 8,192
/// so that neither bound can be the thing that happens to let this pass.
const KEYS: usize = 9_000;

/// Keys per transaction. One transaction of nine thousand is one Raft proposal of nine thousand
/// mutations; this is about the scan, so the writes are ordinary.
const PER_TXN: usize = 1_000;

/// Values big enough that a few of them cross `MAX_SCAN_BYTES` (4 MiB), and each one spills to
/// the `default` column family on its way — which is the path a real large row takes.
const BIG: usize = 128 * 1024;

/// Enough of them to cross the budget twice over, so the batch is cut in the middle rather than
/// at the last pair.
const BIG_KEYS: usize = 40;

fn cluster() -> std::sync::Arc<Cluster> {
    let cluster = Cluster::start(Topology::unreplicated(0x79_0001));
    assert!(cluster.settle(Duration::from_secs(20)), "the stores start");
    cluster
}

fn client(cluster: &Cluster) -> TxnClient {
    cluster
        .client_within(1, Duration::from_secs(20))
        .expect("a client")
}

/// All below `b"m"`, which is where `Topology::unreplicated` puts its boundary — so the whole
/// range is inside one region and what is being measured is the paging **inside** it, not the
/// region walk that has been there since ADR 0073.
fn key(at: usize) -> Bytes {
    Bytes::from(format!("k{at:08}"))
}

fn big_key(at: usize) -> Bytes {
    Bytes::from(format!("b{at:04}"))
}

fn write_batch(client: &TxnClient, keys: impl Iterator<Item = Bytes>, value: &[u8]) {
    let mut txn = client.begin().unwrap();
    for key in keys {
        txn.put(&key, value);
    }
    txn.commit().unwrap().expect("it wrote something");
}

/// **A range of nine thousand live keys comes back whole**, though one page carries a thousand
/// and the store will not put more than 8,192 in any single answer.
///
/// Before the paging, `scan(.., 0)` was one call for one page and the other 7,976 keys were
/// simply not in the answer — with the same type, the same `Ok`, and nothing to distinguish it
/// from a range that really did hold 1,024 keys.
#[test]
fn a_range_of_nine_thousand_live_keys_comes_back_whole() {
    let cluster = cluster();
    let client = client(&cluster);

    for chunk in (0..KEYS).step_by(PER_TXN) {
        write_batch(&client, (chunk..(chunk + PER_TXN).min(KEYS)).map(key), b"v");
    }

    let txn = client.begin().unwrap();
    let pairs = txn.scan(&key(0), b"l", 0).unwrap();

    let page = esker_client::wire::DEFAULT_SCAN_LIMIT as usize;
    println!(
        "  {KEYS} keys written · {} pairs read · page {page} · {} calls expected",
        pairs.len(),
        KEYS.div_ceil(page) + 1,
    );
    assert_eq!(
        pairs.len(),
        KEYS,
        "the scan stopped short of its range and said nothing"
    );
    // Not only the count: a paging walk that resumed from the wrong place would keep the count
    // and lose the keys between two batches.
    let found: Vec<Bytes> = pairs.into_iter().map(|(key, _)| key).collect();
    let want: Vec<Bytes> = (0..KEYS).map(key).collect();
    assert_eq!(found, want, "the keys are the ones written, in order");
}

/// **A batch the byte budget cut short is not the end of the range either.**
///
/// Forty values of 128 KiB is five megabytes, and `MAX_SCAN_BYTES` is four — so the store fills
/// one answer, stops mid-range for a reason the caller cannot see, and the rest has to come from
/// the next call. The caller's `limit` is never reached, so this is the hole a limit-shaped fix
/// leaves open on its own.
#[test]
fn a_batch_cut_by_the_byte_budget_is_not_the_end_of_the_range() {
    let cluster = cluster();
    let client = client(&cluster);
    let value = vec![b'x'; BIG];

    for chunk in (0..BIG_KEYS).step_by(8) {
        write_batch(
            &client,
            (chunk..(chunk + 8).min(BIG_KEYS)).map(big_key),
            &value,
        );
    }

    let txn = client.begin().unwrap();
    let pairs = txn.scan(&big_key(0), b"c", 0).unwrap();

    let bytes: usize = pairs
        .iter()
        .map(|(key, value)| key.len() + value.len())
        .sum();
    println!(
        "  {BIG_KEYS} values of {BIG} bytes · {} pairs read · {bytes} bytes · budget {}",
        pairs.len(),
        4 * 1024 * 1024,
    );
    assert_eq!(
        pairs.len(),
        BIG_KEYS,
        "the byte budget ended the scan instead of ending a batch"
    );
    assert!(
        bytes > 4 * 1024 * 1024,
        "the range has to be bigger than the budget or this test asserts nothing: {bytes} bytes"
    );
    let found: Vec<Bytes> = pairs.into_iter().map(|(key, _)| key).collect();
    let want: Vec<Bytes> = (0..BIG_KEYS).map(big_key).collect();
    assert_eq!(found, want);
}
