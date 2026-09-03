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

#[cfg(test)]
mod tests {
    use super::uuid_v4;
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
