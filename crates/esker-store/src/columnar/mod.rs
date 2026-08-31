//! The columnar apply target: a region applied into columns instead of rows.
//!
//! ADR 0022 Decision 1 in one sentence — *a columnar replica is a Raft **learner** whose apply
//! writes columns instead of rows*. Everything about the peer above and below the apply is
//! unchanged: the log, the snapshot protocol, membership, and the applied index reported to PD.
//! That is what makes this a new **apply target** rather than a new replication system, and it is
//! why nothing in `esker-raft` grows to support it.
//!
//! # Why this buffers, and why that is not a shortcut
//!
//! Runs are sorted by `(key, commit_ts DESC)` so that a read at `ts` finds the newest visible
//! version of a key as the *first* row for that key with `commit_ts <= ts` — one forward pass, no
//! hashing (`docs/plans/phase-8-learner.md`, RULED-2). Raft hands entries over in **apply order**,
//! which is not key order, so a streaming writer could not produce that file at all: the rows have
//! to be in hand before any of them can be placed.
//!
//! So this is a memtable, which is exactly what ADR 0022 said it would be — *"appending an apply
//! stream column-wise produces small runs exactly as a memtable flush does"*. The sort requirement
//! makes the buffering necessary rather than merely convenient, which is worth saying because
//! "buffer it all first" otherwise reads as the lazy option.
//!
//! # What a row becomes
//!
//! The run's schema is the table's own columns followed by two the apply target adds:
//!
//! | | |
//! |---|---|
//! | `__commit_ts` | the version, as committed. Never derived, never a wall clock — it comes off the key (`esker_keys::prefix::split_ts`). |
//! | `__deleted` | whether this version is a tombstone. |
//!
//! **A delete is a version, not an absence.** It has to be: a read at a `ts` after the delete must
//! find the tombstone and report nothing, while a read *before* it must still find the row. An
//! apply target that dropped deletes would answer the second correctly and the first wrongly, and
//! only under time travel — which is the worst way to be wrong.

pub mod compact;
pub mod runs;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_columnar::{
    ColumnDef, ColumnType, Schema, Value, Writer, WriterOptions, format::COLUMNAR_MAGIC,
};
use esker_engine::fs::FileSystem;

use crate::error::{Result, StoreError};

/// The column carrying a version's commit timestamp.
pub const COMMIT_TS_COLUMN: &str = "__commit_ts";

/// The column carrying whether a version is a tombstone.
pub const DELETED_COLUMN: &str = "__deleted";

/// Turns a committed row into the typed columns of its table.
///
/// **The port, whose implementor lives elsewhere.** The store can read a key honestly —
/// `esker_keys` gives it tenant, table and the MVCC suffix — but not a row *value*, whose codec
/// belonged to `esker-sql`, a crate that depends on this one. Phase 8's RULED-1 moves that codec
/// down into `esker-keys`; until it lands this trait is implemented by the tests, and when it
/// lands nothing written against the trait changes. That is the whole reason it is a trait: the
/// question of *who constructs it* was open while the question of *what it does* never was.
pub trait RowDecoder: Send + Sync + fmt::Debug {
    /// The table's own columns, in slot order. The run adds two of its own after these.
    fn schema(&self) -> &Schema;

    /// Which slots form the primary key, in key order.
    ///
    /// The run is sorted by these before `__commit_ts`, so they are what "the newest version of a
    /// key" is a statement about.
    fn key_slots(&self) -> &[usize];

    /// Decodes one committed row.
    ///
    /// `value` is `None` for a delete, whose non-key columns come back NULL: a tombstone still
    /// has an identity, and it is the identity the visibility pass needs.
    fn decode(&self, key: &[u8], value: Option<&[u8]>) -> Result<Vec<Value>>;
}

/// How the apply target seals runs.
#[derive(Debug, Clone)]
pub struct ColumnarOptions {
    /// Buffered rows after which the memtable is sealed into a run.
    pub seal_rows: usize,
    /// Buffered value bytes after which it is sealed, whichever comes first.
    pub seal_bytes: usize,
    /// How the sealed file itself is written.
    pub writer: WriterOptions,
}

impl Default for ColumnarOptions {
    /// 256Ki rows or 64 MiB, which is the engine's memtable budget for the same reason: it is the
    /// largest thing this system will hold in memory to produce one immutable file.
    fn default() -> Self {
        Self {
            seal_rows: 256 * 1024,
            seal_bytes: 64 * 1024 * 1024,
            writer: WriterOptions::default(),
        }
    }
}

/// One buffered version, before it is placed.
#[derive(Debug, Clone)]
struct Row {
    /// The table's columns, then `__commit_ts`, then `__deleted`.
    values: Vec<Value>,
}

/// A region's columnar apply target.
///
/// Not `Sync`-shared: one of these belongs to one region's apply, which is single-threaded by the
/// driver's own pinning, so it needs no lock of its own.
pub struct ColumnarApply {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    decoder: Arc<dyn RowDecoder>,
    options: ColumnarOptions,
    /// The run schema: the decoder's columns plus `__commit_ts` and `__deleted`.
    schema: Schema,
    /// Slots the run is sorted by, before `__commit_ts`.
    key_slots: Vec<usize>,
    /// The slot `__commit_ts` landed in.
    ts_slot: usize,
    buffered: Vec<Row>,
    buffered_bytes: usize,
    /// The number the next sealed run takes.
    next_run: u64,
}

impl fmt::Debug for ColumnarApply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ColumnarApply")
            .field("dir", &self.dir)
            .field("buffered", &self.buffered.len())
            .field("buffered_bytes", &self.buffered_bytes)
            .field("next_run", &self.next_run)
            .finish_non_exhaustive()
    }
}

impl ColumnarApply {
    /// Opens the apply target for a region whose runs live under `dir`.
    ///
    /// Refuses a table that already has a column named like one of the two this adds, rather than
    /// letting [`Schema::new`]'s duplicate-name check fail later with a message about a file
    /// format. A collision here is a schema the operator can change.
    pub fn open(
        fs: Arc<dyn FileSystem>,
        dir: impl AsRef<Path>,
        decoder: Arc<dyn RowDecoder>,
        options: ColumnarOptions,
    ) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs.create_dir_all(&dir)
            .map_err(|error| StoreError::Bootstrap(format!("{}: {error}", dir.display())))?;

        let table = decoder.schema();
        for reserved in [COMMIT_TS_COLUMN, DELETED_COLUMN] {
            if table.columns().iter().any(|column| column.name == reserved) {
                return Err(StoreError::Bootstrap(format!(
                    "a columnar table may not have a column named {reserved}: the apply target \
                     adds one of its own"
                )));
            }
        }
        let mut columns = table.columns().to_vec();
        let ts_slot = columns.len();
        columns.push(ColumnDef::new(COMMIT_TS_COLUMN, ColumnType::Int8));
        columns.push(ColumnDef::new(DELETED_COLUMN, ColumnType::Bool));
        let schema = Schema::new(columns)
            .map_err(|error| StoreError::Bootstrap(format!("the run schema: {error}")))?;

        let key_slots = decoder.key_slots().to_vec();
        if key_slots.is_empty() || key_slots.iter().any(|slot| *slot >= ts_slot) {
            return Err(StoreError::Bootstrap(format!(
                "the key slots {key_slots:?} are empty or name a column the table does not have"
            )));
        }

        Ok(Self {
            fs,
            dir,
            decoder,
            options,
            schema,
            key_slots,
            ts_slot,
            buffered: Vec::new(),
            buffered_bytes: 0,
            next_run: 0,
        })
    }

    /// The run schema, table columns first.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Rows buffered and not yet sealed.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buffered.len()
    }

    /// Applies one committed version.
    ///
    /// `key` is the user key **with** its MVCC suffix, as the log carries it; the commit timestamp
    /// is taken from there rather than from anything this process knows, because a version's
    /// timestamp is a fact about the write and not about the replica applying it.
    pub fn apply(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        let (user_key, commit_ts) = esker_keys::prefix::split_ts(key).map_err(|error| {
            StoreError::Bootstrap(format!(
                "a columnar apply saw a key with no version: {error}"
            ))
        })?;
        let deleted = value.is_none();
        let mut values = self.decoder.decode(user_key, value)?;
        if values.len() != self.ts_slot {
            return Err(StoreError::Bootstrap(format!(
                "the decoder returned {} values for a {}-column table",
                values.len(),
                self.ts_slot
            )));
        }
        // `commit_ts` is a `u64` off the key and the column is PostgreSQL's `bigint`. A timestamp
        // past `i64::MAX` is not reachable from a TSO, and saturating is the honest answer to a
        // corrupt key: it sorts last rather than wrapping to the beginning of time.
        values.push(Value::Int8(i64::try_from(commit_ts).unwrap_or(i64::MAX)));
        values.push(Value::Bool(deleted));

        self.buffered_bytes += row_bytes(&values);
        self.buffered.push(Row { values });
        if self.buffered.len() >= self.options.seal_rows
            || self.buffered_bytes >= self.options.seal_bytes
        {
            self.seal()?;
        }
        Ok(())
    }

    /// Seals what is buffered into an immutable run, if anything is.
    ///
    /// Sorted by `(key slots, __commit_ts DESC)` on the way out — see this module's header for why
    /// that order, and why it is what forces the buffer. `Writer::finish` does the durable half:
    /// write, `sync_data`, atomic rename, `fsync` the directory (invariant 3).
    pub fn seal(&mut self) -> Result<Option<PathBuf>> {
        if self.buffered.is_empty() {
            return Ok(None);
        }
        let key_slots = self.key_slots.clone();
        let ts_slot = self.ts_slot;
        self.buffered.sort_by(|left, right| {
            for slot in &key_slots {
                let ordering = left.values[*slot].pg_cmp(&right.values[*slot]);
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
            // Newest first, so a read at `ts` takes the first row it sees at or below it.
            right.values[ts_slot].pg_cmp(&left.values[ts_slot])
        });

        let path = self.dir.join(run_name(self.next_run));
        let mut writer = Writer::create(
            self.fs.as_ref(),
            &path,
            self.schema.clone(),
            self.options.writer,
        )
        .map_err(|error| columnar_error(&error))?;
        for row in &self.buffered {
            writer
                .append_row(&row.values)
                .map_err(|error| columnar_error(&error))?;
        }
        let summary = writer.finish().map_err(|error| columnar_error(&error))?;

        tracing::debug!(
            path = %path.display(),
            rows = summary.rows,
            bytes = summary.bytes,
            "sealed a columnar run"
        );
        self.buffered.clear();
        self.buffered_bytes = 0;
        self.next_run += 1;
        Ok(Some(path))
    }
}

/// `NNNNNN.col` — six digits like the engine's own file names, and an extension of its own so
/// nothing here is ever mistaken for an SST by a sweep that classifies by name.
#[must_use]
pub fn run_name(number: u64) -> String {
    format!("{number:06}.col")
}

/// The run number a file name carries, if it is one of ours.
///
/// `None` for anything else, including the manifest — which is what keeps the sweep from
/// deleting the very file that tells it what to keep.
#[must_use]
pub fn run_number(name: &str) -> Option<u64> {
    name.strip_suffix(".col")?.parse().ok()
}

/// A rough size for the memtable budget. Deliberately approximate: it decides *when to seal*, and
/// a seal that happens a few kilobytes early costs nothing, while an exact accounting would cost a
/// walk of every value on the hot path.
fn row_bytes(values: &[Value]) -> usize {
    values
        .iter()
        .map(|value| match value {
            Value::Null => 1,
            Value::Text(text) => text.len() + 8,
            Value::Bytea(bytes) => bytes.len() + 8,
            _ => 8,
        })
        .sum()
}

fn columnar_error(error: &esker_columnar::Error) -> StoreError {
    StoreError::Bootstrap(format!("the columnar run: {error}"))
}

/// Kept so the magic is linked and a format change is a compile error here too.
const _: [u8; 8] = COLUMNAR_MAGIC;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use esker_columnar::{ColumnDef, ColumnType, Reader, Schema, Value};
    use esker_engine::fs::{FileSystem, LocalFileSystem};

    use super::{
        COMMIT_TS_COLUMN, ColumnarApply, ColumnarOptions, DELETED_COLUMN, RowDecoder, run_name,
    };
    use crate::error::{Result, StoreError};

    /// Stands in for RULED-1's decoder until the row codec lands in `esker-keys`.
    ///
    /// Deliberately trivial — `id:int8, name:text`, the key being the id in big-endian and the
    /// value the name — because what these tests are about is the apply target's behaviour, not
    /// anybody's row format. When the real decoder arrives this is still what the tests use: a
    /// differential against the real codec belongs in unit 4, not here.
    #[derive(Debug)]
    struct FakeDecoder {
        schema: Schema,
    }

    impl FakeDecoder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                schema: Schema::new(vec![
                    ColumnDef::new("id", ColumnType::Int8),
                    ColumnDef::new("name", ColumnType::Text),
                ])
                .unwrap(),
            })
        }
    }

    impl RowDecoder for FakeDecoder {
        fn schema(&self) -> &Schema {
            &self.schema
        }

        fn key_slots(&self) -> &[usize] {
            &[0]
        }

        fn decode(&self, key: &[u8], value: Option<&[u8]>) -> Result<Vec<Value>> {
            let id = i64::from_be_bytes(
                key.try_into()
                    .map_err(|_| StoreError::Bootstrap(format!("a {}-byte key", key.len())))?,
            );
            Ok(vec![
                Value::Int8(id),
                match value {
                    // A tombstone still has an identity; its non-key columns are NULL.
                    None => Value::Null,
                    Some(bytes) => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
                },
            ])
        }
    }

    /// A key with its MVCC suffix, as the log carries it.
    fn versioned(id: i64, ts: u64) -> Vec<u8> {
        let mut key = id.to_be_bytes().to_vec();
        key.extend_from_slice(&esker_keys::prefix::txn_key(&[], ts)[1..]);
        key
    }

    fn open(dir: &std::path::Path, options: ColumnarOptions) -> ColumnarApply {
        ColumnarApply::open(
            Arc::new(LocalFileSystem::new()) as Arc<dyn FileSystem>,
            dir,
            FakeDecoder::new(),
            options,
        )
        .unwrap()
    }

    /// Every row of a sealed run, in file order.
    fn rows_of(path: &std::path::Path) -> Vec<Vec<Value>> {
        let fs = LocalFileSystem::new();
        let reader = Reader::open(&fs, path).unwrap();
        let projection: Vec<u32> = (0..u32::try_from(reader.schema().len()).unwrap()).collect();
        let fragment = esker_columnar::Fragment::scan(
            esker_columnar::TableRef {
                tenant: 1,
                table_id: 1,
            },
            projection,
        );
        let result = esker_columnar::evaluate(&reader, &fragment).unwrap();
        match result.output {
            esker_columnar::FragmentOutput::Rows(rows) => rows,
            esker_columnar::FragmentOutput::Groups(groups) => {
                panic!("not rows: {groups:?}")
            }
        }
    }

    /// The run schema is the table's columns and then the two the apply target adds — in that
    /// order, because the decoder's slots have to keep meaning what the decoder said they mean.
    #[test]
    fn the_run_schema_is_the_table_plus_two() {
        let dir = tempfile::tempdir().unwrap();
        let apply = open(dir.path(), ColumnarOptions::default());
        let names: Vec<&str> = apply
            .schema()
            .columns()
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(names, ["id", "name", COMMIT_TS_COLUMN, DELETED_COLUMN]);
    }

    /// A table that already has one of those names is refused at open, where an operator can act
    /// on it, rather than at `Schema::new` with a message about a file format.
    #[test]
    fn a_table_colliding_with_a_reserved_column_is_refused_at_open() {
        #[derive(Debug)]
        struct Colliding(Schema);
        impl RowDecoder for Colliding {
            fn schema(&self) -> &Schema {
                &self.0
            }
            fn key_slots(&self) -> &[usize] {
                &[0]
            }
            fn decode(&self, _: &[u8], _: Option<&[u8]>) -> Result<Vec<Value>> {
                unreachable!()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let decoder = Arc::new(Colliding(
            Schema::new(vec![
                ColumnDef::new("id", ColumnType::Int8),
                ColumnDef::new(COMMIT_TS_COLUMN, ColumnType::Int8),
            ])
            .unwrap(),
        ));
        let error = ColumnarApply::open(
            Arc::new(LocalFileSystem::new()) as Arc<dyn FileSystem>,
            dir.path(),
            decoder,
            ColumnarOptions::default(),
        )
        .expect_err("a colliding column must be refused");
        assert!(error.to_string().contains(COMMIT_TS_COLUMN), "{error}");
    }

    /// **The sort is the whole design.** Runs go out `(key, commit_ts DESC)` so a read at `ts`
    /// finds the newest visible version of a key as the *first* row for that key at or below it.
    /// Applied here in deliberately scrambled order, because Raft hands entries over in apply
    /// order and that is never key order.
    #[test]
    fn a_sealed_run_is_sorted_by_key_then_newest_version_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut apply = open(dir.path(), ColumnarOptions::default());
        for (id, ts) in [(2, 10), (1, 20), (2, 30), (1, 10), (2, 20)] {
            apply.apply(&versioned(id, ts), Some(b"v")).unwrap();
        }
        let path = apply.seal().unwrap().expect("something was buffered");

        let seen: Vec<(i64, i64)> = rows_of(&path)
            .iter()
            .map(|row| match (&row[0], &row[2]) {
                (Value::Int8(id), Value::Int8(ts)) => (*id, *ts),
                other => panic!("unexpected row shape: {other:?}"),
            })
            .collect();
        assert_eq!(seen, [(1, 20), (1, 10), (2, 30), (2, 20), (2, 10)]);
    }

    /// A delete is a version, not an absence. A run that dropped tombstones would answer a read
    /// *before* the delete correctly and a read *after* it wrongly — and only under time travel,
    /// which is the worst way to be wrong.
    #[test]
    fn a_delete_is_a_version_with_its_mark_set() {
        let dir = tempfile::tempdir().unwrap();
        let mut apply = open(dir.path(), ColumnarOptions::default());
        apply.apply(&versioned(7, 10), Some(b"seven")).unwrap();
        apply.apply(&versioned(7, 20), None).unwrap();
        let path = apply.seal().unwrap().unwrap();

        let rows = rows_of(&path);
        assert_eq!(rows.len(), 2, "the tombstone was dropped: {rows:?}");
        // Newest first: the delete, then the row it deleted.
        assert_eq!(rows[0][3], Value::Bool(true), "{:?}", rows[0]);
        assert_eq!(rows[0][1], Value::Null, "a tombstone carries no value");
        assert_eq!(rows[1][3], Value::Bool(false), "{:?}", rows[1]);
        assert_eq!(rows[1][1], Value::Text("seven".into()));
    }

    /// The commit timestamp comes off the key, not from anything this process knows: a version's
    /// timestamp is a fact about the write and not about the replica applying it.
    #[test]
    fn the_commit_timestamp_comes_off_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut apply = open(dir.path(), ColumnarOptions::default());
        apply.apply(&versioned(1, 424_242), Some(b"x")).unwrap();
        let path = apply.seal().unwrap().unwrap();
        assert_eq!(rows_of(&path)[0][2], Value::Int8(424_242));
    }

    /// Sealing by row count produces the run it claims, and starts the next one from empty.
    #[test]
    fn the_memtable_seals_itself_when_it_is_full() {
        let dir = tempfile::tempdir().unwrap();
        let mut apply = open(
            dir.path(),
            ColumnarOptions {
                seal_rows: 4,
                ..ColumnarOptions::default()
            },
        );
        for id in 0..4 {
            apply.apply(&versioned(id, 10), Some(b"v")).unwrap();
        }
        assert_eq!(apply.buffered(), 0, "a full memtable did not seal");
        assert!(dir.path().join(run_name(0)).exists());
        assert_eq!(rows_of(&dir.path().join(run_name(0))).len(), 4);

        // And the next run is a new file, not an append to that one.
        apply.apply(&versioned(9, 10), Some(b"v")).unwrap();
        assert_eq!(apply.buffered(), 1);
        apply.seal().unwrap();
        assert_eq!(rows_of(&dir.path().join(run_name(1))).len(), 1);
    }

    /// Sealing nothing writes nothing. A run of zero rows is a file somebody has to sweep.
    #[test]
    fn sealing_an_empty_memtable_writes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut apply = open(dir.path(), ColumnarOptions::default());
        assert!(apply.seal().unwrap().is_none());
        assert!(!dir.path().join(run_name(0)).exists());
    }
}
