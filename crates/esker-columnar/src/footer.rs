//! Where a columnar file keeps what it is: the trailer, and the footer it points at.
//!
//! # The trailer — 32 bytes, exactly, forever
//!
//! A reader seeks to `file_size - 32`. Its size can never change, because finding it must never
//! be a search:
//!
//! ```text
//! byte  0.. 8   footer_offset:  u64 LE
//! byte  8..12   footer_len:     u32 LE
//! byte 12..16   footer_crc32c:  u32 LE   over the footer payload
//! byte 16..20   trailer_crc32c: u32 LE   over bytes 0..16
//! byte 20..24   format_version: u32 LE
//! byte 24..32   magic:          the 8 ASCII bytes "ESKERCOL"
//! ```
//!
//! **The trailer is the commit point** (invariant 3). A writer lays down stripes, then the
//! footer, then these 32 bytes, then syncs and renames the file into place. Every prefix of that
//! sequence is a file a crash left behind, and every one of them lacks the trailing magic — so
//! "was this file finished" is answered by eight bytes at a known offset and nothing else.
//!
//! The trailer checksums *itself* as well as the footer. Without that, a torn write that happened
//! to leave the magic intact would hand a reader a plausible `footer_offset` pointing into the
//! middle of a stripe; the footer's own CRC would eventually catch it, but only after a length
//! from nowhere had been believed for one allocation.
//!
//! # The footer
//!
//! Hand-written little-endian and LEB128 varints, never serde (ADR 0002), and not compressed: the
//! trailer holds its checksum, and a footer small enough to read in one go is worth more than a
//! footer small enough to store.
//!
//! ```text
//! footer       := schema ++ stripe_index
//! schema       := column_count:varint ++ (name_len:varint ++ name ++ type_tag:u8)*
//! stripe_index := stripe_count:varint ++ stripe*
//! stripe       := rows:varint ++ offset:varint ++ len:varint ++ chunk*      one chunk per column
//! chunk        := offset:varint ++ len:varint ++ encoding:u8 ++ stats
//! ```
//!
//! Offsets are absolute file positions, so reading one column of one stripe is one seek computed
//! entirely from the footer — which is the property the whole layout exists for.

use esker_base::{crc32c, varint};

use crate::cursor::Cursor;
use crate::encode::Encoding;
use crate::error::{Error, Result};
use crate::format::{COLUMNAR_FORMAT_VERSION, COLUMNAR_MAGIC, COLUMNAR_TRAILER_SIZE};
use crate::stats::ColumnStats;
use crate::value::{ColumnDef, ColumnType, MAX_COLUMN_NAME, MAX_COLUMNS, Schema};

/// Where the magic begins inside the trailer.
const MAGIC_AT: usize = COLUMNAR_TRAILER_SIZE - COLUMNAR_MAGIC.len();

/// Bytes of the trailer the trailer's own checksum covers.
const TRAILER_CHECKED: usize = 16;

/// Largest footer this build will read into memory.
///
/// A schema of 4096 columns with 1 KiB names and a stripe index of tens of thousands of stripes
/// is comfortably inside it; a corrupt `footer_len` is not.
pub const MAX_FOOTER_LEN: u32 = 64 * 1024 * 1024;

/// The fixed tail of a columnar file: where its footer is, and that it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trailer {
    /// Offset of the footer's first byte.
    pub footer_offset: u64,
    /// The footer's length in bytes.
    pub footer_len: u32,
    /// CRC32C of the footer's bytes.
    pub footer_crc: u32,
    /// Layout version of this file.
    pub format_version: u32,
}

impl Trailer {
    /// A trailer for the current [`COLUMNAR_FORMAT_VERSION`], describing a footer at `offset`.
    #[must_use]
    pub fn new(footer_offset: u64, footer: &[u8]) -> Self {
        Self {
            footer_offset,
            footer_len: u32::try_from(footer.len()).unwrap_or(u32::MAX),
            footer_crc: crc32c::checksum(footer),
            format_version: COLUMNAR_FORMAT_VERSION,
        }
    }

    /// Whether `tail` — the last bytes of a file — ends in the magic that says it was finished.
    ///
    /// This and only this distinguishes a file a crash interrupted from one that is complete.
    #[must_use]
    pub fn sealed(tail: &[u8]) -> bool {
        tail.len() >= COLUMNAR_TRAILER_SIZE
            && tail[tail.len() - COLUMNAR_MAGIC.len()..] == COLUMNAR_MAGIC
    }

    /// Lays the trailer out as the module documents.
    #[must_use]
    pub fn encode(&self) -> [u8; COLUMNAR_TRAILER_SIZE] {
        let mut out = [0u8; COLUMNAR_TRAILER_SIZE];
        out[0..8].copy_from_slice(&self.footer_offset.to_le_bytes());
        out[8..12].copy_from_slice(&self.footer_len.to_le_bytes());
        out[12..16].copy_from_slice(&self.footer_crc.to_le_bytes());
        let checksum = crc32c::checksum(&out[..TRAILER_CHECKED]);
        out[16..20].copy_from_slice(&checksum.to_le_bytes());
        out[20..24].copy_from_slice(&self.format_version.to_le_bytes());
        out[MAGIC_AT..].copy_from_slice(&COLUMNAR_MAGIC);
        out
    }

    /// Parses the last 32 bytes of a file that [`Trailer::sealed`] has already accepted.
    ///
    /// Every failure here is corruption rather than an unfinished file: the magic was present, so
    /// something wrote a complete trailer and its bytes have since stopped meaning what they did.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != COLUMNAR_TRAILER_SIZE {
            return Err(Error::corruption(
                "columnar trailer",
                format!(
                    "trailer is {} bytes, not {COLUMNAR_TRAILER_SIZE}",
                    bytes.len()
                ),
            ));
        }
        if bytes[MAGIC_AT..] != COLUMNAR_MAGIC {
            return Err(Error::corruption(
                "columnar trailer",
                "bad magic: not an esker columnar file",
            ));
        }

        let mut cursor = Cursor::new(bytes, "columnar trailer");
        let footer_offset = cursor.u64_le("footer offset")?;
        let footer_len = cursor.u32_le("footer length")?;
        let footer_crc = cursor.u32_le("footer checksum")?;
        let stored_crc = cursor.u32_le("trailer checksum")?;
        let actual = crc32c::checksum(&bytes[..TRAILER_CHECKED]);
        if actual != stored_crc {
            return Err(Error::corruption(
                "columnar trailer",
                format!(
                    "trailer checksum {actual:#010x} does not match the stored {stored_crc:#010x}"
                ),
            ));
        }

        let format_version = cursor.u32_le("format version")?;
        if format_version != COLUMNAR_FORMAT_VERSION {
            return Err(Error::corruption(
                "columnar trailer",
                format!(
                    "format version {format_version}, this build writes {COLUMNAR_FORMAT_VERSION}"
                ),
            ));
        }
        if footer_len > MAX_FOOTER_LEN {
            return Err(Error::corruption(
                "columnar trailer",
                format!("footer of {footer_len} bytes, over the {MAX_FOOTER_LEN} limit"),
            ));
        }

        Ok(Self {
            footer_offset,
            footer_len,
            footer_crc,
            format_version,
        })
    }
}

/// One column's chunk within one stripe, as the footer records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkMeta {
    /// Offset of the chunk's first framed byte.
    pub offset: u64,
    /// Framed length, trailer included — exactly what [`crate::frame::decode_chunk`] wants.
    pub len: u64,
    /// How the values inside are encoded.
    pub encoding: Encoding,
    /// What is known about the chunk without reading it.
    pub stats: ColumnStats,
}

/// One row group: a run of rows, stored as one chunk per column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripeMeta {
    /// How many rows the stripe holds.
    pub rows: u64,
    /// Offset of the stripe's first byte, which is its first chunk's.
    pub offset: u64,
    /// Bytes the whole stripe occupies.
    pub len: u64,
    /// One entry per column, in schema order.
    pub columns: Vec<ChunkMeta>,
}

/// Everything a reader needs before it touches a single value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    /// The columns, in the order every stripe stores its chunks.
    pub schema: Schema,
    /// The stripes, in the order they were written, which is row order.
    pub stripes: Vec<StripeMeta>,
}

impl Footer {
    /// Lays the footer out as the module documents.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + 32 * self.schema.len() * (self.stripes.len() + 1));
        varint::put_u64(self.schema.len() as u64, &mut out);
        for column in self.schema.columns() {
            varint::put_u64(column.name.len() as u64, &mut out);
            out.extend_from_slice(column.name.as_bytes());
            out.push(column.ty.tag());
        }

        varint::put_u64(self.stripes.len() as u64, &mut out);
        for stripe in &self.stripes {
            varint::put_u64(stripe.rows, &mut out);
            varint::put_u64(stripe.offset, &mut out);
            varint::put_u64(stripe.len, &mut out);
            for chunk in &stripe.columns {
                varint::put_u64(chunk.offset, &mut out);
                varint::put_u64(chunk.len, &mut out);
                out.push(chunk.encoding.as_u8());
                chunk.stats.encode_to(&mut out);
            }
        }
        out
    }

    /// Reads a footer back, given exactly the bytes the trailer named.
    ///
    /// Nothing here trusts a count: the schema's width, the stripe count and every bound length
    /// are checked against the bytes that remain before anything is sized.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes, "columnar footer");

        // Two bytes at least per column: a name length and a type tag.
        let column_count = cursor.count("column count", 2)?;
        if column_count > MAX_COLUMNS {
            return Err(Error::corruption(
                "columnar footer",
                format!("{column_count} columns, over the {MAX_COLUMNS} this format addresses"),
            ));
        }
        let mut columns = Vec::with_capacity(column_count);
        for index in 0..column_count {
            let name_len = cursor.count("column name length", 1)?;
            if name_len > MAX_COLUMN_NAME {
                return Err(Error::corruption(
                    "columnar footer",
                    format!("column {index}'s name is {name_len} bytes, over {MAX_COLUMN_NAME}"),
                ));
            }
            let name =
                std::str::from_utf8(cursor.bytes(name_len, "column name")?).map_err(|error| {
                    Error::corruption(
                        "columnar footer",
                        format!("column {index}'s name is not utf-8: {error}"),
                    )
                })?;
            let ty = ColumnType::from_tag(cursor.u8("column type tag")?)?;
            columns.push(ColumnDef::new(name, ty));
        }
        let schema = Schema::new(columns)
            .map_err(|error| Error::corruption("columnar footer", error.to_string()))?;

        // A stripe is three varints plus one chunk per column, and a chunk is at least five
        // bytes: two varint handles, an encoding tag, a null count and a flags byte.
        let least_per_stripe = 3 + 5 * column_count;
        let stripe_count = cursor.count("stripe count", least_per_stripe)?;
        let mut stripes = Vec::with_capacity(stripe_count);
        for _ in 0..stripe_count {
            let rows = cursor.varint("stripe rows")?;
            let offset = cursor.varint("stripe offset")?;
            let len = cursor.varint("stripe length")?;
            let mut chunks = Vec::with_capacity(column_count);
            for index in 0..column_count {
                let chunk_offset = cursor.varint("chunk offset")?;
                let chunk_len = cursor.varint("chunk length")?;
                let tag = cursor.u8("chunk encoding tag")?;
                let encoding = Encoding::from_u8(tag).ok_or_else(|| {
                    Error::corruption(
                        "columnar footer",
                        format!("column {index} uses encoding tag {tag}"),
                    )
                })?;
                chunks.push(ChunkMeta {
                    offset: chunk_offset,
                    len: chunk_len,
                    encoding,
                    stats: ColumnStats::decode_from(&mut cursor)?,
                });
            }
            stripes.push(StripeMeta {
                rows,
                offset,
                len,
                columns: chunks,
            });
        }

        cursor.finish()?;
        Ok(Self { schema, stripes })
    }

    /// Rows across every stripe.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.stripes.iter().map(|stripe| stripe.rows).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::{ChunkMeta, Footer, MAX_FOOTER_LEN, StripeMeta, Trailer};
    use crate::encode::Encoding;
    use crate::format::{COLUMNAR_FORMAT_VERSION, COLUMNAR_MAGIC, COLUMNAR_TRAILER_SIZE};
    use crate::stats::{Bound, ColumnStats};
    use crate::value::{ColumnDef, ColumnType, Schema};

    fn schema() -> Schema {
        Schema::new(vec![
            ColumnDef::new("id", ColumnType::Int8),
            ColumnDef::new("body", ColumnType::Text),
            ColumnDef::new("ok", ColumnType::Bool),
        ])
        .unwrap()
    }

    fn footer() -> Footer {
        Footer {
            schema: schema(),
            stripes: vec![
                StripeMeta {
                    rows: 1000,
                    offset: 0,
                    len: 512,
                    columns: vec![
                        ChunkMeta {
                            offset: 0,
                            len: 200,
                            encoding: Encoding::FrameOfReference,
                            stats: ColumnStats {
                                null_count: 0,
                                min: Some(Bound::exact(0i64.to_le_bytes().to_vec())),
                                max: Some(Bound::exact(999i64.to_le_bytes().to_vec())),
                            },
                        },
                        ChunkMeta {
                            offset: 200,
                            len: 300,
                            encoding: Encoding::Dictionary,
                            stats: ColumnStats {
                                null_count: 4,
                                min: Some(Bound::exact(b"alpha".to_vec())),
                                max: Some(Bound {
                                    bytes: b"zzz".to_vec(),
                                    truncated: true,
                                }),
                            },
                        },
                        ChunkMeta {
                            offset: 500,
                            len: 12,
                            encoding: Encoding::Rle,
                            stats: ColumnStats {
                                null_count: 1000,
                                min: None,
                                max: None,
                            },
                        },
                    ],
                },
                StripeMeta {
                    rows: 3,
                    offset: 512,
                    len: 40,
                    columns: vec![
                        ChunkMeta {
                            offset: 512,
                            len: 20,
                            encoding: Encoding::Delta,
                            stats: ColumnStats::empty(),
                        },
                        ChunkMeta {
                            offset: 532,
                            len: 12,
                            encoding: Encoding::Plain,
                            stats: ColumnStats::empty(),
                        },
                        ChunkMeta {
                            offset: 544,
                            len: 8,
                            encoding: Encoding::Bitpacked,
                            stats: ColumnStats::empty(),
                        },
                    ],
                },
            ],
        }
    }

    /// The trailer is on disk. Its bytes are written out by hand here so that a layout change
    /// fails a test rather than a database.
    #[test]
    fn golden_trailer_bytes() {
        let trailer = Trailer {
            footer_offset: 0x1234,
            footer_len: 0x56,
            footer_crc: 0x89AB_CDEF,
            format_version: COLUMNAR_FORMAT_VERSION,
        };
        let bytes = trailer.encode();

        #[rustfmt::skip]
        let expected: [u8; COLUMNAR_TRAILER_SIZE] = [
            // footer_offset: u64 LE = 0x1234
            0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // footer_len: u32 LE = 0x56
            0x56, 0x00, 0x00, 0x00,
            // footer_crc32c: u32 LE
            0xef, 0xcd, 0xab, 0x89,
            // trailer_crc32c: u32 LE over the sixteen bytes above
            0x7b, 0xdd, 0x82, 0x61,
            // format_version: u32 LE
            0x02, 0x00, 0x00, 0x00,
            // magic
            b'E', b'S', b'K', b'E', b'R', b'C', b'O', b'L',
        ];
        assert_eq!(
            bytes, expected,
            "the trailer layout changed; it is fixed forever (ADR + version bump)"
        );
        // The only byte that has ever moved here is the version itself, which is what a version
        // bump is supposed to look like: everything around it stayed where it was.
        assert_eq!(Trailer::decode(&bytes).unwrap(), trailer);
        assert!(Trailer::sealed(&bytes));
    }

    #[test]
    fn a_trailer_names_the_footer_it_was_built_from() {
        let payload = footer().encode();
        let trailer = Trailer::new(4096, &payload);
        assert_eq!(trailer.footer_offset, 4096);
        assert_eq!(trailer.footer_len as usize, payload.len());
        assert_eq!(trailer.footer_crc, esker_base::crc32c::checksum(&payload));
        assert_eq!(Trailer::decode(&trailer.encode()).unwrap(), trailer);
    }

    #[test]
    fn trailers_that_are_not_trailers() {
        assert!(!Trailer::sealed(&[]));
        assert!(!Trailer::sealed(&[0u8; COLUMNAR_TRAILER_SIZE]));
        assert!(Trailer::decode(&[]).unwrap_err().is_corruption());
        assert!(
            Trailer::decode(&[0u8; COLUMNAR_TRAILER_SIZE - 1])
                .unwrap_err()
                .is_corruption()
        );
        assert!(
            Trailer::decode(&[0u8; COLUMNAR_TRAILER_SIZE])
                .unwrap_err()
                .to_string()
                .contains("bad magic")
        );

        // A flipped byte inside the checksummed region is caught by the trailer's own CRC.
        let mut bytes = Trailer::new(1, b"footer").encode();
        bytes[0] ^= 0x01;
        assert!(
            Trailer::decode(&bytes)
                .unwrap_err()
                .to_string()
                .contains("trailer checksum")
        );

        // A future format version says so instead of guessing.
        let mut bytes = Trailer::new(1, b"footer").encode();
        bytes[20] = 3;
        assert!(
            Trailer::decode(&bytes)
                .unwrap_err()
                .to_string()
                .contains("format version 3")
        );

        // And so does version 1, which bound a chunk's checksum to nothing but its own bytes.
        let mut bytes = Trailer::new(1, b"footer").encode();
        bytes[20] = 1;
        assert!(
            Trailer::decode(&bytes)
                .unwrap_err()
                .to_string()
                .contains("format version 1")
        );

        // A footer length nothing could hold.
        let mut trailer = Trailer::new(1, b"footer");
        trailer.footer_len = MAX_FOOTER_LEN + 1;
        assert!(
            Trailer::decode(&trailer.encode())
                .unwrap_err()
                .to_string()
                .contains("over the")
        );
    }

    #[test]
    fn the_magic_is_frozen() {
        assert_eq!(COLUMNAR_MAGIC, *b"ESKERCOL");
        assert_eq!(COLUMNAR_TRAILER_SIZE, 32);
        assert_eq!(COLUMNAR_FORMAT_VERSION, 2);
    }

    #[test]
    fn a_footer_round_trips() {
        let footer = footer();
        let bytes = footer.encode();
        assert_eq!(Footer::decode(&bytes).unwrap(), footer);
        assert_eq!(footer.rows(), 1003);
    }

    /// Every truncation of a footer is an error and none of them is a panic.
    #[test]
    fn a_short_footer_never_panics() {
        let bytes = footer().encode();
        for cut in 0..bytes.len() {
            assert!(
                Footer::decode(&bytes[..cut]).is_err(),
                "a footer cut at {cut} decoded"
            );
        }
        assert!(Footer::decode(&[]).is_err());
    }

    /// Trailing bytes mean the reader and the writer disagree, which must not pass silently.
    #[test]
    fn a_footer_with_bytes_to_spare_is_corruption() {
        let mut bytes = footer().encode();
        bytes.push(0);
        assert!(
            Footer::decode(&bytes)
                .unwrap_err()
                .to_string()
                .contains("past the end")
        );
    }

    /// An enormous column count must be refused before a single allocation.
    #[test]
    fn a_dishonest_column_count_is_refused() {
        let mut bytes = Vec::new();
        esker_base::varint::put_u64(u64::MAX, &mut bytes);
        let error = Footer::decode(&bytes).unwrap_err();
        assert!(error.is_corruption(), "{error}");

        let mut bytes = Vec::new();
        esker_base::varint::put_u64(100_000, &mut bytes);
        bytes.extend_from_slice(&[0u8; 64]);
        assert!(Footer::decode(&bytes).unwrap_err().is_corruption());
    }
}
