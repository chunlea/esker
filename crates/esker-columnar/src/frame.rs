//! How one column chunk is framed on disk: compression, a codec byte and a checksum.
//!
//! ```text
//! chunk := payload ++ codec:u8 ++ crc32c:u32 (LE)
//! ```
//!
//! **The checksum covers the chunk's offset, its payload and its codec byte.** Two traps, and the
//! second was found by the fuzz rather than by reading:
//!
//! * Checksumming the payload alone would let a flipped codec byte turn an LZ4 chunk into a
//!   "verified" uncompressed one full of garbage — the trap `esker_engine::sst` documents.
//! * Checksumming the chunk alone proves it is **intact**, not that it is **the chunk that was
//!   asked for**. A whole chunk copied over another — a misdirected write, a partial restore —
//!   carries its own valid checksum, decodes cleanly, and answers a query with another stripe's
//!   rows. That is a wrong answer with no error anywhere, and the fuzz produced one on its first
//!   real run. Folding the offset into the checksum makes a chunk that has moved fail, which is
//!   the difference between "these bytes are fine" and "these bytes belong here".
//!
//! The offset is not stored; both sides know where the chunk is and compute the same seed from
//! it. See [ADR 0027](../../../../docs/adr/0027-columnar-file-format.md).
//!
//! An LZ4 payload is `raw_len:varint ++ lz4 block bytes`, so the decompressed size is known before
//! a byte is decompressed and the buffer is allocated exactly once. A `raw_len` that the
//! compressed bytes could not possibly produce is refused rather than allocated (invariant 2, and
//! the reason the decoder fuzz cannot exhaust memory).
//!
//! Compression is skipped when it saves less than an eighth. That is `LevelDB`'s rule and the
//! engine's, for the same reason: the CPU spent decompressing a barely smaller chunk is not
//! repaid. It also means an already-dense encoding — a bit-packed column of ascending integers —
//! is stored as it is, which is what the per-type encodings exist to make common.

use esker_base::{crc32c, varint};

use crate::error::{Error, Result};
use crate::format::CHUNK_TRAILER_SIZE;

/// Largest payload this build will decompress into memory. A stored `raw_len` above it is
/// corruption, not an allocation request.
const MAX_UNCOMPRESSED_CHUNK: usize = 256 * 1024 * 1024;

/// LZ4's block format cannot expand by more than about 255:1, so a `raw_len` far above this bound
/// for the compressed bytes at hand is impossible however plausible the varint looks.
fn max_plausible_raw_len(compressed_len: usize) -> usize {
    compressed_len.saturating_mul(256).saturating_add(1024)
}

/// The codecs a chunk may be stored under. Two, forever, per the dependency policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// Stored as encoded.
    #[default]
    None,
    /// LZ4 block format, pure Rust (`lz4_flex`).
    Lz4,
}

impl Compression {
    /// The byte written into the chunk trailer.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Compression::None => 0,
            Compression::Lz4 => 1,
        }
    }

    /// The codec a trailer byte names, or `None` for one no version has written.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Compression::None),
            1 => Some(Compression::Lz4),
            _ => None,
        }
    }
}

/// Frames one encoded column chunk: compresses it if that pays, then appends the trailer.
///
/// `offset` is where the chunk will live in the file. It is folded into the checksum and not
/// written down; a chunk decoded from anywhere else fails.
#[must_use]
pub fn encode_chunk(payload: &[u8], compression: Compression, offset: u64) -> Vec<u8> {
    let mut stored = None;
    if compression == Compression::Lz4 && !payload.is_empty() {
        let mut candidate = Vec::with_capacity(payload.len());
        varint::put_u64(payload.len() as u64, &mut candidate);
        candidate.extend_from_slice(&lz4_flex::block::compress(payload));
        if candidate.len() < payload.len() - payload.len() / 8 {
            stored = Some(candidate);
        }
    }

    let (kind, body): (Compression, &[u8]) = match &stored {
        Some(bytes) => (Compression::Lz4, bytes),
        None => (Compression::None, payload),
    };

    let mut out = Vec::with_capacity(body.len() + CHUNK_TRAILER_SIZE);
    out.extend_from_slice(body);
    out.push(kind.as_u8());
    let checksum = checksum_at(offset, &out);
    out.extend_from_slice(&checksum.to_le_bytes());
    out
}

/// The checksum of a chunk that lives at `offset`: the position first, then the bytes.
fn checksum_at(offset: u64, checked: &[u8]) -> u32 {
    crc32c::update(crc32c::checksum(&offset.to_le_bytes()), checked)
}

/// Verifies and decompresses one framed chunk read from disk.
///
/// `raw` is the stored payload plus its 5-byte trailer, `offset` is where it was read from, and
/// `context` names the file or region for the error message. Every failure is an
/// [`Error::Corruption`] value: a bad checksum, a chunk that belongs somewhere else, an unknown
/// codec, a truncated LZ4 stream, or a stored length too large to be honest.
pub fn decode_chunk(raw: &[u8], offset: u64, context: &str) -> Result<Vec<u8>> {
    if raw.len() < CHUNK_TRAILER_SIZE {
        return Err(Error::corruption(
            context,
            format!("a chunk of {} bytes has no trailer", raw.len()),
        ));
    }
    let (checked, crc_bytes) = raw.split_at(raw.len() - 4);
    let mut stored_crc = [0u8; 4];
    stored_crc.copy_from_slice(crc_bytes);
    let stored_crc = u32::from_le_bytes(stored_crc);
    let actual = checksum_at(offset, checked);
    if actual != stored_crc {
        return Err(Error::corruption(
            context,
            format!(
                "chunk checksum {actual:#010x} does not match the stored {stored_crc:#010x}: \
                 damaged, or a chunk that belongs somewhere other than offset {offset}"
            ),
        ));
    }

    // `checked` is the payload followed by the codec byte, both covered by the CRC.
    let (body, kind_byte) = checked.split_at(checked.len() - 1);
    let kind = Compression::from_u8(kind_byte[0]).ok_or_else(|| {
        Error::corruption(context, format!("unknown codec byte {}", kind_byte[0]))
    })?;

    match kind {
        Compression::None => Ok(body.to_vec()),
        Compression::Lz4 => {
            let (raw_len, consumed) = varint::get_u64(body)
                .map_err(|e| Error::corruption(context, format!("lz4 uncompressed length: {e}")))?;
            let compressed = &body[consumed..];
            let raw_len = usize::try_from(raw_len).unwrap_or(usize::MAX);
            if raw_len > MAX_UNCOMPRESSED_CHUNK || raw_len > max_plausible_raw_len(compressed.len())
            {
                return Err(Error::corruption(
                    context,
                    format!(
                        "lz4 chunk claims {raw_len} uncompressed bytes from {} compressed",
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
    use esker_base::rng::Pcg32;
    use esker_base::varint;

    use super::{
        Compression, MAX_UNCOMPRESSED_CHUNK, decode_chunk, encode_chunk, max_plausible_raw_len,
    };
    use crate::format::CHUNK_TRAILER_SIZE;

    #[test]
    fn chunks_round_trip_under_both_codecs() {
        let mut rng = Pcg32::from_seed(0xC0_1B);
        let mut incompressible = vec![0u8; 4096];
        rng.fill_bytes(&mut incompressible);

        let payloads: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0u8; 1],
            vec![b'a'; 4096],
            incompressible,
            (0..8192u32).map(|i| (i % 7) as u8).collect(),
        ];
        for payload in payloads {
            for compression in [Compression::None, Compression::Lz4] {
                let framed = encode_chunk(&payload, compression, 0);
                assert!(framed.len() >= CHUNK_TRAILER_SIZE);
                assert_eq!(decode_chunk(&framed, 0, "test").unwrap(), payload);
            }
        }
    }

    #[test]
    fn compression_is_used_only_when_it_pays() {
        let mut rng = Pcg32::from_seed(0xC0_1C);
        let mut noise = vec![0u8; 4096];
        rng.fill_bytes(&mut noise);
        let framed = encode_chunk(&noise, Compression::Lz4, 0);
        assert_eq!(
            framed[framed.len() - CHUNK_TRAILER_SIZE],
            Compression::None.as_u8(),
            "an incompressible chunk was stored as lz4"
        );

        let runs = vec![b'z'; 4096];
        let framed = encode_chunk(&runs, Compression::Lz4, 0);
        assert_eq!(
            framed[framed.len() - CHUNK_TRAILER_SIZE],
            Compression::Lz4.as_u8()
        );
        assert!(framed.len() < runs.len());
    }

    /// The trap the checksum layout exists for: rewriting the codec byte must be caught.
    #[test]
    fn flipping_the_codec_byte_is_detected() {
        let payload = vec![b'q'; 2048];
        let mut framed = encode_chunk(&payload, Compression::Lz4, 0);
        let kind_at = framed.len() - CHUNK_TRAILER_SIZE;
        assert_eq!(framed[kind_at], Compression::Lz4.as_u8());

        framed[kind_at] = Compression::None.as_u8();
        let error = decode_chunk(&framed, 0, "test").unwrap_err();
        assert!(error.is_corruption(), "{error}");
        assert!(error.to_string().contains("checksum"), "{error}");
    }

    #[test]
    fn corrupt_trailers_are_errors_not_panics() {
        let payload = vec![b'a'; 512];
        let framed = encode_chunk(&payload, Compression::None, 0);

        assert!(decode_chunk(&[], 0, "test").is_err(), "no trailer");
        assert!(decode_chunk(&[0u8; 4], 0, "test").is_err(), "short trailer");

        let mut forged = framed.clone();
        let kind_at = forged.len() - CHUNK_TRAILER_SIZE;
        forged[kind_at] = 9;
        let checksum = super::checksum_at(0, &forged[..forged.len() - 4]);
        forged[kind_at + 1..].copy_from_slice(&checksum.to_le_bytes());
        let error = decode_chunk(&forged, 0, "test").unwrap_err();
        assert!(error.to_string().contains("unknown codec"), "{error}");

        let mut flipped = framed;
        flipped[10] ^= 0x01;
        assert!(
            decode_chunk(&flipped, 0, "test")
                .unwrap_err()
                .is_corruption()
        );
    }

    /// A dishonest uncompressed length must be refused before anything is allocated.
    #[test]
    fn an_impossible_uncompressed_length_is_refused() {
        assert!(max_plausible_raw_len(0) < MAX_UNCOMPRESSED_CHUNK);
        assert_eq!(max_plausible_raw_len(usize::MAX), usize::MAX);

        let payload = vec![b'k'; 4096];
        let framed = encode_chunk(&payload, Compression::Lz4, 0);
        let body_end = framed.len() - CHUNK_TRAILER_SIZE;
        let (_, consumed) = varint::get_u64(&framed[..body_end]).unwrap();

        let mut body = Vec::new();
        varint::put_u64(1 << 30, &mut body);
        body.extend_from_slice(&framed[consumed..body_end]);
        body.push(Compression::Lz4.as_u8());
        let checksum = super::checksum_at(0, &body);
        body.extend_from_slice(&checksum.to_le_bytes());

        let error = decode_chunk(&body, 0, "test").unwrap_err();
        assert!(error.is_corruption(), "{error}");
        assert!(error.to_string().contains("uncompressed bytes"), "{error}");
    }

    /// A chunk proves it belongs where it was found, not merely that it is intact.
    #[test]
    fn a_chunk_does_not_verify_at_another_offset() {
        let payload = vec![b'p'; 512];
        let framed = encode_chunk(&payload, Compression::Lz4, 4096);
        assert_eq!(decode_chunk(&framed, 4096, "test").unwrap(), payload);

        for offset in [0u64, 4095, 4097, u64::MAX] {
            let error = decode_chunk(&framed, offset, "test").unwrap_err();
            assert!(error.is_corruption(), "{error}");
            assert!(
                error.to_string().contains("belongs somewhere other than"),
                "{error}"
            );
        }
    }

    #[test]
    fn codec_bytes_are_frozen() {
        assert_eq!(Compression::None.as_u8(), 0);
        assert_eq!(Compression::Lz4.as_u8(), 1);
        assert_eq!(Compression::from_u8(0), Some(Compression::None));
        assert_eq!(Compression::from_u8(1), Some(Compression::Lz4));
        assert_eq!(Compression::from_u8(2), None);
    }
}
