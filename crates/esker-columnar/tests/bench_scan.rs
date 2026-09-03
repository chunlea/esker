//! Scan throughput, and the honest comparison against reading the same rows row-wise.
//!
//! Ignored by default: `CLAUDE.md` says benchmarks are not gates, and a measurement in the test
//! suite would be a flaky one. Run it deliberately:
//!
//! ```text
//! cargo test --release -p esker-columnar --test bench_scan -- --ignored --nocapture
//! ```
//!
//! # What is compared, and what would make it dishonest
//!
//! The row baseline is **not** an in-memory `Vec<Vec<Value>>`. That would be a comparison between
//! a format and no format at all, and columnar would lose it for a reason that has nothing to do
//! with layout. It is the same values encoded in the row **value format**
//! (`esker_sql::row::encode_row`, mirrored here because `esker-sql` is not a dependency), decoded
//! per row and evaluated — so both sides pay for a decode, and the difference is the layout.
//!
//! What the row side still does not pay is I/O, key encoding, or MVCC. It is therefore a *fast*
//! baseline, which is the direction that keeps the numbers honest: columnar has to beat a row
//! store that is better than the real one.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
)]

use std::path::Path;
use std::time::Instant;

use esker_base::varint;
use esker_columnar::fragment::expr::CompareOp;
use esker_columnar::{
    Aggregate, ColumnType, Expr, Fragment, Reader, TableRef, Value, Writer, WriterOptions, evaluate,
};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;

#[path = "corpus.rs"]
mod corpus;

/// Rows measured. Large enough that per-file overhead disappears, small enough to hold twice.
const ROWS: usize = 200_000;

fn table() -> TableRef {
    TableRef {
        tenant: 1,
        table_id: 1,
    }
}

/// One row in the row value format: `version ++ columns:varint ++ null bitmap ++ values`.
fn encode_row(values: &[Value]) -> Vec<u8> {
    let mut out = vec![2u8];
    varint::put_u64(values.len() as u64, &mut out);
    let bitmap_at = out.len();
    out.resize(bitmap_at + values.len().div_ceil(8), 0);
    for (index, value) in values.iter().enumerate() {
        match value {
            Value::Null => out[bitmap_at + index / 8] |= 1 << (index % 8),
            Value::Int8(v) | Value::TimestampTz(v) | Value::Timestamp(v) | Value::Time(v) => {
                out.extend_from_slice(&v.to_le_bytes());
            }
            Value::Int4(v) | Value::Date(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Oid(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Int2(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Real(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Bool(v) => out.push(u8::from(*v)),
            Value::Text(v) => {
                varint::put_u64(v.len() as u64, &mut out);
                out.extend_from_slice(v.as_bytes());
            }
            // The bench corpus has no `numeric` column, and this helper mirrors the row codec
            // byte for byte; a guessed encoding here would measure the wrong thing silently.
            // Adding a numeric column means writing the kind-byte/zigzag-scale/digits pair.
            Value::Numeric(_) => unimplemented!("the bench corpus has no numeric column"),
            // A uuid is sixteen fixed bytes with no length before them; the corpus has no
            // uuid column, and a guessed encoding here would measure the wrong thing.
            Value::Uuid(_) | Value::Interval(_) => {
                unimplemented!("the bench corpus has no uuid or interval column")
            }
            Value::Bytea(v) => {
                varint::put_u64(v.len() as u64, &mut out);
                out.extend_from_slice(v);
            }
        }
    }
    out
}

/// Reads one back, given the columns the table has. The mirror of the above.
#[allow(clippy::too_many_lines, reason = "one arm per column type")]
fn decode_row(types: &[ColumnType], bytes: &[u8]) -> Vec<Value> {
    let mut at = 1;
    let (count, used) = varint::get_u64(&bytes[at..]).unwrap();
    at += used;
    let count = count as usize;
    let bitmap = &bytes[at..at + count.div_ceil(8)];
    at += count.div_ceil(8);

    let mut values = Vec::with_capacity(count);
    for (index, ty) in types.iter().enumerate().take(count) {
        if bitmap[index / 8] & (1 << (index % 8)) != 0 {
            values.push(Value::Null);
            continue;
        }
        let fixed = |at: usize| {
            let mut eight = [0u8; 8];
            eight.copy_from_slice(&bytes[at..at + 8]);
            eight
        };
        values.push(match ty {
            ColumnType::Int8 => {
                let value = Value::Int8(i64::from_le_bytes(fixed(at)));
                at += 8;
                value
            }
            ColumnType::Uuid | ColumnType::Interval => {
                unimplemented!("the bench corpus has no uuid or interval column")
            }
            ColumnType::Time => {
                let value = Value::Time(i64::from_le_bytes(fixed(at)));
                at += 8;
                value
            }
            ColumnType::Int4 => {
                let mut four = [0u8; 4];
                four.copy_from_slice(&bytes[at..at + 4]);
                at += 4;
                Value::Int4(i32::from_le_bytes(four))
            }
            ColumnType::Date => {
                let mut four = [0u8; 4];
                four.copy_from_slice(&bytes[at..at + 4]);
                at += 4;
                Value::Date(i32::from_le_bytes(four))
            }
            ColumnType::Oid => {
                let mut four = [0u8; 4];
                four.copy_from_slice(&bytes[at..at + 4]);
                at += 4;
                Value::Oid(u32::from_le_bytes(four))
            }
            ColumnType::Int2 => {
                let mut two = [0u8; 2];
                two.copy_from_slice(&bytes[at..at + 2]);
                at += 2;
                Value::Int2(i16::from_le_bytes(two))
            }
            ColumnType::Real => {
                let mut four = [0u8; 4];
                four.copy_from_slice(&bytes[at..at + 4]);
                at += 4;
                Value::Real(f32::from_le_bytes(four))
            }
            ColumnType::Timestamp => {
                let value = Value::Timestamp(i64::from_le_bytes(fixed(at)));
                at += 8;
                value
            }
            ColumnType::TimestampTz => {
                let value = Value::TimestampTz(i64::from_le_bytes(fixed(at)));
                at += 8;
                value
            }
            ColumnType::Double => {
                let value = Value::Double(f64::from_le_bytes(fixed(at)));
                at += 8;
                value
            }
            ColumnType::Bool => {
                let value = Value::Bool(bytes[at] == 1);
                at += 1;
                value
            }
            ColumnType::Text
            | ColumnType::Varchar
            | ColumnType::Bpchar
            | ColumnType::Json
            | ColumnType::Jsonb
            | ColumnType::Bytea => {
                let (len, used) = varint::get_u64(&bytes[at..]).unwrap();
                at += used;
                let raw = bytes[at..at + len as usize].to_vec();
                at += len as usize;
                if matches!(
                    ty,
                    ColumnType::Text
                        | ColumnType::Varchar
                        | ColumnType::Bpchar
                        | ColumnType::Json
                        | ColumnType::Jsonb
                ) {
                    Value::Text(String::from_utf8(raw).unwrap())
                } else {
                    Value::Bytea(raw)
                }
            }
            ColumnType::Numeric => unimplemented!("the bench corpus has no numeric column"),
        });
    }
    values
}

fn million_rows_per_second(rows: usize, seconds: f64) -> f64 {
    rows as f64 / seconds / 1e6
}

#[test]
#[ignore = "a measurement, not a gate; see the module docs for the command"]
fn scan_throughput() {
    let rows = corpus::rows(ROWS);
    let schema = corpus::schema();
    let types: Vec<ColumnType> = schema.columns().iter().map(|column| column.ty).collect();

    let fs = MemFileSystem::new();
    let path = Path::new("/b/bench.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    let started = Instant::now();
    let mut writer = Writer::create(&fs, path, schema.clone(), WriterOptions::default()).unwrap();
    for row in &rows {
        writer.append_row(row).unwrap();
    }
    let summary = writer.finish().unwrap();
    let write_seconds = started.elapsed().as_secs_f64();

    let encoded: Vec<Vec<u8>> = rows.iter().map(|row| encode_row(row)).collect();
    let row_bytes: usize = encoded.iter().map(Vec::len).sum();
    let reader = Reader::open(&fs, path).unwrap();

    println!(
        "\n{ROWS} rows, {} stripes, {} bytes columnar, {row_bytes} bytes row-format",
        summary.stripes, summary.bytes
    );
    println!(
        "  write            {:.2} M rows/s",
        million_rows_per_second(ROWS, write_seconds)
    );

    // --- full scan: every column decoded ------------------------------------------------------
    let wide = Fragment::aggregate(
        table(),
        (0..6).collect(),
        Vec::new(),
        (0..6).map(Aggregate::Count).collect(),
    );
    let started = Instant::now();
    let result = evaluate(&reader, &wide).unwrap();
    let wide_seconds = started.elapsed().as_secs_f64();
    assert_eq!(result.stats.rows_scanned as usize, ROWS);
    println!(
        "  scan, 6 columns  {:.2} M rows/s   {} chunks decoded",
        million_rows_per_second(ROWS, wide_seconds),
        result.stats.chunks_decoded
    );

    // --- projected scan: one column of six ----------------------------------------------------
    let narrow = Fragment::aggregate(table(), vec![0], Vec::new(), vec![Aggregate::Sum(0)]);
    let started = Instant::now();
    let result = evaluate(&reader, &narrow).unwrap();
    let narrow_seconds = started.elapsed().as_secs_f64();
    println!(
        "  scan, 1 column   {:.2} M rows/s   {} chunks decoded   {:.1}x the six-column scan",
        million_rows_per_second(ROWS, narrow_seconds),
        result.stats.chunks_decoded,
        wide_seconds / narrow_seconds
    );

    // --- count(*): no chunk at all ------------------------------------------------------------
    let counted = Fragment::aggregate(table(), Vec::new(), Vec::new(), vec![Aggregate::CountStar]);
    let started = Instant::now();
    let result = evaluate(&reader, &counted).unwrap();
    let count_seconds = started.elapsed().as_secs_f64();
    assert_eq!(result.stats.chunks_decoded, 0);
    println!(
        "  count(*)         {:.2} M rows/s   0 chunks decoded (rows are walked, nothing is read)",
        million_rows_per_second(ROWS, count_seconds)
    );

    // --- the row-wise baseline: same predicate, same aggregate, row format --------------------
    let started = Instant::now();
    let mut total: i64 = 0;
    for bytes in &encoded {
        let row = decode_row(&types, bytes);
        if let Value::Int8(value) = row[0] {
            total = total.wrapping_add(value);
        }
    }
    let row_seconds = started.elapsed().as_secs_f64();
    println!(
        "  row-wise sum     {:.2} M rows/s   (decodes all six columns per row)   total {total}",
        million_rows_per_second(ROWS, row_seconds)
    );
    println!(
        "  columnar sum is {:.1}x the row-wise scan for the same answer",
        row_seconds / narrow_seconds
    );
}

#[test]
#[ignore = "a measurement, not a gate; see the module docs for the command"]
fn pruning_selectivity() {
    let rows = corpus::rows(ROWS);
    let fs = MemFileSystem::new();
    let path = Path::new("/b/prune.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    let mut writer = Writer::create(&fs, path, corpus::schema(), WriterOptions::default()).unwrap();
    for row in &rows {
        writer.append_row(row).unwrap();
    }
    writer.finish().unwrap();
    let reader = Reader::open(&fs, path).unwrap();
    let stripes = reader.stripes().len();

    println!("\npruning, {ROWS} rows in {stripes} stripes (id ascending)");
    println!("  predicate keeps   stripes read   chunks   rows scanned   elapsed");
    for (label, low) in [
        ("everything", 0i64),
        ("~half", ROWS as i64 / 2),
        ("~a tenth", ROWS as i64 * 9 / 10),
        ("~a hundredth", ROWS as i64 * 99 / 100),
        ("nothing", ROWS as i64 * 2),
    ] {
        let mut fragment =
            Fragment::aggregate(table(), vec![0], Vec::new(), vec![Aggregate::CountStar]);
        fragment.filter = Some(Expr::compare(0, CompareOp::GtEq, Value::Int8(low)));

        let started = Instant::now();
        let result = evaluate(&reader, &fragment).unwrap();
        let elapsed = started.elapsed();
        // A rate over rows the scan never looked at would flatter the pruner by dividing by
        // work it did not do. Elapsed time and the work itself are the honest columns.
        println!(
            "  {label:>15}   {:>12}   {:>6}   {:>12}   {:>7.2?}",
            result.stats.stripes_read,
            result.stats.chunks_decoded,
            result.stats.rows_scanned,
            elapsed
        );
    }
}
