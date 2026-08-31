//! Writing a columnar file: rows in, stripes out, and a trailer that commits the lot.
//!
//! The writer is streaming — rows arrive one at a time and are accumulated per column until a
//! stripe is full, at which point every column is encoded, framed and appended. Nothing is held
//! for longer than one stripe, so memory is bounded by [`WriterOptions::stripe_bytes`] rather
//! than by the size of the file.
//!
//! # The commit, and why it is a rename
//!
//! ```text
//! stripes → footer → trailer → sync_data → rename → fsync the directory
//! ```
//!
//! Two independent things make that safe, and the format has both because they fail differently.
//! The **trailer** means a file read directly can be told apart from one a crash interrupted: the
//! magic is the last thing written, so its presence is the commit (invariant 3). The **rename**
//! means the final name never exists in a half-written state at all, so a directory listing never
//! offers a reader a file it would have to reject.
//!
//! A writer that is dropped without [`Writer::finish`] leaves its temporary behind. That is the
//! correct outcome and not a leak to paper over: the caller is the only one who knows whether the
//! file was abandoned deliberately, and a `Drop` that deleted it would delete it on a panic in the
//! middle of a legitimate write too.
//!
//! # Stripes
//!
//! A stripe is sealed when it reaches [`WriterOptions::stripe_rows`] rows **or**
//! [`WriterOptions::stripe_bytes`] of accumulated values, whichever comes first. Both bounds are
//! needed: a row count alone lets a column of long strings produce a chunk too large to decode,
//! and a byte budget alone lets a narrow table produce stripes with millions of rows and a
//! statistics entry too coarse to prune with.

use std::path::{Path, PathBuf};

use esker_engine::fs::{FileSystem, WritableFile};

use crate::column::ColumnBuilder;
use crate::encode::encode_column;
use crate::error::{Error, IoResultExt, Result};
use crate::footer::{ChunkMeta, Footer, StripeMeta, Trailer};
use crate::format::{MAX_COLUMN_BYTES, MAX_STRIPE_ROWS};
use crate::frame::{Compression, encode_chunk};
use crate::stats::ColumnStats;
use crate::value::{Schema, Value};

/// How a file is cut into stripes, and whether its chunks are compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterOptions {
    /// Rows after which a stripe is sealed.
    pub stripe_rows: usize,
    /// Accumulated value bytes after which a stripe is sealed, whichever comes first.
    pub stripe_bytes: usize,
    /// The codec applied to each encoded chunk, when it pays.
    pub compression: Compression,
}

impl Default for WriterOptions {
    /// 64Ki rows or 8 MiB per stripe, LZ4 on top.
    ///
    /// The byte budget matches the engine's default SST size, for the same reason: it is the
    /// largest thing this system is willing to hold in memory to produce one immutable file.
    fn default() -> Self {
        Self {
            stripe_rows: 64 * 1024,
            stripe_bytes: 8 * 1024 * 1024,
            compression: Compression::Lz4,
        }
    }
}

/// What one finished file turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSummary {
    /// Rows across every stripe.
    pub rows: u64,
    /// How many stripes were written.
    pub stripes: usize,
    /// The file's size in bytes, trailer included.
    pub bytes: u64,
}

/// Builds one columnar file.
pub struct Writer<'a> {
    fs: &'a dyn FileSystem,
    path: PathBuf,
    temp: PathBuf,
    file: Option<Box<dyn WritableFile>>,
    schema: Schema,
    options: WriterOptions,
    builders: Vec<ColumnBuilder>,
    stripes: Vec<StripeMeta>,
    offset: u64,
    stripe_rows: usize,
}

/// Hand-written because a [`WritableFile`] is a trait object with no `Debug` of its own, and
/// because what is worth printing about a half-written file is where it is and how far it got.
impl std::fmt::Debug for Writer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("path", &self.path)
            .field("columns", &self.schema.len())
            .field("stripes", &self.stripes.len())
            .field("offset", &self.offset)
            .field("pending_rows", &self.stripe_rows)
            .finish_non_exhaustive()
    }
}

impl<'a> Writer<'a> {
    /// Starts a file at `path`, writing through `<path>.tmp` until [`Writer::finish`].
    ///
    /// Truncates an existing temporary, which is what makes a retry after a crash work: the
    /// leftover from the interrupted attempt is not something anybody has to clean up first.
    pub fn create(
        fs: &'a dyn FileSystem,
        path: &Path,
        schema: Schema,
        options: WriterOptions,
    ) -> Result<Self> {
        if options.stripe_rows == 0 || options.stripe_bytes == 0 {
            return Err(Error::InvalidArgument(
                "a stripe budget of zero would never seal a stripe".into(),
            ));
        }
        let temp = temp_path(path);
        let file = fs.create(&temp).at(&temp)?;
        let builders = schema
            .columns()
            .iter()
            .map(|column| ColumnBuilder::new(column.ty))
            .collect();
        Ok(Self {
            fs,
            path: path.to_path_buf(),
            temp,
            file: Some(file),
            schema,
            options,
            builders,
            stripes: Vec::new(),
            offset: 0,
            stripe_rows: 0,
        })
    }

    /// The columns this file is being written with.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Appends one row, which must have one value per column, in schema order.
    ///
    /// Seals the current stripe when either budget is reached, so a caller that only ever calls
    /// this and [`Writer::finish`] still gets a properly striped file.
    pub fn append_row(&mut self, values: &[Value]) -> Result<()> {
        if values.len() != self.schema.len() {
            return Err(Error::InvalidArgument(format!(
                "a row of {} values for a schema of {} columns",
                values.len(),
                self.schema.len()
            )));
        }
        for (builder, value) in self.builders.iter_mut().zip(values) {
            builder.push(value)?;
        }
        self.stripe_rows += 1;

        if is_full(self.stripe_rows, self.accumulated(), &self.options) {
            self.seal_stripe()?;
        }
        Ok(())
    }

    /// Bytes the current stripe's values occupy in memory.
    fn accumulated(&self) -> usize {
        self.builders.iter().map(ColumnBuilder::heap_bytes).sum()
    }

    /// Rows appended to the stripe now being built.
    #[must_use]
    pub fn pending_rows(&self) -> usize {
        self.stripe_rows
    }

    /// Seals the current stripe, encoding and appending every column. A no-op when it is empty:
    /// a stripe of no rows would be an entry in the index that no reader could ever want.
    pub fn seal_stripe(&mut self) -> Result<()> {
        if self.stripe_rows == 0 {
            return Ok(());
        }
        let rows = self.stripe_rows as u64;
        let start = self.offset;
        let mut chunks = Vec::with_capacity(self.builders.len());

        for index in 0..self.builders.len() {
            let column = self.builders[index].finish()?;
            let (encoding, payload) = encode_column(&column)?;
            if payload.len() > MAX_COLUMN_BYTES {
                return Err(Error::InvalidArgument(format!(
                    "column {index} of this stripe encodes to {} bytes, over the \
                     {MAX_COLUMN_BYTES} a reader will decode",
                    payload.len()
                )));
            }
            let framed = encode_chunk(&payload, self.options.compression);
            let offset = self.offset;
            self.append(&framed)?;
            chunks.push(ChunkMeta {
                offset,
                len: framed.len() as u64,
                encoding,
                stats: ColumnStats::of(&column),
            });
        }

        self.stripes.push(StripeMeta {
            rows,
            offset: start,
            len: self.offset - start,
            columns: chunks,
        });
        self.stripe_rows = 0;
        Ok(())
    }

    /// Seals what is left, writes the footer and the trailer, and renames the file into place.
    pub fn finish(mut self) -> Result<FileSummary> {
        self.seal_stripe()?;

        let footer = Footer {
            schema: self.schema.clone(),
            stripes: std::mem::take(&mut self.stripes),
        };
        let payload = footer.encode();
        let footer_offset = self.offset;
        self.append(&payload)?;
        let trailer = Trailer::new(footer_offset, &payload);
        self.append(&trailer.encode())?;

        let mut file = self
            .file
            .take()
            .ok_or_else(|| Error::InvalidArgument("this writer is already finished".into()))?;
        file.sync_data().at(&self.temp)?;
        drop(file);

        self.fs.rename(&self.temp, &self.path).at(&self.path)?;
        if let Some(dir) = self.path.parent() {
            self.fs.fsync_dir(dir).at(dir)?;
        }

        let summary = FileSummary {
            rows: footer.rows(),
            stripes: footer.stripes.len(),
            bytes: self.offset,
        };
        tracing::debug!(
            path = %self.path.display(),
            rows = summary.rows,
            stripes = summary.stripes,
            bytes = summary.bytes,
            "sealed a columnar file"
        );
        Ok(summary)
    }

    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| Error::InvalidArgument("this writer is already finished".into()))?;
        file.append(bytes).at(&self.temp)?;
        self.offset += bytes.len() as u64;
        Ok(())
    }
}

/// Whether a stripe holding `rows` rows and `bytes` of accumulated values is full.
///
/// [`MAX_STRIPE_ROWS`] is the **format's** cap and outranks the options, because a caller may
/// legitimately set `stripe_rows` to `usize::MAX` and lean entirely on the byte budget — and a
/// narrow table would then build a stripe with more rows than any reader will accept. A limit a
/// writer can be configured past is not a limit.
fn is_full(rows: usize, bytes: usize, options: &WriterOptions) -> bool {
    rows >= options.stripe_rows.min(MAX_STRIPE_ROWS) || bytes >= options.stripe_bytes
}

/// `<path>.tmp`, by extending the file name rather than replacing its extension — a columnar file
/// may legitimately have none, and `with_extension` would then rewrite the name itself.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use esker_engine::fs::FileSystem;
    use esker_engine::memfs::MemFileSystem;

    use super::{Writer, WriterOptions, is_full, temp_path};
    use crate::value::{ColumnDef, ColumnType, Schema, Value};

    fn schema() -> Schema {
        Schema::new(vec![
            ColumnDef::new("id", ColumnType::Int8),
            ColumnDef::new("body", ColumnType::Text),
        ])
        .unwrap()
    }

    #[test]
    fn a_temporary_extends_the_name_rather_than_replacing_it() {
        assert_eq!(temp_path(Path::new("/d/t.col")), Path::new("/d/t.col.tmp"));
        assert_eq!(temp_path(Path::new("/d/t")), Path::new("/d/t.tmp"));
    }

    #[test]
    fn a_file_appears_only_when_it_is_finished() {
        let fs = MemFileSystem::new();
        fs.create_dir_all(Path::new("/c")).unwrap();
        let path = Path::new("/c/one.col");

        let mut writer = Writer::create(&fs, path, schema(), WriterOptions::default()).unwrap();
        for id in 0..100i64 {
            writer
                .append_row(&[Value::Int8(id), Value::Text(format!("row-{id}"))])
                .unwrap();
        }
        assert!(!fs.exists(path).unwrap(), "the file exists before finish");
        assert!(fs.exists(&temp_path(path)).unwrap(), "no temporary");

        let summary = writer.finish().unwrap();
        assert!(fs.exists(path).unwrap());
        assert!(
            !fs.exists(&temp_path(path)).unwrap(),
            "the temporary survived"
        );
        assert_eq!(summary.rows, 100);
        assert_eq!(summary.stripes, 1);
        assert_eq!(summary.bytes, fs.contents(path).unwrap().len() as u64);
    }

    #[test]
    fn both_stripe_budgets_seal() {
        let fs = MemFileSystem::new();
        fs.create_dir_all(Path::new("/c")).unwrap();

        let options = WriterOptions {
            stripe_rows: 10,
            ..WriterOptions::default()
        };
        let mut writer = Writer::create(&fs, Path::new("/c/rows.col"), schema(), options).unwrap();
        for id in 0..25i64 {
            writer.append_row(&[Value::Int8(id), Value::Null]).unwrap();
        }
        assert_eq!(writer.pending_rows(), 5);
        assert_eq!(writer.finish().unwrap().stripes, 3);

        let options = WriterOptions {
            stripe_rows: usize::MAX,
            stripe_bytes: 4096,
            ..WriterOptions::default()
        };
        let mut writer = Writer::create(&fs, Path::new("/c/bytes.col"), schema(), options).unwrap();
        for id in 0..1000i64 {
            writer
                .append_row(&[Value::Int8(id), Value::Text("x".repeat(64))])
                .unwrap();
        }
        assert!(
            writer.finish().unwrap().stripes > 10,
            "the byte budget never sealed"
        );
    }

    #[test]
    fn a_row_of_the_wrong_width_or_type_is_refused() {
        let fs = MemFileSystem::new();
        fs.create_dir_all(Path::new("/c")).unwrap();
        let mut writer = Writer::create(
            &fs,
            Path::new("/c/bad.col"),
            schema(),
            WriterOptions::default(),
        )
        .unwrap();

        assert!(writer.append_row(&[Value::Int8(1)]).is_err(), "too narrow");
        assert!(
            writer
                .append_row(&[Value::Int8(1), Value::Text("a".into()), Value::Null])
                .is_err(),
            "too wide"
        );
        assert!(
            writer
                .append_row(&[Value::Text("a".into()), Value::Int8(1)])
                .is_err(),
            "wrong types"
        );
        assert!(writer.append_row(&[Value::Null, Value::Null]).is_ok());
    }

    #[test]
    fn a_stripe_budget_of_zero_is_refused() {
        let fs = MemFileSystem::new();
        fs.create_dir_all(Path::new("/c")).unwrap();
        let options = WriterOptions {
            stripe_rows: 0,
            ..WriterOptions::default()
        };
        assert!(Writer::create(&fs, Path::new("/c/z.col"), schema(), options).is_err());
    }

    /// An empty file is still a file: a footer, a trailer and no stripes.
    #[test]
    fn a_file_with_no_rows_is_still_sealed() {
        let fs = MemFileSystem::new();
        fs.create_dir_all(Path::new("/c")).unwrap();
        let path = Path::new("/c/empty.col");
        let writer = Writer::create(&fs, path, schema(), WriterOptions::default()).unwrap();
        let summary = writer.finish().unwrap();
        assert_eq!(summary.rows, 0);
        assert_eq!(summary.stripes, 0);
        assert!(fs.exists(path).unwrap());
    }

    /// The format's own cap outranks the options, so a caller leaning entirely on the byte
    /// budget cannot build a stripe no reader would accept.
    ///
    /// Four million rows is too slow to write in a test, so the decision itself is the unit —
    /// which is why it is a function rather than a condition inlined in `append_row`.
    #[test]
    fn no_stripe_exceeds_the_format_cap() {
        use crate::format::MAX_STRIPE_ROWS;

        let unbounded = WriterOptions {
            stripe_rows: usize::MAX,
            stripe_bytes: usize::MAX,
            ..WriterOptions::default()
        };
        assert!(!is_full(MAX_STRIPE_ROWS - 1, 0, &unbounded));
        assert!(
            is_full(MAX_STRIPE_ROWS, 0, &unbounded),
            "an unbounded option let a stripe past the format's own cap"
        );

        // And the ordinary budgets still decide below it.
        let small = WriterOptions {
            stripe_rows: 10,
            stripe_bytes: 100,
            ..WriterOptions::default()
        };
        assert!(!is_full(9, 99, &small));
        assert!(is_full(10, 0, &small), "the row budget");
        assert!(is_full(0, 100, &small), "the byte budget");
    }
}
