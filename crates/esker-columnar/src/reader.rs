//! Reading a columnar file: open by the trailer, then decode only what was asked for.
//!
//! Opening reads two regions and no more — the fixed trailer at the end of the file, and the
//! footer it points at. After that every question a pruner asks (how many stripes, how many rows
//! in each, what range each column chunk covers, how many of its rows are NULL) is answered from
//! memory, and reading one column of one stripe is a single seek whose offset and length came
//! from the footer. That is the whole reason the layout is shaped the way it is.
//!
//! # What "never returns partial data as complete" means here
//!
//! Three separate checks stand between a damaged file and a wrong answer, and they are separate
//! because they catch different damage:
//!
//! 1. **The trailer.** No magic means the file was never finished — [`Error::Unsealed`], and the
//!    file is discardable. Magic with a failing self-checksum means it was finished and has since
//!    rotted — corruption, and an alarm.
//! 2. **The footer's checksum and its arithmetic.** The CRC comes from the trailer, and then
//!    every offset and length in the stripe index is checked to lie inside the data region
//!    ([`Reader::open`] does this once, at open, so no later read can walk off the file).
//! 3. **The chunk.** Its own CRC covers its payload and codec byte, and its header repeats the
//!    row count, null count and encoding the footer stated. A disagreement is corruption rather
//!    than something to resolve.
//!
//! A short read where the format promised more bytes is corruption too, never a truncated value
//! quietly returned.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use esker_engine::fs::{FileSystem, RandomAccessFile};

use crate::column::Column;
use crate::encode::decode_column;
use crate::error::{Error, IoResultExt, Result};
use crate::footer::{Footer, StripeMeta, Trailer};
use crate::format::COLUMNAR_TRAILER_SIZE;
use crate::frame::decode_chunk;
use crate::value::Schema;

/// What a reader has actually read, since it was opened or last reset.
///
/// The instrument behind "decode only the columns that were projected", which is the claim the
/// whole crate rests on and which nothing else can check: a refactor that quietly read every
/// column would still return the right answers. A test counts these instead of trusting the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadCounters {
    /// Column chunks decoded.
    pub columns_decoded: u64,
    /// Bytes read from the file, chunk framing included, the footer excluded.
    pub bytes_read: u64,
}

/// One open columnar file.
pub struct Reader {
    file: Box<dyn RandomAccessFile>,
    path: PathBuf,
    footer: Footer,
    columns_decoded: AtomicU64,
    bytes_read: AtomicU64,
}

/// Hand-written because a [`RandomAccessFile`] is a trait object with no `Debug` of its own.
impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("path", &self.path)
            .field("columns", &self.footer.schema.len())
            .field("stripes", &self.footer.stripes.len())
            .field("rows", &self.footer.rows())
            .finish_non_exhaustive()
    }
}

impl Reader {
    /// Opens `path`, reading its trailer and footer.
    ///
    /// Returns [`Error::Unsealed`] for a file no writer ever finished, which is the expected
    /// shape of a crash and is not corruption — see the module docs.
    pub fn open(fs: &dyn FileSystem, path: &Path) -> Result<Self> {
        let file = fs.open(path).at(path)?;
        let size = file.size().at(path)?;
        if size < COLUMNAR_TRAILER_SIZE as u64 {
            return Err(Error::Unsealed {
                path: path.to_path_buf(),
            });
        }

        let mut tail = [0u8; COLUMNAR_TRAILER_SIZE];
        read_exact_at(&*file, path, size - COLUMNAR_TRAILER_SIZE as u64, &mut tail)?;
        if !Trailer::sealed(&tail) {
            return Err(Error::Unsealed {
                path: path.to_path_buf(),
            });
        }
        let trailer = Trailer::decode(&tail)?;

        let context = path.display().to_string();
        let data_end = size - COLUMNAR_TRAILER_SIZE as u64;
        let footer_len = u64::from(trailer.footer_len);
        if trailer.footer_offset > data_end || footer_len > data_end - trailer.footer_offset {
            return Err(Error::corruption(
                context,
                format!(
                    "the trailer puts a {footer_len}-byte footer at {} in a file of {size}",
                    trailer.footer_offset
                ),
            ));
        }

        let mut bytes = vec![0u8; trailer.footer_len as usize];
        read_exact_at(&*file, path, trailer.footer_offset, &mut bytes)?;
        let actual = esker_base::crc32c::checksum(&bytes);
        if actual != trailer.footer_crc {
            return Err(Error::corruption(
                context,
                format!(
                    "footer checksum {actual:#010x} does not match the stored {:#010x}",
                    trailer.footer_crc
                ),
            ));
        }

        let footer = Footer::decode(&bytes)?;
        validate(&footer, trailer.footer_offset, &context)?;
        tracing::debug!(
            path = %path.display(),
            rows = footer.rows(),
            stripes = footer.stripes.len(),
            "opened a columnar file"
        );
        Ok(Self {
            file,
            path: path.to_path_buf(),
            footer,
            columns_decoded: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
        })
    }

    /// The columns this file holds, in the order its stripes store them.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.footer.schema
    }

    /// The stripes, with every chunk's position and statistics.
    #[must_use]
    pub fn stripes(&self) -> &[StripeMeta] {
        &self.footer.stripes
    }

    /// Rows across every stripe.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.footer.rows()
    }

    /// What this reader has read so far.
    #[must_use]
    pub fn counters(&self) -> ReadCounters {
        ReadCounters {
            columns_decoded: self.columns_decoded.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
        }
    }

    /// Zeroes the counters, so a test can measure one scan rather than a reader's whole life.
    pub fn reset_counters(&self) {
        self.columns_decoded.store(0, Ordering::Relaxed);
        self.bytes_read.store(0, Ordering::Relaxed);
    }

    /// Decodes one column of one stripe, reading only that chunk's bytes.
    pub fn read_column(&self, stripe: usize, column: usize) -> Result<Column> {
        let meta = self.footer.stripes.get(stripe).ok_or_else(|| {
            Error::InvalidArgument(format!(
                "stripe {stripe} of a file that has {}",
                self.footer.stripes.len()
            ))
        })?;
        let chunk = meta.columns.get(column).ok_or_else(|| {
            Error::InvalidArgument(format!(
                "column {column} of a file that has {}",
                meta.columns.len()
            ))
        })?;
        let ty = self.footer.schema.column_type(column)?;

        // `validate` bounded this length by the file's own size at open, so the allocation is one
        // the footer's arithmetic already justified against bytes that exist.
        let len = usize::try_from(chunk.len).map_err(|_| {
            Error::corruption(
                self.path.display().to_string(),
                format!(
                    "a chunk of {} bytes on a machine that cannot address it",
                    chunk.len
                ),
            )
        })?;
        let mut raw = vec![0u8; len];
        read_exact_at(&*self.file, &self.path, chunk.offset, &mut raw)?;
        self.columns_decoded.fetch_add(1, Ordering::Relaxed);
        self.bytes_read.fetch_add(chunk.len, Ordering::Relaxed);
        let context = format!("{} stripe {stripe} column {column}", self.path.display());
        let payload = decode_chunk(&raw, &context)?;
        decode_column(ty, meta.rows, chunk.encoding, &payload)
    }

    /// Decodes the named columns of one stripe, in the order they were named.
    ///
    /// The projection is the point: a fragment that touches two of forty columns reads two
    /// chunks, which is the arithmetic ADR 0022's cost rule is stated in.
    pub fn read_stripe(&self, stripe: usize, columns: &[usize]) -> Result<Vec<Column>> {
        columns
            .iter()
            .map(|column| self.read_column(stripe, *column))
            .collect()
    }
}

/// Checks that every offset and length in the stripe index addresses bytes the file actually has.
///
/// Done once, at open, so that no later read can walk off the end and no chunk length can become
/// an allocation the footer did not justify. `data_end` is where the footer begins, which is one
/// past the last byte any stripe may occupy.
fn validate(footer: &Footer, data_end: u64, context: &str) -> Result<()> {
    let columns = footer.schema.len();
    let mut previous_end = 0u64;
    for (index, stripe) in footer.stripes.iter().enumerate() {
        if stripe.columns.len() != columns {
            return Err(Error::corruption(
                context,
                format!(
                    "stripe {index} has {} chunks for {columns} columns",
                    stripe.columns.len()
                ),
            ));
        }
        let end = stripe
            .offset
            .checked_add(stripe.len)
            .filter(|end| *end <= data_end)
            .ok_or_else(|| {
                Error::corruption(
                    context,
                    format!(
                        "stripe {index} spans {}..+{} in {data_end} bytes of data",
                        stripe.offset, stripe.len
                    ),
                )
            })?;
        if stripe.offset < previous_end {
            return Err(Error::corruption(
                context,
                format!(
                    "stripe {index} starts at {}, inside its predecessor",
                    stripe.offset
                ),
            ));
        }
        previous_end = end;

        for (column, chunk) in stripe.columns.iter().enumerate() {
            let fits = chunk
                .offset
                .checked_add(chunk.len)
                .is_some_and(|chunk_end| chunk.offset >= stripe.offset && chunk_end <= end);
            if !fits {
                return Err(Error::corruption(
                    context,
                    format!(
                        "stripe {index} column {column} spans {}..+{}, outside its stripe",
                        chunk.offset, chunk.len
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Fills `buf` from `offset`, treating a short read as corruption: the format promised the bytes.
fn read_exact_at(
    file: &dyn RandomAccessFile,
    path: &Path,
    offset: u64,
    buf: &mut [u8],
) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let read = file
            .read_at(offset + filled as u64, &mut buf[filled..])
            .at(path)?;
        if read == 0 {
            return Err(Error::corruption(
                path.display().to_string(),
                format!(
                    "wanted {} bytes at {offset} and the file ended after {filled}",
                    buf.len()
                ),
            ));
        }
        filled += read;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use esker_engine::fs::FileSystem;
    use esker_engine::memfs::MemFileSystem;

    use super::Reader;
    use crate::value::{ColumnDef, ColumnType, Schema, Value};
    use crate::writer::{Writer, WriterOptions};

    fn schema() -> Schema {
        Schema::new(vec![
            ColumnDef::new("id", ColumnType::Int8),
            ColumnDef::new("body", ColumnType::Text),
            ColumnDef::new("ok", ColumnType::Bool),
        ])
        .unwrap()
    }

    fn row(id: i64) -> Vec<Value> {
        vec![
            Value::Int8(id),
            if id % 7 == 0 {
                Value::Null
            } else {
                Value::Text(format!("body-{}", id % 5))
            },
            Value::Bool(id % 3 == 0),
        ]
    }

    fn write(fs: &MemFileSystem, path: &Path, rows: i64, options: WriterOptions) {
        fs.create_dir_all(path.parent().unwrap()).unwrap();
        let mut writer = Writer::create(fs, path, schema(), options).unwrap();
        for id in 0..rows {
            writer.append_row(&row(id)).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn a_file_reads_back_row_for_row() {
        let fs = MemFileSystem::new();
        let path = Path::new("/c/t.col");
        let options = WriterOptions {
            stripe_rows: 37,
            ..WriterOptions::default()
        };
        write(&fs, path, 300, options);

        let reader = Reader::open(&fs, path).unwrap();
        assert_eq!(reader.schema(), &schema());
        assert_eq!(reader.rows(), 300);
        assert!(reader.stripes().len() > 1);

        let mut id = 0i64;
        for (index, stripe) in reader.stripes().iter().enumerate() {
            let columns = reader.read_stripe(index, &[0, 1, 2]).unwrap();
            assert_eq!(columns[0].rows() as u64, stripe.rows);
            for offset in 0..columns[0].rows() {
                let got: Vec<_> = columns
                    .iter()
                    .map(|column| column.to_values().unwrap().into_iter().nth(offset).unwrap())
                    .collect();
                assert_eq!(got, row(id), "row {id}");
                id += 1;
            }
        }
        assert_eq!(id, 300);
    }

    /// The projection is the point: one column of one stripe is one chunk's bytes.
    #[test]
    fn a_projection_reads_only_what_it_named() {
        let fs = MemFileSystem::new();
        let path = Path::new("/c/p.col");
        write(&fs, path, 100, WriterOptions::default());

        let reader = Reader::open(&fs, path).unwrap();
        let only = reader.read_stripe(0, &[2]).unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].ty(), ColumnType::Bool);

        // And the same column twice, in the order it was named.
        let twice = reader.read_stripe(0, &[1, 1]).unwrap();
        assert!(twice[0].identical(&twice[1]));

        assert!(reader.read_column(0, 3).is_err(), "no such column");
        assert!(reader.read_column(9, 0).is_err(), "no such stripe");
    }

    #[test]
    fn an_empty_file_opens_and_holds_nothing() {
        let fs = MemFileSystem::new();
        let path = Path::new("/c/empty.col");
        write(&fs, path, 0, WriterOptions::default());

        let reader = Reader::open(&fs, path).unwrap();
        assert_eq!(reader.rows(), 0);
        assert!(reader.stripes().is_empty());
        assert_eq!(reader.schema().len(), 3);
        assert!(reader.read_column(0, 0).is_err());
    }

    #[test]
    fn a_file_that_was_never_finished_is_unsealed_not_corrupt() {
        let fs = MemFileSystem::new();
        fs.create_dir_all(Path::new("/c")).unwrap();
        let path = Path::new("/c/torn.col");

        fs.install(path, Vec::new()).unwrap();
        assert!(Reader::open(&fs, path).unwrap_err().is_unsealed(), "empty");

        fs.install(path, vec![0u8; 31]).unwrap();
        assert!(Reader::open(&fs, path).unwrap_err().is_unsealed(), "short");

        fs.install(path, vec![0u8; 4096]).unwrap();
        assert!(
            Reader::open(&fs, path).unwrap_err().is_unsealed(),
            "no magic"
        );

        // A whole file with its last byte lost: the magic goes with it.
        write(&fs, Path::new("/c/whole.col"), 50, WriterOptions::default());
        let mut bytes = fs.contents(Path::new("/c/whole.col")).unwrap();
        bytes.pop();
        fs.install(path, bytes).unwrap();
        assert!(Reader::open(&fs, path).unwrap_err().is_unsealed());
    }

    /// A sealed file whose bytes have rotted is an alarm, not a discardable leftover.
    #[test]
    fn damage_to_a_sealed_file_is_corruption() {
        let fs = MemFileSystem::new();
        let path = Path::new("/c/rot.col");
        write(&fs, path, 200, WriterOptions::default());
        let good = fs.contents(path).unwrap();

        // The trailer's own checksum.
        let mut bytes = good.clone();
        let at = bytes.len() - 32;
        bytes[at] ^= 0x01;
        fs.install(path, bytes).unwrap();
        assert!(Reader::open(&fs, path).unwrap_err().is_corruption());

        // The footer's checksum, which lives in the trailer.
        let mut bytes = good.clone();
        let at = bytes.len() - 40;
        bytes[at] ^= 0x01;
        fs.install(path, bytes).unwrap();
        assert!(Reader::open(&fs, path).unwrap_err().is_corruption());

        // A chunk's own checksum: the file still opens, and the damaged column is the one that
        // fails. Every other column is untouched and must still read, which is the property that
        // makes a per-chunk checksum worth more than a per-file one.
        fs.install(path, good.clone()).unwrap();
        let offsets: Vec<u64> = Reader::open(&fs, path).unwrap().stripes()[0]
            .columns
            .iter()
            .map(|chunk| chunk.offset)
            .collect();
        for (column, offset) in offsets.iter().enumerate() {
            let mut bytes = good.clone();
            bytes[usize::try_from(*offset).unwrap()] ^= 0x01;
            fs.install(path, bytes).unwrap();
            let reader = Reader::open(&fs, path).unwrap();
            assert!(
                reader.read_column(0, column).unwrap_err().is_corruption(),
                "column {column} did not notice a flipped byte"
            );
            for other in 0..offsets.len() {
                if other != column {
                    assert!(reader.read_column(0, other).is_ok(), "column {other} broke");
                }
            }
        }
    }

    #[test]
    fn a_stripe_index_that_points_outside_the_file_is_refused_at_open() {
        use crate::footer::{Footer, Trailer};

        let fs = MemFileSystem::new();
        let path = Path::new("/c/liar.col");
        write(&fs, path, 50, WriterOptions::default());
        let bytes = fs.contents(path).unwrap();

        let trailer = Trailer::decode(&bytes[bytes.len() - 32..]).unwrap();
        let start = usize::try_from(trailer.footer_offset).unwrap();
        let mut footer =
            Footer::decode(&bytes[start..start + trailer.footer_len as usize]).unwrap();
        footer.stripes[0].columns[0].len = 1 << 40;

        let payload = footer.encode();
        let mut forged = bytes[..start].to_vec();
        forged.extend_from_slice(&payload);
        forged.extend_from_slice(&Trailer::new(trailer.footer_offset, &payload).encode());
        fs.install(path, forged).unwrap();

        let error = Reader::open(&fs, path).unwrap_err();
        assert!(error.is_corruption(), "{error}");
        assert!(error.to_string().contains("outside its stripe"), "{error}");
    }
}
