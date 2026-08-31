//! Merging runs, one stripe at a time.
//!
//! Sealing the memtable produces a run per flush, so a busy region accumulates them and every
//! fragment pays for the count: a read has to consider each live run, and "the newest visible
//! version of a key" is only cheap *within* a run. Compaction merges several into one and hands
//! the region back a bounded number of them.
//!
//! # Why this streams by stripe
//!
//! Every input is already sorted `(key, commit_ts DESC)` — that is what
//! [`ColumnarApply::seal`](super::ColumnarApply::seal) guarantees — so merging them is a k-way
//! merge and needs only the *front* of each input in memory, never the whole of it. The cursors
//! below hold **one stripe** each, so a merge of `k` runs costs `k` stripes rather than `k` runs:
//! tens of megabytes instead of hundreds, and flat in the size of the region.
//!
//! Reading each input whole would have been three lines shorter and would have made compaction the
//! largest memory consumer in the process, which is the wrong shape for the one job that runs in
//! the background while everything else is trying to work.
//!
//! # What it does not do
//!
//! **It drops nothing.** No version is discarded, not even one shadowed by a newer version of the
//! same key, because a read at an older `ts` still needs it — this replica answers time-travel
//! reads by design (ADR 0022 Decision 4). Discarding shadowed versions is garbage collection
//! against a safepoint, which is a different job with a different input, and doing it here on the
//! grounds that it looks like the same loop is how a compaction quietly becomes destructive.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_columnar::{ColumnType, Reader, Schema, Value, ValueRef, Writer, WriterOptions};
use esker_engine::fs::FileSystem;

use super::runs::RunSet;
use crate::error::{Result, StoreError};

/// How many live runs a region is allowed before a merge is worth doing.
pub const DEFAULT_RUN_LIMIT: usize = 8;

/// One input's position in the merge: a reader, and the stripe it currently holds.
struct Cursor {
    reader: Reader,
    types: Vec<ColumnType>,
    columns: usize,
    stripe: usize,
    stripes: usize,
    /// The decoded rows of the current stripe, and how far into them we are.
    rows: Vec<Vec<Value>>,
    at: usize,
}

impl Cursor {
    fn open(fs: &dyn FileSystem, path: &Path) -> Result<Self> {
        let reader = Reader::open(fs, path).map_err(|error| columnar(&error))?;
        let columns = reader.schema().len();
        let types: Vec<ColumnType> = reader
            .schema()
            .columns()
            .iter()
            .map(|column| column.ty)
            .collect();
        let stripes = reader.stripes().len();
        let mut cursor = Self {
            reader,
            types,
            columns,
            stripe: 0,
            stripes,
            rows: Vec::new(),
            at: 0,
        };
        cursor.fill()?;
        Ok(cursor)
    }

    /// Decodes the next stripe that has any rows, or leaves the cursor exhausted.
    fn fill(&mut self) -> Result<()> {
        self.rows.clear();
        self.at = 0;
        while self.stripe < self.stripes {
            let wanted: Vec<usize> = (0..self.columns).collect();
            let columns = self
                .reader
                .read_stripe(self.stripe, &wanted)
                .map_err(|error| columnar(&error))?;
            self.stripe += 1;
            let rows = columns.first().map_or(0, esker_columnar::Column::rows);
            if rows == 0 {
                continue;
            }
            let mut decoded = vec![Vec::with_capacity(self.columns); rows];
            for (slot, column) in columns.iter().enumerate() {
                let ty = self.types.get(slot).copied().unwrap_or(ColumnType::Int8);
                for (row, value) in column.iter().enumerate() {
                    decoded[row].push(owned(&value, ty));
                }
            }
            self.rows = decoded;
            return Ok(());
        }
        Ok(())
    }

    fn peek(&self) -> Option<&Vec<Value>> {
        self.rows.get(self.at)
    }

    fn advance(&mut self) -> Result<()> {
        self.at += 1;
        if self.at >= self.rows.len() {
            self.fill()?;
        }
        Ok(())
    }
}

/// Merges `inputs` into one run and swaps it into the live set.
///
/// The swap is [`RunSet::replace`], whose manifest write is the instant the merge takes effect.
/// A crash before it leaves the inputs live and the output an orphan the sweep collects; a crash
/// after it leaves the output live and the inputs orphans. There is no instant at which both are
/// live, which is what stops a reader counting every row twice.
pub fn merge(
    fs: &Arc<dyn FileSystem>,
    runs: &mut RunSet,
    schema: &Schema,
    key_slots: &[usize],
    ts_slot: usize,
    inputs: &[u64],
    options: &WriterOptions,
) -> Result<Option<PathBuf>> {
    if inputs.len() < 2 {
        return Ok(None);
    }
    let mut cursors = Vec::with_capacity(inputs.len());
    for number in inputs {
        cursors.push(Cursor::open(fs.as_ref(), &runs.path_of(*number))?);
    }

    let output = runs.reserve();
    let path = runs.path_of(output);
    let result = (|| -> Result<()> {
        let mut writer = Writer::create(fs.as_ref(), &path, schema.clone(), *options)
            .map_err(|error| columnar(&error))?;
        loop {
            // The smallest front row across every cursor, in the runs' own order.
            let mut best: Option<usize> = None;
            for (index, cursor) in cursors.iter().enumerate() {
                let Some(row) = cursor.peek() else { continue };
                match best {
                    None => best = Some(index),
                    Some(current) => {
                        let incumbent = cursors[current].peek().unwrap_or(row);
                        if order(row, incumbent, key_slots, ts_slot) == std::cmp::Ordering::Less {
                            best = Some(index);
                        }
                    }
                }
            }
            let Some(index) = best else { break };
            let row = cursors[index].peek().cloned().unwrap_or_default();
            writer.append_row(&row).map_err(|error| columnar(&error))?;
            cursors[index].advance()?;
        }
        writer.finish().map_err(|error| columnar(&error))?;
        Ok(())
    })();

    if let Err(error) = result {
        runs.abandon(output);
        let _ = fs.delete(&path);
        return Err(error);
    }

    runs.replace(inputs, output)?;
    tracing::info!(
        inputs = inputs.len(),
        output,
        path = %path.display(),
        "merged columnar runs"
    );
    Ok(Some(path))
}

/// The runs' order: key slots ascending, then `commit_ts` **descending**.
///
/// The same comparison [`ColumnarApply::seal`](super::ColumnarApply::seal) sorts by, and it has to
/// be: a merge that ordered its output differently from its inputs would produce a file the
/// one-pass visibility scan cannot read correctly, and it would look fine until a read at an old
/// `ts` returned the wrong version.
fn order(
    left: &[Value],
    right: &[Value],
    key_slots: &[usize],
    ts_slot: usize,
) -> std::cmp::Ordering {
    for slot in key_slots {
        let Some((a, b)) = left.get(*slot).zip(right.get(*slot)) else {
            continue;
        };
        let ordering = a.pg_cmp(b);
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    match left.get(ts_slot).zip(right.get(ts_slot)) {
        Some((a, b)) => b.pg_cmp(a),
        None => std::cmp::Ordering::Equal,
    }
}

/// Owns a borrowed value, using the column's type to say which of the two it is.
///
/// [`ValueRef`] deliberately collapses the pairs that share a representation — `Int8` and
/// `TimestampTz` are the same 64 bits, `Text` and `Bytea` the same bytes — so the schema is what
/// distinguishes them. Getting this wrong would round-trip a timestamp column into an integer one
/// and change the file's type on the way through a merge.
fn owned(value: &ValueRef<'_>, ty: ColumnType) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Bool(flag) => Value::Bool(*flag),
        ValueRef::Double(double) => Value::Double(*double),
        ValueRef::Int(int) => match ty {
            ColumnType::TimestampTz => Value::TimestampTz(*int),
            _ => Value::Int8(*int),
        },
        ValueRef::Bytes(bytes) => match ty {
            ColumnType::Bytea => Value::Bytea((*bytes).to_vec()),
            _ => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
        },
    }
}

fn columnar(error: &esker_columnar::Error) -> StoreError {
    StoreError::Bootstrap(format!("merging columnar runs: {error}"))
}
