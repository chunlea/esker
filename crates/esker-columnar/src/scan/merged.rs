//! Scanning a region's runs as one stream.
//!
//! See [`super::evaluate_merged`] for why this exists: MVCC visibility is a property of the
//! region and not of a file, so the runs have to be merged *before* versions are resolved rather
//! than after. This is the merge.
//!
//! Each run is already sorted `(key, commit_ts DESC)` — the apply target's seal guarantees it and
//! compaction preserves it — so a k-way merge over their fronts is globally sorted, and
//! the resolver in [`super::visible`] then works over the result unchanged.

use crate::column::Column;
use crate::error::Result;
use crate::fragment::Fragment;
use crate::reader::Reader;
use crate::scan::{ScanOptions, ScanStats, Sink, visible};
use crate::value::{ColumnType, Value, ValueRef};

/// One run's position in the merge, holding **one stripe** at a time.
struct Cursor<'a> {
    reader: &'a Reader,
    types: Vec<ColumnType>,
    stripe: usize,
    stripes: usize,
    rows: Vec<Vec<Value>>,
    at: usize,
}

impl<'a> Cursor<'a> {
    fn open(reader: &'a Reader) -> Result<Self> {
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
            stripe: 0,
            stripes,
            rows: Vec::new(),
            at: 0,
        };
        cursor.fill()?;
        Ok(cursor)
    }

    /// Decodes the next stripe with any rows in it, or leaves the cursor exhausted.
    ///
    /// Every column, not only the projected ones: the resolver reads the run's own key, timestamp
    /// and tombstone columns, and a fragment need not project a single one of them.
    fn fill(&mut self) -> Result<()> {
        self.rows.clear();
        self.at = 0;
        while self.stripe < self.stripes {
            let wanted: Vec<usize> = (0..self.types.len()).collect();
            let columns = self.reader.read_stripe(self.stripe, &wanted)?;
            self.stripe += 1;
            let rows = columns.first().map_or(0, Column::rows);
            if rows == 0 {
                continue;
            }
            let mut decoded = vec![Vec::with_capacity(self.types.len()); rows];
            for (slot, column) in columns.iter().enumerate() {
                let ty = self.types.get(slot).copied().unwrap_or(ColumnType::Int8);
                for (row, value) in column.iter().enumerate() {
                    decoded[row].push(value.to_value(ty)?);
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

/// Evaluates `fragment` over every run at once.
pub(crate) fn evaluate(
    readers: &[Reader],
    fragment: &Fragment,
    options: &ScanOptions,
) -> Result<crate::scan::FragmentResult> {
    let slots = fragment.validate(readers[0].schema())?;
    let mut stats = ScanStats::default();
    let mut resolver = visible::Resolver::default();
    let mut sink = Sink::new(fragment, &slots);

    let mut cursors = Vec::with_capacity(readers.len());
    for reader in readers {
        stats.stripes_considered += reader.stripes().len() as u64;
        stats.stripes_read += reader.stripes().len() as u64;
        cursors.push(Cursor::open(reader)?);
    }

    // Projection slots into file columns, so the sink sees the row the fragment asked for.
    let projection: Vec<usize> = fragment
        .projection
        .iter()
        .map(|column| *column as usize)
        .collect();

    loop {
        let mut best: Option<usize> = None;
        for (index, cursor) in cursors.iter().enumerate() {
            let Some(row) = cursor.peek() else { continue };
            match best {
                None => best = Some(index),
                Some(current) => {
                    if let Some(incumbent) = cursors[current].peek()
                        && order(row, incumbent, options) == std::cmp::Ordering::Less
                    {
                        best = Some(index);
                    }
                }
            }
        }
        let Some(index) = best else { break };
        let row = cursors[index].peek().cloned().unwrap_or_default();
        cursors[index].advance()?;
        stats.rows_scanned += 1;

        if let Some(visibility) = &options.visibility {
            let key: Vec<ValueRef<'_>> = visibility
                .key_columns
                .iter()
                .map(|column| {
                    row.get(*column as usize)
                        .map_or(ValueRef::Null, Value::as_ref)
                })
                .collect();
            let commit_ts = match row.get(visibility.ts_column as usize) {
                Some(Value::Int8(ts) | Value::TimestampTz(ts)) => *ts,
                _ => i64::MIN,
            };
            let deleted = matches!(
                row.get(visibility.deleted_column as usize),
                Some(Value::Bool(true))
            );
            if !resolver.visible(&key, commit_ts, deleted, visibility.ts) {
                continue;
            }
        }

        let projected: Vec<ValueRef<'_>> = projection
            .iter()
            .map(|column| row.get(*column).map_or(ValueRef::Null, Value::as_ref))
            .collect();
        if !sink.push(&projected, &mut stats)? {
            break;
        }
    }

    Ok(crate::scan::FragmentResult {
        output: sink.finish(),
        stats,
    })
}

/// The runs' own order: the visibility key ascending, then the timestamp descending.
///
/// Without visibility there is no key to merge on, so the runs are concatenated in the order they
/// were given — which is what a scan with no version resolution wants, and what the single-run
/// path does anyway.
fn order(left: &[Value], right: &[Value], options: &ScanOptions) -> std::cmp::Ordering {
    let Some(visibility) = &options.visibility else {
        return std::cmp::Ordering::Equal;
    };
    for column in &visibility.key_columns {
        let (Some(a), Some(b)) = (left.get(*column as usize), right.get(*column as usize)) else {
            continue;
        };
        let ordering = a.pg_cmp(b);
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    match (
        left.get(visibility.ts_column as usize),
        right.get(visibility.ts_column as usize),
    ) {
        // Newest first, so the first row a key offers is the one a read at or below it takes.
        (Some(a), Some(b)) => b.pg_cmp(a),
        _ => std::cmp::Ordering::Equal,
    }
}
