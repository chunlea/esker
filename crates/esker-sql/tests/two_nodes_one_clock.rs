//! **Two SQL nodes must not hand out the same timestamps** — `CLAUDE.md` invariant 6, *"timestamps
//! come only from PD's TSO. No node uses its wall clock for ordering."*
//!
//! The harness beside this one already says what the rule is, in `cluster/mod.rs`'s
//! `another_client`: a second node gets *"its own connections and its own region cache, which is
//! what makes it another node — **over the same oracle**, which is what two nodes against one TSO
//! have. Two independent counters would hand the same timestamp to two different transactions,
//! which is not a second node but a broken cluster."*
//!
//! Until 2026-09-10 the **binary** did not do that. `esker-sql.rs`'s `connect` built
//! `CountingOracle::starting_at(1)` whatever `--pd` said and handed it to the `TxnClient` that
//! drives Percolator, so two `esker-sql` processes against one cluster were exactly the
//! arrangement that comment calls a broken cluster. Debt #51.
//!
//! Both halves are here: that the driver's clock keeps two nodes apart, and what a local counter
//! costs instead.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod standin_pd;

use std::sync::Arc;

use esker_client::{CountingOracle, TimestampOracle};
use esker_sql::pd::PdConn;

/// Eight timestamps from one oracle, in order.
fn eight(oracle: &dyn TimestampOracle) -> Vec<u64> {
    (0..8).map(|_| oracle.timestamp().unwrap()).collect()
}

/// **Two nodes on one driver never share a timestamp.**
///
/// The regression for #51, at the level the fix lives: `PdConn` is the node's connection to the
/// driver *and* its oracle, so two nodes are two connections to one counter. A `start_ts` is what
/// every MVCC decision in this system is made against — visibility, first-committer-wins, lock
/// ownership — so two live transactions holding the same one is not a degraded mode, it is the
/// ordering gone.
#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_on_one_driver_never_share_a_timestamp() {
    let (_pd, _handle, address) = standin_pd::serve().await;

    tokio::task::block_in_place(|| {
        let left = Arc::new(PdConn::new(address));
        let right = Arc::new(PdConn::new(address));

        let mine = eight(left.as_ref());
        let theirs = eight(right.as_ref());

        let shared: Vec<u64> = mine
            .iter()
            .filter(|ts| theirs.contains(ts))
            .copied()
            .collect();
        assert!(
            shared.is_empty(),
            "two nodes of one cluster were handed the same timestamp: {shared:?}\n\
             left {mine:?}\nright {theirs:?}"
        );
        // And each node's own are strictly increasing, which is the other half of an ordering.
        for run in [&mine, &theirs] {
            assert!(
                run.windows(2).all(|pair| pair[0] < pair[1]),
                "one node's timestamps did not increase: {run:?}"
            );
        }
        assert!(
            mine.iter().chain(&theirs).all(|ts| *ts > 0),
            "a timestamp is never zero (`docs/txn-spec.md` §5.5)"
        );
    });
}

/// **And why a local counter cannot stand in for it**, which is the fact #51 was opened on.
///
/// Not a hypothetical and not a test of `CountingOracle`'s arithmetic: this is the arrangement the
/// binary shipped — each node its own counter from one — and the collision is immediate and total.
/// Kept as a test so that anyone tempted to give a node its own clock again sees the cost in one
/// line rather than deducing it.
#[test]
fn two_local_counters_collide_from_their_first_timestamp() {
    let left = CountingOracle::starting_at(1);
    let right = CountingOracle::starting_at(1);
    assert_eq!(
        eight(&left),
        eight(&right),
        "two independent counters are not two clocks, they are one clock read twice — which is why \
         a node with `--pd` must ask the driver"
    );
}
