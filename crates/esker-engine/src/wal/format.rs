//! The physical log format: 32 KiB blocks of length-prefixed, checksummed fragments.
//!
//! ```text
//! block   = record* trailer?
//! record  = crc32c:u32 ++ len:u16 ++ type:u8 ++ payload[len]      (header is 7 bytes)
//! trailer = zero bytes, only when fewer than 7 bytes are left in the block
//! type    = FULL 1 | FIRST 2 | MIDDLE 3 | LAST 4                  (0 is reserved, never valid)
//! crc32c  = CRC32C over the type byte followed by the payload
//! ```
//!
//! All integers are little-endian. The layout is `LevelDB`'s, and two of its decisions are
//! load-bearing:
//!
//! * **A record never straddles a block.** A reader that starts at any block boundary can
//!   resynchronise, and a corrupt block costs the records inside it and nothing else.
//! * **The CRC is seeded with the type byte, not just taken over the payload.** Without that,
//!   a valid header and payload copied from one offset to another would verify and replay
//!   cleanly, turning a misdirected write into silent data loss. Because the type is part of
//!   the checksum, a fragment that lands in the wrong place fails its own CRC.
//!
//! `LevelDB` additionally *masks* the stored CRC by rotating it, because its CRC covers bytes
//! that can themselves contain checksums. Ours covers the type and the payload only, never the
//! checksum field, so the mask would protect nothing and is not applied. That is the one
//! deliberate departure from `LevelDB`'s bytes, and the golden file pins it.

use esker_base::crc32c;

use crate::format::{WAL_BLOCK_SIZE, WAL_HEADER_SIZE};

/// One log block. Every record lies entirely inside one.
pub const BLOCK_SIZE: usize = WAL_BLOCK_SIZE;

/// `crc32c:u32 ++ len:u16 ++ type:u8`.
pub const HEADER_SIZE: usize = WAL_HEADER_SIZE;

/// The most payload one fragment can carry: a whole block minus its header.
pub const MAX_FRAGMENT_LEN: usize = BLOCK_SIZE - HEADER_SIZE;

/// Which part of a record a fragment is.
///
/// The discriminants are on disk. `0` is deliberately not a type: an all-zero header — the
/// shape a partially written or zeroed block takes — is then invalid rather than an empty
/// record that replays as if it were real.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordType {
    /// A whole record.
    Full = 1,
    /// The first fragment of a record continued in later blocks.
    First = 2,
    /// A fragment with more on both sides.
    Middle = 3,
    /// The last fragment of a record.
    Last = 4,
}

impl RecordType {
    /// The on-disk byte.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decodes a type byte, or `None` for `0` and anything above `4`.
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Full),
            2 => Some(Self::First),
            3 => Some(Self::Middle),
            4 => Some(Self::Last),
            _ => None,
        }
    }
}

/// The checksum stored in a fragment's header: CRC32C over the type byte, then the payload.
pub fn fragment_crc(kind: RecordType, payload: &[u8]) -> u32 {
    crc32c::update(crc32c::update(0, &[kind.as_u8()]), payload)
}

/// Builds a fragment header. `payload` must fit in [`MAX_FRAGMENT_LEN`], which the writer
/// guarantees by construction — the length field is 16 bits and a block is 15.
pub fn encode_header(kind: RecordType, payload: &[u8]) -> [u8; HEADER_SIZE] {
    debug_assert!(payload.len() <= MAX_FRAGMENT_LEN);
    let mut header = [0u8; HEADER_SIZE];
    header[0..4].copy_from_slice(&fragment_crc(kind, payload).to_le_bytes());
    // Truncation is impossible: MAX_FRAGMENT_LEN is 32761, well inside u16.
    let len = u16::try_from(payload.len()).unwrap_or(u16::MAX);
    header[4..6].copy_from_slice(&len.to_le_bytes());
    header[6] = kind.as_u8();
    header
}

/// Splits a header into its three fields. The type byte is returned raw, because deciding
/// what an unknown one means belongs to the reader.
pub fn decode_header(header: &[u8; HEADER_SIZE]) -> (u32, usize, u8) {
    let crc = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let len = usize::from(u16::from_le_bytes([header[4], header[5]]));
    (crc, len, header[6])
}

#[cfg(test)]
mod tests {
    use super::{
        BLOCK_SIZE, HEADER_SIZE, MAX_FRAGMENT_LEN, RecordType, decode_header, encode_header,
        fragment_crc,
    };

    #[test]
    fn header_round_trips() {
        let header = encode_header(RecordType::Middle, b"payload");
        let (crc, len, kind) = decode_header(&header);
        assert_eq!(len, 7);
        assert_eq!(RecordType::from_u8(kind), Some(RecordType::Middle));
        assert_eq!(crc, fragment_crc(RecordType::Middle, b"payload"));
    }

    /// The whole point of the type seed: identical payloads under different types must not
    /// share a checksum, or a fragment moved between positions would verify.
    #[test]
    fn the_crc_depends_on_the_record_type() {
        let full = fragment_crc(RecordType::Full, b"same bytes");
        let first = fragment_crc(RecordType::First, b"same bytes");
        let middle = fragment_crc(RecordType::Middle, b"same bytes");
        let last = fragment_crc(RecordType::Last, b"same bytes");
        assert_ne!(full, first);
        assert_ne!(first, middle);
        assert_ne!(middle, last);
        assert_ne!(full, last);
        // And it is not simply the payload's CRC.
        assert_ne!(full, esker_base::crc32c::checksum(b"same bytes"));
    }

    #[test]
    fn type_zero_is_never_a_record() {
        assert_eq!(RecordType::from_u8(0), None);
        for byte in 5..=u8::MAX {
            assert_eq!(RecordType::from_u8(byte), None, "byte {byte}");
        }
        // An all-zero header is therefore not a valid empty record.
        let (_, len, kind) = decode_header(&[0u8; HEADER_SIZE]);
        assert_eq!(len, 0);
        assert_eq!(RecordType::from_u8(kind), None);
    }

    /// The header is exactly the bytes `docs/DESIGN.md` §4.3 promises, little-endian.
    #[test]
    fn header_byte_layout_is_frozen() {
        let header = encode_header(RecordType::Full, b"");
        let crc = fragment_crc(RecordType::Full, b"");
        assert_eq!(&header[0..4], &crc.to_le_bytes());
        assert_eq!(&header[4..6], &[0, 0]);
        assert_eq!(header[6], 1);
        assert_eq!(HEADER_SIZE, 7);
        assert_eq!(BLOCK_SIZE, 32 * 1024);
        assert_eq!(MAX_FRAGMENT_LEN, 32 * 1024 - 7);
        // The length field must be able to describe any fragment the writer can produce.
        assert!(u16::try_from(MAX_FRAGMENT_LEN).is_ok());
    }
}
