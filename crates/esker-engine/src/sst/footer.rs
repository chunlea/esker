//! The physical framing of a table: block handles, the per-block trailer, and the footer.
//!
//! # The footer — 48 bytes, exactly, forever
//!
//! A reader finds the footer by seeking to `file_size - 48`, so its size can never change.
//! `docs/DESIGN.md` §4.5 and [`crate::format::SST_FOOTER_SIZE`] fix it at 48, and this is how
//! those bytes are spent:
//!
//! ```text
//! byte  0 ..35   six LEB128 varints, then zero padding:
//!                  index.offset, index.size,
//!                  filter.offset, filter.size,
//!                  properties.offset, properties.size
//! byte 35 ..39   format_version: u32 LE
//! byte 39 ..48   magic: the 9 ASCII bytes "ESKERSST1"
//! ```
//!
//! Six varints have to fit in 35 bytes, which is why [`MAX_TABLE_SIZE`] exists: capping a
//! table at 2^35 bytes caps every offset and size at five varint bytes, so the worst case is
//! 30 bytes and the remaining 5 are zero padding. The padding is *after* the handles, and a
//! zero byte is itself a valid varint, so a decoder simply reads six varints from the front
//! and ignores the rest. 32 GiB is far beyond anything this engine writes — a region splits
//! at 96 MiB (§14) — so the cap costs nothing and buys a fixed-size footer.
//!
//! An absent block (no filter, because `bloom_bits_per_key` is 0) is the handle
//! `{offset: 0, size: 0}`. That is unambiguous: offset 0 is where the first data block starts,
//! and the first data block is never the filter, the index or the properties.
//!
//! # The block trailer
//!
//! Every block — data, filter, properties, index — is followed by
//! `compression_type:u8 ++ crc32c:u32` (LE), and **the checksum covers the payload and the
//! type byte together**. Checksumming the payload alone would let a flipped type byte turn an
//! LZ4 block into a "verified" uncompressed one full of garbage. A block handle's `size` is
//! the stored payload only; the 5-byte trailer is not counted, and neither is it in the next
//! block's offset arithmetic — the reader adds it back.
//!
//! # Compression
//!
//! `None` or `Lz4` ([`Compression`]), never both in one file and never anything else
//! (`docs/adr/0003-dependencies.md`). An LZ4 payload is `raw_len:varint ++ lz4 block bytes`,
//! so the exact output size is known before a byte is decompressed and the buffer is
//! allocated once. A corrupt `raw_len` is rejected rather than allocated: see
//! [`decode_block`].

use esker_base::{crc32c, varint};

use crate::error::{Error, Result};
use crate::format::{BLOCK_TRAILER_SIZE, SST_FOOTER_SIZE, SST_FORMAT_VERSION, SST_MAGIC};
use crate::options::Compression;

/// Largest table this format addresses: 2^35 bytes, 32 GiB.
///
/// The bound that makes the 48-byte footer work — see the module docs. The builder refuses to
/// grow past it rather than writing a footer it could not encode.
pub const MAX_TABLE_SIZE: u64 = 1 << 35;

/// Bytes of the footer available to the three block handles.
const HANDLE_REGION: usize = SST_FOOTER_SIZE - 4 - SST_MAGIC.len();

/// Largest block this build will decompress into memory. A stored `raw_len` above it is
/// corruption, not an allocation request.
const MAX_UNCOMPRESSED_BLOCK: usize = 256 * 1024 * 1024;

/// LZ4's block format cannot expand by more than about 255:1, so a `raw_len` far above this
/// bound for the compressed bytes at hand is impossible however plausible the varint looks.
/// The slack covers the smallest possible literal-only frames.
fn max_plausible_raw_len(compressed_len: usize) -> usize {
    compressed_len.saturating_mul(256).saturating_add(1024)
}

/// Where a block lives: its offset in the file, and its stored payload size excluding the
/// 5-byte trailer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockHandle {
    /// Offset of the block's first payload byte.
    pub offset: u64,
    /// Stored payload bytes, trailer excluded.
    pub size: u64,
}

impl BlockHandle {
    /// The handle of a block that is not present.
    pub const NONE: Self = Self { offset: 0, size: 0 };

    /// A handle for `size` bytes at `offset`.
    #[must_use]
    pub fn new(offset: u64, size: u64) -> Self {
        Self { offset, size }
    }

    /// Whether this names a block at all. See the module docs for why `{0, 0}` is safe to
    /// reserve.
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.offset == 0 && self.size == 0
    }

    /// Bytes the block occupies including its trailer.
    #[must_use]
    pub fn total_len(&self) -> u64 {
        self.size + BLOCK_TRAILER_SIZE as u64
    }

    /// Appends `offset` then `size` as LEB128 varints. This is also the encoding of an index
    /// block's values.
    pub fn encode_to(&self, out: &mut Vec<u8>) {
        varint::put_u64(self.offset, out);
        varint::put_u64(self.size, out);
    }

    /// Encodes to a fresh buffer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(2 * varint::MAX_LEN_U64);
        self.encode_to(&mut out);
        out
    }

    /// Decodes a handle from the front of `buf`, returning it and the bytes consumed.
    pub fn decode_from(buf: &[u8]) -> Result<(Self, usize)> {
        let (offset, n1) = varint::get_u64(buf)
            .map_err(|e| Error::corruption("sst block handle", format!("offset: {e}")))?;
        let (size, n2) = varint::get_u64(&buf[n1..])
            .map_err(|e| Error::corruption("sst block handle", format!("size: {e}")))?;
        Ok((Self { offset, size }, n1 + n2))
    }
}

/// The three handles and the format version that let a reader open a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    /// The index block: one entry per data block.
    pub index: BlockHandle,
    /// The bloom filter block, or [`BlockHandle::NONE`] when the table has no filter.
    pub filter: BlockHandle,
    /// The properties block.
    pub properties: BlockHandle,
    /// Layout version of this table.
    pub format_version: u32,
}

impl Footer {
    /// A footer for the current [`SST_FORMAT_VERSION`].
    #[must_use]
    pub fn new(index: BlockHandle, filter: BlockHandle, properties: BlockHandle) -> Self {
        Self {
            index,
            filter,
            properties,
            format_version: SST_FORMAT_VERSION,
        }
    }

    /// Lays the footer out as the module documents.
    ///
    /// Fails only if the handles do not fit the padded region, which [`MAX_TABLE_SIZE`] makes
    /// unreachable — the check is here so that a future format change cannot silently
    /// overflow the fixed size.
    pub fn encode(&self) -> Result<[u8; SST_FOOTER_SIZE]> {
        let mut handles = Vec::with_capacity(HANDLE_REGION);
        self.index.encode_to(&mut handles);
        self.filter.encode_to(&mut handles);
        self.properties.encode_to(&mut handles);
        if handles.len() > HANDLE_REGION {
            return Err(Error::InvalidArgument(format!(
                "sst footer handles need {} bytes but only {HANDLE_REGION} are reserved",
                handles.len()
            )));
        }

        let mut out = [0u8; SST_FOOTER_SIZE];
        out[..handles.len()].copy_from_slice(&handles);
        out[HANDLE_REGION..HANDLE_REGION + 4].copy_from_slice(&self.format_version.to_le_bytes());
        out[HANDLE_REGION + 4..].copy_from_slice(&SST_MAGIC);
        Ok(out)
    }

    /// Parses the last 48 bytes of a table.
    ///
    /// The magic is checked first: a file that is not a table, or is truncated, must say so
    /// rather than produce nonsense handles.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != SST_FOOTER_SIZE {
            return Err(Error::corruption(
                "sst footer",
                format!("footer is {} bytes, not {SST_FOOTER_SIZE}", bytes.len()),
            ));
        }
        if bytes[HANDLE_REGION + 4..] != SST_MAGIC {
            return Err(Error::corruption(
                "sst footer",
                "bad magic: not an esker sorted string table, or the tail is truncated",
            ));
        }
        let mut version = [0u8; 4];
        version.copy_from_slice(&bytes[HANDLE_REGION..HANDLE_REGION + 4]);
        let format_version = u32::from_le_bytes(version);
        if format_version != SST_FORMAT_VERSION {
            return Err(Error::corruption(
                "sst footer",
                format!("format version {format_version}, this build writes {SST_FORMAT_VERSION}"),
            ));
        }

        let region = &bytes[..HANDLE_REGION];
        let (index, n1) = BlockHandle::decode_from(region)?;
        let (filter, n2) = BlockHandle::decode_from(&region[n1..])?;
        let (properties, _) = BlockHandle::decode_from(&region[n1 + n2..])?;
        Ok(Self {
            index,
            filter,
            properties,
            format_version,
        })
    }
}

/// Frames one block: compresses it if that helps, then appends the trailer.
///
/// Compression is skipped when it saves less than an eighth, because the CPU spent
/// decompressing a barely-smaller block is not repaid — `LevelDB`'s rule.
#[must_use]
pub fn encode_block(payload: &[u8], compression: Compression) -> Vec<u8> {
    let mut stored = None;
    if compression == Compression::Lz4 {
        let mut candidate = Vec::with_capacity(payload.len());
        varint::put_u64(
            u64::try_from(payload.len()).unwrap_or(u64::MAX),
            &mut candidate,
        );
        candidate.extend_from_slice(&lz4_flex::block::compress(payload));
        if candidate.len() < payload.len() - payload.len() / 8 {
            stored = Some(candidate);
        }
    }

    let (kind, body): (Compression, &[u8]) = match &stored {
        Some(bytes) => (Compression::Lz4, bytes),
        None => (Compression::None, payload),
    };

    let mut out = Vec::with_capacity(body.len() + BLOCK_TRAILER_SIZE);
    out.extend_from_slice(body);
    out.push(kind.as_u8());
    // The checksum covers the payload *and* the type byte, so the codec cannot be rewritten.
    let checksum = crc32c::checksum(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

/// Verifies and decompresses one block read from disk.
///
/// `raw` is the stored payload plus its 5-byte trailer. `context` names the file for the
/// error message. Every failure is an [`Error::Corruption`] value: a bad checksum, an unknown
/// codec, a truncated LZ4 stream, or a stored length too large to be honest (invariant 2).
pub fn decode_block(raw: &[u8], context: &str) -> Result<Vec<u8>> {
    if raw.len() < BLOCK_TRAILER_SIZE {
        return Err(Error::corruption(
            context,
            format!("block of {} bytes has no trailer", raw.len()),
        ));
    }
    let (checked, crc_bytes) = raw.split_at(raw.len() - 4);
    let mut stored_crc = [0u8; 4];
    stored_crc.copy_from_slice(crc_bytes);
    let stored_crc = u32::from_le_bytes(stored_crc);
    let actual = crc32c::checksum(checked);
    if actual != stored_crc {
        return Err(Error::corruption(
            context,
            format!("block checksum {actual:#010x} does not match the stored {stored_crc:#010x}"),
        ));
    }

    // `checked` is the payload followed by the compression byte, both covered by the CRC.
    let (body, kind_byte) = checked.split_at(checked.len() - 1);
    let kind = Compression::from_u8(kind_byte[0]).ok_or_else(|| {
        Error::corruption(
            context,
            format!("unknown compression code {}", kind_byte[0]),
        )
    })?;

    match kind {
        Compression::None => Ok(body.to_vec()),
        Compression::Lz4 => {
            let (raw_len, consumed) = varint::get_u64(body)
                .map_err(|e| Error::corruption(context, format!("lz4 uncompressed length: {e}")))?;
            let compressed = &body[consumed..];
            let raw_len = usize::try_from(raw_len).unwrap_or(usize::MAX);
            if raw_len > MAX_UNCOMPRESSED_BLOCK || raw_len > max_plausible_raw_len(compressed.len())
            {
                return Err(Error::corruption(
                    context,
                    format!(
                        "lz4 block claims {raw_len} uncompressed bytes from {} compressed",
                        compressed.len()
                    ),
                ));
            }
            lz4_flex::block::decompress(compressed, raw_len)
                .map_err(|e| Error::corruption(context, format!("lz4 decode failed: {e}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BlockHandle, Footer, HANDLE_REGION, MAX_TABLE_SIZE, MAX_UNCOMPRESSED_BLOCK, decode_block,
        encode_block, max_plausible_raw_len,
    };
    use crate::format::{BLOCK_TRAILER_SIZE, SST_FOOTER_SIZE, SST_FORMAT_VERSION, SST_MAGIC};
    use crate::options::Compression;
    use esker_base::{rng::Pcg32, varint};

    /// The arithmetic the whole fixed-size footer rests on: six varints of a capped table
    /// always fit the padded region.
    #[test]
    fn the_handle_region_can_always_hold_three_handles() {
        assert_eq!(HANDLE_REGION, 35);
        assert_eq!(HANDLE_REGION + 4 + SST_MAGIC.len(), SST_FOOTER_SIZE);

        let widest = MAX_TABLE_SIZE - 1;
        assert_eq!(varint::encoded_len_u64(widest), 5);
        assert!(6 * varint::encoded_len_u64(widest) <= HANDLE_REGION);

        let footer = Footer::new(
            BlockHandle::new(widest, widest),
            BlockHandle::new(widest, widest),
            BlockHandle::new(widest, widest),
        );
        let bytes = footer.encode().expect("a capped table's footer fits");
        assert_eq!(bytes.len(), SST_FOOTER_SIZE);
        assert_eq!(Footer::decode(&bytes).unwrap(), footer);
    }

    /// A handle wider than the cap is refused rather than silently overflowing the footer.
    #[test]
    fn an_uncapped_handle_is_rejected() {
        let footer = Footer::new(
            BlockHandle::new(u64::MAX, u64::MAX),
            BlockHandle::new(u64::MAX, u64::MAX),
            BlockHandle::new(u64::MAX, u64::MAX),
        );
        assert!(footer.encode().is_err());
    }

    /// The footer is on disk, so its bytes are frozen. Written out by hand here.
    #[test]
    fn golden_footer_bytes() {
        let footer = Footer::new(
            BlockHandle::new(0x1234, 0x56),
            BlockHandle::NONE,
            BlockHandle::new(0x99, 0x07),
        );
        let bytes = footer.encode().unwrap();

        #[rustfmt::skip]
        let expected: [u8; SST_FOOTER_SIZE] = [
            // index.offset 0x1234 = 4660 -> 0xb4 0x24; index.size 0x56
            0xb4, 0x24, 0x56,
            // filter: the absent handle, two zero varints
            0x00, 0x00,
            // properties.offset 0x99 = 153 -> 0x99 0x01; properties.size 0x07
            0x99, 0x01, 0x07,
            // zero padding out to byte 35
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            // format_version: u32 LE
            0x01, 0x00, 0x00, 0x00,
            // magic
            b'E', b'S', b'K', b'E', b'R', b'S', b'S', b'T', b'1',
        ];
        assert_eq!(
            bytes, expected,
            "the footer layout changed; it is fixed forever (ADR + version bump)"
        );

        let decoded = Footer::decode(&bytes).unwrap();
        assert_eq!(decoded, footer);
        assert!(decoded.filter.is_none());
        assert!(!decoded.index.is_none());
        assert_eq!(decoded.format_version, SST_FORMAT_VERSION);
    }

    /// A file that is not a table, or whose tail was lost, must be named as such.
    #[test]
    fn footers_that_are_not_footers() {
        assert!(Footer::decode(&[]).is_err(), "empty");
        assert!(Footer::decode(&[0u8; 47]).is_err(), "one byte short");
        assert!(Footer::decode(&[0u8; 48]).is_err(), "no magic");

        let mut bytes = Footer::new(BlockHandle::NONE, BlockHandle::NONE, BlockHandle::NONE)
            .encode()
            .unwrap();
        bytes[SST_FOOTER_SIZE - 1] = b'2';
        assert!(Footer::decode(&bytes).is_err(), "wrong magic");

        let mut bytes = Footer::new(BlockHandle::NONE, BlockHandle::NONE, BlockHandle::NONE)
            .encode()
            .unwrap();
        bytes[HANDLE_REGION] = 2;
        assert!(Footer::decode(&bytes).is_err(), "future format version");
    }

    #[test]
    fn handles_round_trip() {
        for handle in [
            BlockHandle::NONE,
            BlockHandle::new(1, 1),
            BlockHandle::new(MAX_TABLE_SIZE - 1, 4096),
        ] {
            let bytes = handle.encode();
            let (decoded, consumed) = BlockHandle::decode_from(&bytes).unwrap();
            assert_eq!(decoded, handle);
            assert_eq!(consumed, bytes.len());
            assert_eq!(handle.total_len(), handle.size + BLOCK_TRAILER_SIZE as u64);
        }
        assert!(BlockHandle::decode_from(&[]).is_err());
        assert!(
            BlockHandle::decode_from(&[0x80]).is_err(),
            "truncated varint"
        );
    }

    /// Both codecs round-trip byte for byte, including the payloads compression cannot help.
    #[test]
    fn blocks_round_trip_under_both_codecs() {
        let mut rng = Pcg32::from_seed(7);
        let mut incompressible = vec![0u8; 4096];
        rng.fill_bytes(&mut incompressible);

        let payloads: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0u8; 1],
            vec![b'a'; 4096],
            incompressible,
            (0..8192u32)
                .map(|i| u8::try_from(i % 7).unwrap_or(0))
                .collect(),
        ];
        for payload in payloads {
            for compression in [Compression::None, Compression::Lz4] {
                let framed = encode_block(&payload, compression);
                assert!(framed.len() >= BLOCK_TRAILER_SIZE);
                let decoded = decode_block(&framed, "test").unwrap();
                assert_eq!(decoded, payload, "{compression:?} did not round-trip");
            }
        }
    }

    /// Compression is only used when it pays, so a random block is stored as-is.
    #[test]
    fn incompressible_blocks_are_stored_uncompressed() {
        let mut rng = Pcg32::from_seed(11);
        let mut payload = vec![0u8; 4096];
        rng.fill_bytes(&mut payload);
        let framed = encode_block(&payload, Compression::Lz4);
        assert_eq!(
            framed[framed.len() - BLOCK_TRAILER_SIZE],
            Compression::None.as_u8(),
            "an incompressible block was stored as lz4"
        );

        let compressible = vec![b'z'; 4096];
        let framed = encode_block(&compressible, Compression::Lz4);
        assert_eq!(
            framed[framed.len() - BLOCK_TRAILER_SIZE],
            Compression::Lz4.as_u8()
        );
        assert!(framed.len() < compressible.len());
    }

    /// The trap this checksum layout exists for: rewriting the codec byte on disk must be
    /// caught, not decoded into garbage.
    #[test]
    fn flipping_the_compression_byte_is_detected() {
        let payload = vec![b'q'; 2048];
        let mut framed = encode_block(&payload, Compression::Lz4);
        let kind_at = framed.len() - BLOCK_TRAILER_SIZE;
        assert_eq!(framed[kind_at], Compression::Lz4.as_u8());

        framed[kind_at] = Compression::None.as_u8();
        let error = decode_block(&framed, "test").unwrap_err();
        assert!(error.is_corruption(), "{error}");
        assert!(error.to_string().contains("checksum"), "{error}");
    }

    /// Every other way a trailer can lie.
    #[test]
    fn corrupt_trailers_are_errors_not_panics() {
        let payload = vec![b'a'; 512];
        let framed = encode_block(&payload, Compression::None);

        assert!(decode_block(&[], "test").is_err(), "no trailer");
        assert!(decode_block(&[0u8; 4], "test").is_err(), "short trailer");

        // An unknown codec code, with the checksum recomputed so only the code is wrong.
        let mut forged = framed.clone();
        let kind_at = forged.len() - BLOCK_TRAILER_SIZE;
        forged[kind_at] = 7;
        let checksum = esker_base::crc32c::checksum(&forged[..forged.len() - 4]);
        forged[kind_at + 1..].copy_from_slice(&checksum.to_le_bytes());
        let error = decode_block(&forged, "test").unwrap_err();
        assert!(error.to_string().contains("unknown compression"), "{error}");

        // A single flipped payload byte.
        let mut flipped = framed.clone();
        flipped[10] ^= 0x01;
        assert!(decode_block(&flipped, "test").unwrap_err().is_corruption());
    }

    /// A corrupt LZ4 length must be refused before anything is allocated: an attacker-chosen
    /// varint would otherwise ask for gigabytes.
    #[test]
    fn an_impossible_uncompressed_length_is_refused() {
        assert!(max_plausible_raw_len(0) < MAX_UNCOMPRESSED_BLOCK);
        assert_eq!(max_plausible_raw_len(usize::MAX), usize::MAX);

        let payload = vec![b'k'; 4096];
        let framed = encode_block(&payload, Compression::Lz4);
        // Rebuild the block with a dishonest length: 1 GiB from a few hundred bytes.
        let body_end = framed.len() - BLOCK_TRAILER_SIZE;
        let (_, consumed) = varint::get_u64(&framed[..body_end]).unwrap();
        let mut body = Vec::new();
        varint::put_u64(1 << 30, &mut body);
        body.extend_from_slice(&framed[consumed..body_end]);
        body.push(Compression::Lz4.as_u8());
        let checksum = esker_base::crc32c::checksum(&body);
        body.extend_from_slice(&checksum.to_le_bytes());

        let error = decode_block(&body, "test").unwrap_err();
        assert!(error.is_corruption(), "{error}");
        assert!(error.to_string().contains("uncompressed bytes"), "{error}");
    }
}
