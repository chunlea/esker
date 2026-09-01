//! What the columnar learner costs and what it buys.
//!
//! Not a gate. `CLAUDE.md` keeps benchmarks runnable and recorded so regressions are visible, and
//! out of the test gate so nobody tunes before correctness is proven. `#[ignore]`d, run
//! deliberately:
//!
//! ```text
//! cargo test --release -p esker-store --test bench_columnar \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! **`--test-threads=1` is not optional.** The three cases below each push a few hundred thousand
//! rows through an engine; run concurrently they measure each other's contention rather than
//! their own subject, and the numbers come out both slower and meaningless.
//!
//! Three questions, and the third is one this lane owes as a *measurement* rather than an
//! assertion: resolving MVCC visibility across several runs materialises rows where a single run
//! borrows them, and the size of that is not something to guess at.

// A benchmark counts rows and divides by seconds. Every cast below is a row count or an id, all
// far inside every type involved, and writing `try_from` around each would say nothing a reader
// needs. Allowed here and deliberately not in the crate that ships.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::sync::Arc;
use std::time::Instant;

use esker_columnar::scan::visible::Visibility;
use esker_columnar::{
    ColumnDef, ColumnType, Fragment, FragmentOutput, Reader, ScanOptions, Schema, TableRef, Value,
    evaluate_merged,
};
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_engine::{Db, Options, ReadOptions, WalSyncMode, cf};
use esker_store::columnar::{ColumnarApply, ColumnarOptions, RowDecoder};
use esker_store::error::Result;

/// Deliberately modest, and the reason is a finding rather than a convenience.
///
/// The row side writes through `Db::put`, and a sample of this benchmark at 200,000 rows put
/// **2075 of 2114 stacks inside `DbInner::commit_group` itself** — not the WAL flush (31), not the
/// memtable (1). Every single-row put forms its own commit group, so the row-side ingestion number
/// here is dominated by group-commit coordination rather than by the write, at roughly three
/// milliseconds a row with one writer and nothing to contend with.
///
/// That is worth knowing and is not this lane's to fix (`esker-engine`'s write path is consumed
/// here, not owned). It does mean the ingestion comparison below should be read as *this is what
/// a one-entry-at-a-time apply costs on each side today*, which is the honest question for a Raft
/// apply loop, rather than as a claim about either engine's peak throughput.
const ROWS: usize = 20_000;

/// `id int8, name text`, the same two columns on both sides.
#[derive(Debug)]
struct Decoder {
    schema: Schema,
}

impl Decoder {
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

impl RowDecoder for Decoder {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn decode(&self, key: &[u8], value: Option<&[u8]>) -> Result<Vec<Value>> {
        let mut id = [0u8; 8];
        id.copy_from_slice(&key[..8]);
        Ok(match value {
            None => vec![Value::Null, Value::Null],
            Some(bytes) => vec![
                Value::Int8(i64::from_be_bytes(id)),
                Value::Text(String::from_utf8_lossy(bytes).into_owned()),
            ],
        })
    }
}

fn versioned_key(id: i64, ts: u64) -> Vec<u8> {
    let mut key = id.to_be_bytes().to_vec();
    key.extend_from_slice(&esker_keys::prefix::txn_key(&[], ts)[1..]);
    key
}

fn value_of(id: i64) -> Vec<u8> {
    format!("row-{id:012}").into_bytes()
}

fn rate(what: &str, rows: usize, elapsed: std::time::Duration) {
    let per_second = rows as f64 / elapsed.as_secs_f64();
    println!(
        "{what:<44} {:>10.2} M rows/s   {elapsed:>10.2?}",
        per_second / 1e6
    );
}

/// **Ingestion: the same stream applied columnar and row-wise.**
///
/// The columnar side decodes, buffers and seals; the row side writes through the engine with the
/// WAL in `Never` mode, so neither is measuring an `fsync`. What this compares is the *apply*, and
/// nothing else — a store hosting a region as a columnar learner does exactly one of these two
/// things per committed entry.
#[test]
#[ignore = "a benchmark; see this file's header"]
fn ingestion_columnar_against_row() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new());
    let mut apply = ColumnarApply::open(
        Arc::clone(&fs),
        dir.path().join("columnar"),
        Decoder::new(),
        ColumnarOptions::default(),
    )
    .unwrap();

    let keys: Vec<Vec<u8>> = (0..ROWS as i64)
        .map(|id| versioned_key(id, 10 + id as u64))
        .collect();
    let values: Vec<Vec<u8>> = (0..ROWS as i64).map(value_of).collect();

    let started = Instant::now();
    for (key, value) in keys.iter().zip(&values) {
        apply.apply(key, Some(value)).unwrap();
    }
    apply.seal().unwrap();
    rate("columnar apply + seal", ROWS, started.elapsed());

    let row_dir = dir.path().join("row");
    std::fs::create_dir_all(&row_dir).unwrap();
    let db = Db::open_with(
        &row_dir,
        Options {
            create_if_missing: true,
            // The same bargain the columnar side gets: no fsync in the measurement.
            wal_sync_mode: WalSyncMode::Never,
            ..Options::default()
        },
        Arc::new(LocalFileSystem::new()),
        &cf::BUILTIN,
    )
    .unwrap();
    let started = Instant::now();
    for (key, value) in keys.iter().zip(&values) {
        db.put(cf::DEFAULT, key, value).unwrap();
    }
    rate("row apply", ROWS, started.elapsed());
}

/// **The read: a fragment on the learner against a row scan on a voter.**
///
/// Both answer the same question over the same rows — every visible version at a timestamp past
/// all of them — and both walk every row, so this is the honest comparison rather than the
/// flattering one. The columnar side projects **one** column of two; a scan that reads every
/// column is a different measurement, and `docs/bench/columnar-m2.md` already records that it goes
/// the *other* way.
#[test]
#[ignore = "a benchmark; see this file's header"]
fn fragment_scan_against_row_scan() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new());
    let mut apply = ColumnarApply::open(
        Arc::clone(&fs),
        dir.path().join("columnar"),
        Decoder::new(),
        ColumnarOptions::default(),
    )
    .unwrap();
    let row_dir = dir.path().join("row");
    std::fs::create_dir_all(&row_dir).unwrap();
    let db = Db::open_with(
        &row_dir,
        Options {
            create_if_missing: true,
            wal_sync_mode: WalSyncMode::Never,
            ..Options::default()
        },
        Arc::new(LocalFileSystem::new()),
        &cf::BUILTIN,
    )
    .unwrap();

    for id in 0..ROWS as i64 {
        let key = versioned_key(id, 10 + id as u64);
        let value = value_of(id);
        apply.apply(&key, Some(&value)).unwrap();
        db.put(cf::DEFAULT, &key, &value).unwrap();
    }
    apply.seal().unwrap();

    // Warm the page cache and the table cache, so the timed run below measures the scan rather
    // than the first open of every file.
    let _ = scan_columnar(&apply, i64::MAX);

    let started = Instant::now();
    let rows = scan_columnar(&apply, i64::MAX);
    rate(
        "learner fragment scan, 1 of 2 columns",
        rows,
        started.elapsed(),
    );

    let started = Instant::now();
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default()).unwrap();
    iter.seek_to_first();
    let mut seen = 0usize;
    while iter.valid() {
        // The same work the fragment does: look at the row, keep one column's worth.
        std::hint::black_box(iter.value().len());
        seen += 1;
        iter.next();
    }
    rate("voter row scan, whole row", seen, started.elapsed());
    assert_eq!(seen, rows, "the two sides did not see the same rows");
}

/// **The merged path's cost, measured rather than asserted.**
///
/// One run borrows values straight out of the decoded chunk; several must materialise each row to
/// merge them. This is that difference over the same rows, and the reason a region wants
/// compaction to keep its run count down rather than merely tidy.
#[test]
#[ignore = "a benchmark; see this file's header"]
fn the_merged_path_against_a_single_run() {
    for runs in [1usize, 2, 4, 8] {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new());
        let mut apply = ColumnarApply::open(
            Arc::clone(&fs),
            dir.path(),
            Decoder::new(),
            ColumnarOptions {
                // Sealed so the same rows land in exactly `runs` files.
                seal_rows: ROWS / runs,
                ..ColumnarOptions::default()
            },
        )
        .unwrap();
        for id in 0..ROWS as i64 {
            apply
                .apply(&versioned_key(id, 10 + id as u64), Some(&value_of(id)))
                .unwrap();
        }
        apply.seal().unwrap();
        assert_eq!(
            apply.runs().live().len(),
            runs,
            "the seal did not split evenly"
        );

        let _ = scan_columnar(&apply, i64::MAX);
        let started = Instant::now();
        let rows = scan_columnar(&apply, i64::MAX);
        rate(
            &format!("visible scan across {runs} run(s)"),
            rows,
            started.elapsed(),
        );
    }
}

/// Every visible row at `at`, projecting one column.
fn scan_columnar(apply: &ColumnarApply, at: i64) -> usize {
    let fs = LocalFileSystem::new();
    let runs = apply.runs();
    let readers: Vec<Reader> = runs
        .live()
        .iter()
        .map(|number| Reader::open(&fs, &runs.path_of(*number)).unwrap())
        .collect();
    let (key_slot, ts_slot, deleted_slot) = apply.visibility_slots();
    let fragment = Fragment::scan(
        TableRef {
            tenant: 1,
            table_id: 1,
        },
        vec![1],
    );
    let result = evaluate_merged(
        &readers,
        &fragment,
        &ScanOptions {
            prune: true,
            widening: None,
            visibility: Some(Visibility {
                key_columns: vec![key_slot],
                ts_column: ts_slot,
                deleted_column: deleted_slot,
                ts: at,
            }),
        },
    )
    .unwrap();
    match result.output {
        FragmentOutput::Rows(rows) => rows.len(),
        FragmentOutput::Groups(groups) => panic!("not rows: {groups:?}"),
    }
}
