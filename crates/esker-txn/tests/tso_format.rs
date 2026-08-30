//! The one fact `esker-txn` copies from the placement driver, checked against its source.
//!
//! `esker_txn::TSO_LOGICAL_BITS` is a copy of `esker_pd::TSO_LOGICAL_BITS`: transactions sit
//! above the placement driver in `CLAUDE.md`'s layer table, so the crate cannot link it and
//! must restate the timestamp layout to read a lock's age out of one. A copy that drifts is
//! the failure this file exists to catch, and it would not be subtle — every lock TTL would be
//! wrong by a factor of 2^18.
//!
//! `esker-pd` is a dev-dependency only. Nothing `esker-txn` ships links it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[test]
fn the_timestamp_layout_still_matches_the_oracle() {
    assert_eq!(
        esker_txn::TSO_LOGICAL_BITS,
        esker_pd::TSO_LOGICAL_BITS,
        "esker-txn's copy of the timestamp layout has drifted from PD's"
    );
}

/// And the reading itself agrees with the oracle's own composer, not merely with the constant.
#[test]
fn the_physical_reading_agrees_with_the_oracle() {
    for physical in [0u64, 1, 1_700_000_000_000, (1 << 46) - 1] {
        for logical in [0u64, 1, esker_pd::TSO_MAX_LOGICAL] {
            let ts = esker_pd::compose_ts(physical, logical);
            assert_eq!(esker_txn::physical_ms(ts), physical, "ts {ts}");
            assert_eq!(esker_pd::decompose_ts(ts).0, esker_txn::physical_ms(ts));
        }
    }
}

/// A lock minted at one timestamp and judged against another, in the units PD hands out.
#[test]
fn a_ttl_spans_the_milliseconds_the_oracle_counts() {
    let start = esker_pd::compose_ts(1_000, 5);
    let ttl_ms = esker_txn::LOCK_TTL_MS;

    // Every logical counter inside the last millisecond of the lease is still live.
    let last_live = esker_pd::compose_ts(1_000 + ttl_ms, esker_pd::TSO_MAX_LOGICAL);
    assert!(!esker_txn::is_expired(start, ttl_ms, last_live));

    let first_dead = esker_pd::compose_ts(1_000 + ttl_ms + 1, 0);
    assert!(esker_txn::is_expired(start, ttl_ms, first_dead));
}
