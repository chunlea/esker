//! Engine keys for the three Percolator column families (`docs/txn-spec.md` §1).
//!
//! Every function here takes a **user key** and returns the bytes the engine is indexed by.
//! The direction is one-way on purpose: a user key crosses the wire, sits in a lock's
//! `primary` field and is what a client passes; the `'x'` prefix and the version suffix are
//! this layer's, applied on the way down and stripped on the way up, exactly as the store
//! rather than the client applies `'r'` to `RawKV` keys (`docs/DESIGN.md` §10).
//!
//! # The user key is group-encoded first
//!
//! `docs/DESIGN.md` §3 writes the layout as `'x' ++ user_key ++ enc_ts`, which is correct only
//! when the user key is prefix-free. Arbitrary `TxnKV` keys are not: with the raw form the
//! versions of `"a"` interleave with those of `"ab"`, and the prefix check that should catch it
//! passes, because `"a"` *is* a prefix of `"ab"`. `docs/txn-spec.md` §2 works the bytes
//! through. So the user key goes through [`esker_keys::encode_bytes`] — the memcomparable
//! group encoding, which is prefix-free — before the timestamp is appended, and each key's
//! versions form one contiguous run.
//!
//! `esker_keys::prefix::txn_key` is the raw form and is deliberately not used here.
//!
//! # Newer sorts first
//!
//! The suffix is `enc_ts` — the timestamp's bitwise complement — so the versions of one key
//! run newest to oldest and "the newest version at or below `ts`" is a **forward seek** to
//! [`seek_write`] rather than a scan. The boundary is inclusive: a version committed at
//! exactly `ts` is visible at `ts`, because the two encodings are equal and the seek lands on
//! it. That case has its own test, because getting the direction wrong reads the *oldest*
//! version instead of the newest and every test that uses one version still passes.

use esker_keys::codec::{self, CodecError};
use esker_keys::prefix::{TS_SUFFIX_LEN, TXN};

use crate::error::{Result, TxnError};

/// `'x' ++ enc(user_key)` — the unversioned key a lock is stored under.
///
/// It is also the prefix every version of the key shares, which is why the versioned builders
/// below are this plus eight bytes.
#[must_use]
pub fn lock(user_key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + codec::encoded_bytes_len(user_key.len()));
    out.push(TXN);
    codec::encode_bytes(user_key, &mut out);
    out
}

/// The prefix every version of `user_key` shares. Identical to [`lock`]; named separately
/// because the two uses are different and a reader should not have to infer that they coincide.
#[must_use]
pub fn prefix(user_key: &[u8]) -> Vec<u8> {
    lock(user_key)
}

/// `'x' ++ enc(user_key) ++ !commit_ts` — a record in the `write` column family.
#[must_use]
pub fn write(user_key: &[u8], commit_ts: u64) -> Vec<u8> {
    versioned(user_key, commit_ts)
}

/// `'x' ++ enc(user_key) ++ !start_ts` — a value in the `default` column family.
///
/// Versioned by `start_ts`, not `commit_ts`: the value is written at prewrite, before any
/// commit timestamp exists, and the `write` record's `start_ts` field is the link back to it.
#[must_use]
pub fn value(user_key: &[u8], start_ts: u64) -> Vec<u8> {
    versioned(user_key, start_ts)
}

/// Where a read at `ts` starts looking in the `write` column family.
///
/// The first key at or after this one, still under [`prefix`], is the newest version with
/// `commit_ts <= ts`. Same bytes as [`write()`]; a separate name because the two are asking
/// opposite questions and a future change to one is unlikely to be right for the other.
#[must_use]
pub fn seek_write(user_key: &[u8], ts: u64) -> Vec<u8> {
    versioned(user_key, ts)
}

/// The half-open range holding every version of `user_key`: newest possible to oldest.
///
/// `[prefix ++ !u64::MAX, prefix ++ 0xff*8]` is inclusive at both ends, so the upper bound
/// returned here is the *exclusive* one — the prefix's successor.
#[must_use]
pub fn version_range(user_key: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let start = versioned(user_key, u64::MAX);
    let mut end = prefix(user_key);
    // A prefix always ends in the group marker of its final group, and that marker is
    // `0xff - padding` with at least one byte of padding — never `0xff`. So incrementing the
    // last byte cannot carry, and the successor of the prefix is the prefix with one byte
    // changed. `if let` rather than an index: a prefix is never empty, but saying so with a
    // panic would put one on a path that takes bytes from a caller.
    if let Some(marker) = end.last_mut() {
        debug_assert!(*marker < u8::MAX, "the final group marker is never 0xff");
        *marker = marker.saturating_add(1);
    }
    (start, end)
}

/// Splits an engine key back into its user key and its timestamp.
///
/// The inverse of [`write()`] and [`value()`]. Used by the compaction filter and by tooling; the
/// read path never needs it, because it seeks to a key it built itself.
pub fn split(engine_key: &[u8]) -> Result<(Vec<u8>, u64)> {
    let rest = engine_key.strip_prefix(&[TXN]).ok_or_else(|| {
        TxnError::corrupt(
            "key",
            format!("not in the 'x' namespace: {engine_key:02x?}"),
        )
    })?;
    let (user_key, rest) = codec::decode_bytes(rest).map_err(|error| bad_key(&error))?;
    if rest.len() != TS_SUFFIX_LEN {
        return Err(TxnError::corrupt(
            "key",
            format!(
                "{} bytes after the user key, want {TS_SUFFIX_LEN}",
                rest.len()
            ),
        ));
    }
    let ts = codec::dec_ts(rest).map_err(|error| bad_key(&error))?;
    Ok((user_key, ts))
}

/// Splits the unversioned key a lock is stored under.
pub fn split_lock(engine_key: &[u8]) -> Result<Vec<u8>> {
    let rest = engine_key.strip_prefix(&[TXN]).ok_or_else(|| {
        TxnError::corrupt(
            "key",
            format!("not in the 'x' namespace: {engine_key:02x?}"),
        )
    })?;
    let (user_key, rest) = codec::decode_bytes(rest).map_err(|error| bad_key(&error))?;
    if !rest.is_empty() {
        return Err(TxnError::corrupt(
            "key",
            format!("{} bytes after an unversioned key", rest.len()),
        ));
    }
    Ok(user_key)
}

fn versioned(user_key: &[u8], ts: u64) -> Vec<u8> {
    let mut out = prefix(user_key);
    out.extend_from_slice(&codec::enc_ts(ts));
    out
}

fn bad_key(error: &CodecError) -> TxnError {
    TxnError::corrupt("key", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{lock, prefix, seek_write, split, split_lock, value, version_range, write};

    /// The trap this module exists for. With the raw layout of `docs/DESIGN.md` §3 the
    /// versions of `"a"` and `"ab"` interleave; group-encoding first makes each key's versions
    /// one contiguous run, whatever the timestamps are.
    #[test]
    fn a_prefix_of_another_key_does_not_interleave() {
        let a_oldest = write(b"a", 0);
        let a_newest = write(b"a", u64::MAX);
        let longer_oldest = write(b"ab", 0);
        let longer_newest = write(b"ab", u64::MAX);

        assert!(a_newest < a_oldest, "newer sorts first");
        assert!(longer_newest < longer_oldest);
        // Every version of "a" is below every version of "ab" — the whole point.
        assert!(
            a_oldest < longer_newest,
            "\"a\"@0 must sort below \"ab\"@MAX"
        );

        // And the raw layout really does fail it, so the encoding is not cargo cult.
        let raw_a_oldest = esker_keys::prefix::txn_key(b"a", 0);
        let raw_ab_newest = esker_keys::prefix::txn_key(b"ab", u64::MAX);
        assert!(
            raw_a_oldest > raw_ab_newest,
            "the raw layout is what this module encodes around"
        );
    }

    /// A key's versions must never be confusable with the *lock* on a neighbouring key. They
    /// live in different column families, so this only has to hold within one.
    #[test]
    fn a_versioned_key_is_the_prefix_plus_eight() {
        let p = prefix(b"account/1");
        let w = write(b"account/1", 42);
        assert_eq!(w.len(), p.len() + 8);
        assert!(w.starts_with(&p));
        assert_eq!(lock(b"account/1"), p);
    }

    /// The boundary that decides whether a read at `ts` sees a commit at `ts`. It must: a
    /// snapshot at `ts` includes everything committed at or before `ts`.
    #[test]
    fn the_seek_boundary_includes_a_commit_at_exactly_the_read_ts() {
        let at_ts = write(b"k", 100);
        assert_eq!(seek_write(b"k", 100), at_ts, "the seek lands on it");

        // Older commits sort after the seek point, so a forward scan reaches them next.
        assert!(write(b"k", 99) > seek_write(b"k", 100));
        // Newer ones sort before it, so a forward scan never returns them.
        assert!(write(b"k", 101) < seek_write(b"k", 100));
    }

    /// The version range has to cover every timestamp and stop before the next key.
    #[test]
    fn the_version_range_covers_exactly_one_key() {
        let (start, end) = version_range(b"k");
        assert!(start <= write(b"k", u64::MAX));
        for ts in [0u64, 1, 42, u64::MAX] {
            let key = write(b"k", ts);
            assert!(start <= key && key < end, "ts {ts} is outside the range");
        }
        assert!(write(b"j", 0) < start);
        assert!(
            end <= prefix(b"k\x00"),
            "the range stops before the next key"
        );
    }

    #[test]
    fn keys_round_trip() {
        for user_key in [b"".as_slice(), b"k", b"account/1", &[0xff; 40]] {
            for ts in [0u64, 1, 1 << 41, u64::MAX] {
                let (back, got) = split(&write(user_key, ts)).unwrap();
                assert_eq!(back, user_key);
                assert_eq!(got, ts);
                assert_eq!(
                    split(&value(user_key, ts)).unwrap(),
                    (user_key.to_vec(), ts)
                );
            }
            assert_eq!(split_lock(&lock(user_key)).unwrap(), user_key);
        }
    }

    /// Bytes off disk are not trusted: every malformed shape is an error, never a panic and
    /// never a wrong answer (`CLAUDE.md` invariant 9).
    #[test]
    fn a_malformed_key_is_an_error() {
        assert!(split(b"").is_err(), "empty");
        assert!(split(b"r\x00").is_err(), "wrong namespace");
        assert!(split(b"x").is_err(), "no user key");
        assert!(split(&lock(b"k")).is_err(), "no timestamp suffix");

        let mut too_long = write(b"k", 1);
        too_long.push(0);
        assert!(split(&too_long).is_err(), "nine bytes of suffix");

        assert!(split_lock(&write(b"k", 1)).is_err(), "a version on a lock");

        // A group marker that claims more padding than a group holds.
        let mut bad_marker = lock(b"k");
        *bad_marker.last_mut().unwrap() = 0x00;
        assert!(split_lock(&bad_marker).is_err(), "bad group marker");
    }

    /// An empty user key is a legal key and must not collide with anything.
    #[test]
    fn the_empty_key_is_a_key() {
        assert_ne!(lock(b""), lock(b"\x00"));
        assert!(lock(b"") < lock(b"\x00"));
        assert_eq!(split(&write(b"", 7)).unwrap(), (Vec::new(), 7));
    }
}
