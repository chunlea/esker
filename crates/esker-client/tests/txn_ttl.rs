//! The one fact this crate copies from the placement driver, checked against its source.
//!
//! `esker_client::TSO_LOGICAL_BITS` is a copy of `esker_pd::TSO_LOGICAL_BITS`: a client sits
//! above the placement driver in `CLAUDE.md`'s layer table and does not link it, but it has to
//! read a lock's age out of a timestamp to decide whether the lease has run out
//! (`docs/plans/phase-5.md` §10.2 — that judgement is the client's, because apply may not read
//! a clock). `esker-txn` keeps the same copy for the same reason, and
//! `crates/esker-txn/tests/tso_format.rs` is this file's sibling.
//!
//! A copy that drifts would not be subtle: every lock TTL would be out by a factor of 2^18, so
//! a lease of three seconds would be judged either immortal or already dead.
//!
//! `esker-pd` is a dev-dependency only. Nothing `esker-client` ships links it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_client::{LOCK_TTL_MS, TSO_LOGICAL_BITS, is_expired, physical_ms};

#[test]
fn the_timestamp_layout_still_matches_the_oracle() {
    assert_eq!(
        TSO_LOGICAL_BITS,
        esker_pd::TSO_LOGICAL_BITS,
        "the client's copy of the timestamp layout has drifted from PD's"
    );
}

/// And the reading itself agrees with the oracle's own composer, not merely with the constant.
#[test]
fn the_physical_reading_agrees_with_the_oracle() {
    for physical in [0u64, 1, 1_700_000_000_000, (1 << 46) - 1] {
        for logical in [0u64, 1, esker_pd::TSO_MAX_LOGICAL] {
            let ts = esker_pd::compose_ts(physical, logical);
            assert_eq!(physical_ms(ts), physical, "ts {ts}");
            assert_eq!(esker_pd::decompose_ts(ts).0, physical_ms(ts));
        }
    }
}

/// A lease spans the milliseconds the oracle counts, and every logical counter inside the last
/// one of them is still live.
#[test]
fn a_ttl_spans_the_milliseconds_the_oracle_counts() {
    let start = esker_pd::compose_ts(1_000, 5);

    let last_live = esker_pd::compose_ts(1_000 + LOCK_TTL_MS, esker_pd::TSO_MAX_LOGICAL);
    assert!(
        !is_expired(start, LOCK_TTL_MS, last_live),
        "a lock is live to the end of the last millisecond of its lease"
    );

    let first_dead = esker_pd::compose_ts(1_000 + LOCK_TTL_MS + 1, 0);
    assert!(is_expired(start, LOCK_TTL_MS, first_dead));
}

/// A timestamp *below* the lock's own is not evidence of anything, and must never read as
/// expired: a client whose oracle answer is behind would otherwise kill live transactions.
#[test]
fn a_timestamp_before_the_lock_is_never_expired() {
    let start = esker_pd::compose_ts(10_000, 0);
    assert!(!is_expired(start, 0, esker_pd::compose_ts(9_999, 0)));
    assert!(!is_expired(start, 0, start));
}

/// A lease long enough to overflow is a lease that never ends, not one that wraps into the
/// past. Saturating is the only reading that keeps the judgement conservative.
#[test]
fn an_absurd_ttl_does_not_wrap_into_the_past() {
    let start = esker_pd::compose_ts(1_000, 0);
    assert!(!is_expired(start, u64::MAX, u64::MAX));
}
