//! Random bytes from the operating system, and the version-4 UUID built from them.
//!
//! **No crate**, which is this project's standing rule for anything a few hundred lines long
//! (`CLAUDE.md`, the dependency policy). The bytes come from the same place a crate would take
//! them: the kernel's own pool, read through the filesystem.
//!
//! # Why the file and not a syscall
//!
//! `getrandom(2)` and `arc4random_buf(3)` are the direct calls, and reaching either needs `unsafe`
//! and a `libc` binding — a `*-sys` dependency the policy bans outright, or an `extern "C"` block
//! of our own with a `// SAFETY:` comment per platform. `/dev/urandom` is the same pool through an
//! interface `std` already has, on every platform this project targets, and it costs one open per
//! call rather than one syscall. A UUID is generated per row at most; this is not the hot path.

use std::io::Read as _;

use crate::error::{Result, SqlError};

/// Sixteen bytes from the OS random source.
///
/// A short read is an **error**, not a retry and not a pad: fewer bytes than asked for means the
/// pool did not answer, and a UUID built from a partly-zero buffer is one that could collide.
fn random_bytes() -> Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    let mut source = std::fs::File::open("/dev/urandom").map_err(|error| {
        SqlError::Internal(format!("the random source could not be opened: {error}"))
    })?;
    source
        .read_exact(&mut bytes)
        .map_err(|error| SqlError::Internal(format!("the random source was short: {error}")))?;
    Ok(bytes)
}

/// A `double precision` in `[0, 1)`, which is what `random()` answers.
///
/// **53 bits, not 64.** A `f64` has 53 bits of mantissa, so the low eleven bits of a 64-bit draw
/// cannot be represented and dividing by `2^64` would round some values to exactly `1.0` — outside
/// the half-open range PostgreSQL documents and this node's own corpus asserts (`random() < 1`).
/// Shifting first and scaling by `2^-53` gives every representable value in the range one chance.
pub fn random_f64() -> Result<f64> {
    let bytes = random_bytes()?;
    let draw = u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]);
    #[expect(
        clippy::cast_precision_loss,
        reason = "the shift leaves 53 bits, which is exactly what an f64 mantissa holds"
    )]
    Ok((draw >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0))
}

/// A version-4 UUID: sixteen random bytes, with the version and variant written over them.
///
/// **Two nibbles are not random**, and they are the two a formatter that only hyphenated random
/// bytes would get wrong: the high nibble of byte 6 is the version and is `4`, and the top two
/// bits of byte 8 are the variant and are `10`. That is why a value reads
/// `xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx` with `y` in `8`, `9`, `a` or `b` — measured on
/// PostgreSQL 19, at the 15th and 20th characters.
pub fn uuid_v4() -> Result<[u8; 16]> {
    let mut bytes = random_bytes()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes)
}

/// The Gregorian epoch a version-1 UUID counts from — 1582-10-15 00:00:00 UTC — as 100-ns units
/// *before* the Unix epoch.
///
/// The one number a v1 implementation cannot derive from anything else it has. It is the date the
/// Gregorian calendar took effect, chosen by RFC 4122 and by nothing in this project.
const GREGORIAN_OFFSET_100NS: u64 = 122_192_928_000_000_000;

/// The node id every version-1 UUID from this process carries, and the clock sequence beside it.
///
/// **A node with no MAC address uses a random one with the multicast bit set**, which is RFC 4122
/// §4.5 word for word and is the whole of this node's divergence from `uuid-ossp`: a real server
/// reads the host's card and its node id is therefore *unicast*, so `uuid_generate_v1()`'s
/// multicast bit is `f` there and `t` here. Declared in `tests/uuid_functions.rs`. The half that
/// matters is the half that agrees: the id is the **same for every call**, which is what tells a
/// `v1` from a `v1mc`.
static NODE: std::sync::OnceLock<[u8; 6]> = std::sync::OnceLock::new();
/// Random once per process, as RFC 4122 §4.1.5 asks: it is what keeps two processes that start in
/// the same 100-ns tick from making the same UUID.
static CLOCK_SEQ: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
/// The last tick handed out, so that two calls in one transaction cannot collide.
///
/// **A transaction has one timestamp** (invariant 6: the TSO's physical half is the only clock this
/// node reads), so the wall clock alone would give every row of one `INSERT` the same UUID — and
/// the column it defaults is a primary key. RFC 4122 §4.2.1.2 answers it directly: "the timestamp
/// can be simulated by keeping a counter that increments when the clock has not". That is this,
/// and it is why the *timestamp* advances rather than the clock sequence — a 14-bit sequence would
/// bound one transaction to 16384 rows.
static LAST_TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A version-1 UUID: a timestamp, a clock sequence and a node id.
///
/// `micros` is the transaction's own instant, from the same place `now()` reads it. `fresh_node`
/// is what tells `uuid_generate_v1mc` from `uuid_generate_v1`: the `mc` form draws a **new random
/// multicast node for every call** — measured, its node bytes differ between two calls where the
/// plain form's do not.
///
/// The layout is not derivable and each field was checked against a real server: the version `1`
/// is the high nibble of byte 6, the variant `10` is the top two bits of byte 8, and the timestamp
/// is split **low half first** — which is why two v1s made a moment apart do not sort in the order
/// they were made.
pub fn uuid_v1(micros: i64, fresh_node: bool) -> Result<[u8; 16]> {
    use std::sync::atomic::Ordering;

    let now = u64::try_from(i128::from(micros) * 10 + i128::from(GREGORIAN_OFFSET_100NS))
        .unwrap_or(GREGORIAN_OFFSET_100NS);
    // `max(now, last + 1)`: the clock when it has moved, the counter when it has not.
    let ticks = LAST_TICKS
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            Some(now.max(last.saturating_add(1)))
        })
        .map_or(now, |last| now.max(last.saturating_add(1)));

    let node = if fresh_node {
        multicast_node(random_bytes()?)
    } else {
        let mut seeded = None;
        if NODE.get().is_none() {
            seeded = Some(random_bytes()?);
        }
        *NODE.get_or_init(|| multicast_node(seeded.unwrap_or([0; 16])))
    };
    let clock_seq = if let Some(seq) = CLOCK_SEQ.get() {
        *seq
    } else {
        let bytes = random_bytes()?;
        *CLOCK_SEQ.get_or_init(|| u16::from_be_bytes([bytes[0], bytes[1]]) & 0x3fff)
    };

    let mut uuid = [0u8; 16];
    uuid[0..4].copy_from_slice(
        &u32::try_from(ticks & 0xffff_ffff)
            .unwrap_or(0)
            .to_be_bytes(),
    );
    uuid[4..6].copy_from_slice(
        &u16::try_from((ticks >> 32) & 0xffff)
            .unwrap_or(0)
            .to_be_bytes(),
    );
    let time_hi = u16::try_from((ticks >> 48) & 0x0fff).unwrap_or(0) | 0x1000;
    uuid[6..8].copy_from_slice(&time_hi.to_be_bytes());
    uuid[8] = u8::try_from(clock_seq >> 8).unwrap_or(0) | 0x80;
    uuid[9] = u8::try_from(clock_seq & 0xff).unwrap_or(0);
    uuid[10..16].copy_from_slice(&node);
    Ok(uuid)
}

/// Six bytes with the **multicast** bit — the low bit of the first one — set.
fn multicast_node(bytes: [u8; 16]) -> [u8; 6] {
    let mut node = [0u8; 6];
    node.copy_from_slice(&bytes[0..6]);
    node[0] |= 0x01;
    node
}

#[cfg(test)]
mod tests {
    use super::uuid_v4;

    /// The node id is the **same for every `uuid_generate_v1`** and **new for every
    /// `uuid_generate_v1mc`**, which is the whole difference between the two functions.
    ///
    /// Here rather than in the corpus because a corpus asks what both servers can answer, and this
    /// node has no `substring` to cut the node bytes out of the text with. Measured on a real
    /// server all the same — `substring(uuid_generate_v1()::text, 25, 12)` is equal across two
    /// calls there and `uuid_generate_v1mc`'s is not.
    #[test]
    fn a_v1_keeps_its_node_id_and_a_v1mc_draws_a_new_one() {
        let (first, second) = (
            super::uuid_v1(0, false).unwrap(),
            super::uuid_v1(0, false).unwrap(),
        );
        assert_eq!(
            first[10..16],
            second[10..16],
            "a v1's node id is the process's"
        );
        let (mc1, mc2) = (
            super::uuid_v1(0, true).unwrap(),
            super::uuid_v1(0, true).unwrap(),
        );
        assert_ne!(mc1[10..16], mc2[10..16], "a v1mc draws a node id per call");
        // Both are version 1, RFC variant, and multicast — this node has no MAC to be unicast
        // with.
        for uuid in [first, second, mc1, mc2] {
            assert_eq!(uuid[6] & 0xf0, 0x10, "the version nibble is 1");
            assert_eq!(uuid[8] & 0xc0, 0x80, "the variant bits are 10");
            assert_eq!(
                uuid[10] & 0x01,
                0x01,
                "the node id is multicast (RFC 4122 §4.5)"
            );
        }
    }

    /// **Two calls in one transaction differ**, which the wall clock alone cannot give them: a
    /// transaction has one timestamp here (invariant 6), and the column `uuid_test.rb` defaults
    /// from `uuid_generate_v1()` is a primary key. The tick counter is what separates them, and
    /// it is why the timestamp advances rather than the 14-bit clock sequence — that would bound
    /// one transaction to 16384 rows.
    #[test]
    fn two_version_ones_from_one_instant_are_two_values() {
        let mut seen = BTreeSet::new();
        for _ in 0..4096 {
            assert!(seen.insert(super::uuid_v1(1_700_000_000_000_000, false).unwrap()));
        }
    }
    use std::collections::BTreeSet;

    /// `random()` stays inside the half-open range, over enough draws to catch an endpoint.
    #[test]
    fn a_random_double_is_in_the_half_open_unit_range() {
        let mut seen = BTreeSet::new();
        for _ in 0..256 {
            let draw = super::random_f64().unwrap();
            assert!((0.0..1.0).contains(&draw), "{draw} is outside [0, 1)");
            seen.insert(draw.to_bits());
        }
        assert_eq!(seen.len(), 256, "256 draws, 256 different values");
    }

    /// The two nibbles that are not random, and the fourteen bytes that are.
    #[test]
    fn the_version_and_variant_are_written_over_the_randomness() {
        let mut seen = BTreeSet::new();
        for _ in 0..256 {
            let bytes = uuid_v4().unwrap();
            assert_eq!(bytes[6] >> 4, 4, "the version nibble");
            assert_eq!(bytes[8] >> 6, 0b10, "the variant bits");
            seen.insert(bytes);
        }
        assert_eq!(seen.len(), 256, "256 draws, 256 different values");
    }
}
