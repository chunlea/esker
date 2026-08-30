//! CRC32C — the Castagnoli CRC used by every checksummed byte in Esker.
//!
//! `CLAUDE.md` invariant 2 says every on-disk byte is checksummed; this is the function that
//! does it. It is the standard CRC-32/ISCSI: reflected input and output, initial and final
//! value `0xFFFF_FFFF`, reflected polynomial `0x82F6_3B78` (normal form `0x1EDC_6F41`).
//!
//! Two implementations are compiled from the same tests:
//!
//! * a portable slicing-by-8 table version, which consumes eight bytes per iteration;
//! * a hardware version using the CRC32C instruction, selected at compile time by
//!   `cfg(target_feature)` — never by runtime detection, so that a build is reproducible and
//!   the fast path is not a hidden branch.
//!
//! The two agree byte for byte, and a test checks that on random inputs at every length
//! class whenever the hardware path is compiled in.

/// The reflected Castagnoli polynomial. Part of the on-disk format: changing it invalidates
/// every checksum ever written.
pub const POLYNOMIAL: u32 = 0x82F6_3B78;

/// True when this build compiled the hardware CRC32C path. Diagnostic only — both paths
/// produce identical values.
pub const HAS_HARDWARE: bool = cfg!(any(
    all(target_arch = "aarch64", target_feature = "crc"),
    all(target_arch = "x86_64", target_feature = "sse4.2")
));

/// Slicing-by-8 lookup tables, built at compile time so there is no initialisation order to
/// get wrong and no lazy static to synchronise.
static TABLES: [[u32; 256]; 8] = build_tables();

// The loop bounds are 256 and 8, so the `as` casts below cannot truncate.
#[allow(clippy::cast_possible_truncation)]
const fn build_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];

    let mut byte = 0usize;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][byte] = crc;
        byte += 1;
    }

    // Table `k` answers "what does this byte contribute once `k` more bytes follow it".
    let mut byte = 0usize;
    while byte < 256 {
        let mut k = 1usize;
        while k < 8 {
            let prev = tables[k - 1][byte];
            tables[k][byte] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            k += 1;
        }
        byte += 1;
    }

    tables
}

/// The CRC32C of `data`.
#[must_use]
pub fn checksum(data: &[u8]) -> u32 {
    update(0, data)
}

/// Continues a CRC over another slice: `update(update(0, a), b) == checksum([a, b].concat())`.
///
/// Records are checksummed in pieces (a header, then a payload), so this is the primary
/// entry point and [`checksum`] is the convenience wrapper.
#[must_use]
pub fn update(prev: u32, data: &[u8]) -> u32 {
    #[cfg(any(
        all(target_arch = "aarch64", target_feature = "crc"),
        all(target_arch = "x86_64", target_feature = "sse4.2")
    ))]
    {
        update_hardware(prev, data)
    }
    #[cfg(not(any(
        all(target_arch = "aarch64", target_feature = "crc"),
        all(target_arch = "x86_64", target_feature = "sse4.2")
    )))]
    {
        update_software(prev, data)
    }
}

/// The portable slicing-by-8 implementation.
///
/// Always compiled, on every target, so it is always tested: it is the specification that
/// the hardware path is checked against. Prefer [`update`], which picks the faster path.
#[must_use]
pub fn update_software(prev: u32, data: &[u8]) -> u32 {
    // The running state is the bitwise complement of the reported CRC.
    let mut crc = !prev;

    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let low = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ crc;
        let high = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        crc = TABLES[7][byte_of(low, 0)]
            ^ TABLES[6][byte_of(low, 1)]
            ^ TABLES[5][byte_of(low, 2)]
            ^ TABLES[4][byte_of(low, 3)]
            ^ TABLES[3][byte_of(high, 0)]
            ^ TABLES[2][byte_of(high, 1)]
            ^ TABLES[1][byte_of(high, 2)]
            ^ TABLES[0][byte_of(high, 3)];
    }
    for &byte in chunks.remainder() {
        crc = (crc >> 8) ^ TABLES[0][byte_of(crc ^ u32::from(byte), 0)];
    }

    !crc
}

/// Byte `index` (0 = least significant) of `word`, as a table index.
#[inline]
fn byte_of(word: u32, index: u32) -> usize {
    ((word >> (index * 8)) & 0xFF) as usize
}

/// The `ARMv8` CRC32C instruction path.
///
/// Selected only when `target_feature = "crc"` is statically enabled, so there is no runtime
/// feature check and no undefined behaviour hiding behind one.
///
/// The `unsafe` here is the one relaxation of `CLAUDE.md` invariant 8 in this crate: the
/// intrinsics are `#[target_feature]` functions, and rustc requires the call to be marked
/// even when the feature is statically enabled for the whole compilation unit.
#[cfg(all(target_arch = "aarch64", target_feature = "crc"))]
#[allow(
    unsafe_code,
    reason = "std::arch intrinsics; the cfg above proves the precondition"
)]
#[must_use]
fn update_hardware(prev: u32, data: &[u8]) -> u32 {
    use std::arch::aarch64::{__crc32cb, __crc32cd};

    let mut crc = !prev;
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        // SAFETY: `__crc32cd` requires the `crc` target feature, which the `cfg` on this
        // function guarantees is enabled for this compilation unit. The intrinsic reads no
        // memory and has no other precondition.
        crc = unsafe { __crc32cd(crc, word) };
    }
    for &byte in chunks.remainder() {
        // SAFETY: as above.
        crc = unsafe { __crc32cb(crc, byte) };
    }
    !crc
}

/// The SSE4.2 CRC32C instruction path. See the aarch64 version above for why this is a
/// compile-time choice.
#[cfg(all(target_arch = "x86_64", target_feature = "sse4.2"))]
#[allow(
    unsafe_code,
    reason = "std::arch intrinsics; the cfg above proves the precondition"
)]
#[must_use]
fn update_hardware(prev: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

    let mut crc = u64::from(!prev);
    let mut chunks = data.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        // SAFETY: `_mm_crc32_u64` requires SSE4.2, which the `cfg` on this function
        // guarantees is enabled for this compilation unit. It reads no memory.
        crc = unsafe { _mm_crc32_u64(crc, word) };
    }
    for &byte in chunks.remainder() {
        // SAFETY: as above.
        crc = u64::from(unsafe { _mm_crc32_u8(crc as u32, byte) });
    }
    // The instruction leaves the result in the low 32 bits.
    !(crc as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Pcg32;

    /// Published CRC-32/ISCSI vectors. `"123456789"` is the check value from the CRC
    /// catalogue; the four 32-byte cases are from RFC 3720 appendix B.4. These are the
    /// reason this module is trustworthy — a property test alone would happily agree with a
    /// wrong polynomial.
    #[test]
    fn matches_published_vectors() {
        assert_eq!(checksum(b""), 0x0000_0000);
        assert_eq!(checksum(b"a"), 0xC1D0_4330);
        assert_eq!(checksum(b"123456789"), 0xE306_9283);
        assert_eq!(
            checksum(b"The quick brown fox jumps over the lazy dog"),
            0x2262_0404
        );
        assert_eq!(checksum(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(checksum(&[0xFFu8; 32]), 0x62A8_AB43);

        let mut ascending = [0u8; 32];
        let mut descending = [0u8; 32];
        for i in 0..32u8 {
            ascending[i as usize] = i;
            descending[i as usize] = 31 - i;
        }
        assert_eq!(checksum(&ascending), 0x46DD_794E);
        assert_eq!(checksum(&descending), 0x113F_DB5C);
    }

    /// The software path is the specification; the hardware path must not drift from it.
    /// This is also the test that exercises the `unsafe`-adjacent intrinsic code required by
    /// `CLAUDE.md` invariant 8.
    #[test]
    fn hardware_agrees_with_software() {
        let mut rng = Pcg32::new(0xE306_9283, 1);
        let mut buffer = vec![0u8; 4096];
        rng.fill_bytes(&mut buffer);

        // Every length class matters: full 8-byte blocks plus every possible remainder.
        for len in 0..=300 {
            let slice = &buffer[..len];
            assert_eq!(
                update(0, slice),
                update_software(0, slice),
                "dispatch disagrees with the table implementation at len {len}"
            );
        }
        assert_eq!(update(0, &buffer), update_software(0, &buffer));
    }

    /// Records are checksummed in pieces, so splitting must not change the answer.
    #[test]
    fn chaining_equals_one_shot() {
        let data = b"the log is the database";
        for split in 0..=data.len() {
            let (head, tail) = data.split_at(split);
            assert_eq!(
                update(update(0, head), tail),
                checksum(data),
                "split at {split}"
            );
        }
        assert_eq!(update(checksum(b"123"), b"456789"), 0xE306_9283);
    }

    /// A single flipped bit anywhere must change the checksum — the property the format
    /// actually relies on when it reports corruption.
    #[test]
    fn detects_single_bit_flips() {
        let mut rng = Pcg32::new(7, 7);
        let mut data = vec![0u8; 137];
        rng.fill_bytes(&mut data);
        let good = checksum(&data);

        for index in 0..data.len() {
            for bit in 0..8 {
                data[index] ^= 1 << bit;
                assert_ne!(
                    checksum(&data),
                    good,
                    "flip of byte {index} bit {bit} went unnoticed"
                );
                data[index] ^= 1 << bit;
            }
        }
    }

    #[test]
    fn table_zero_is_the_bitwise_definition() {
        // Independently recompute table 0 the slow way, one bit at a time.
        for (byte, &entry) in TABLES[0].iter().enumerate() {
            let mut expected = u32::try_from(byte).unwrap();
            for _ in 0..8 {
                expected = if expected & 1 == 1 {
                    (expected >> 1) ^ POLYNOMIAL
                } else {
                    expected >> 1
                };
            }
            assert_eq!(entry, expected);
        }
    }
}
