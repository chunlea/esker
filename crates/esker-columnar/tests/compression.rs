//! What the columnar layout actually costs, against the two baselines that matter.
//!
//! ADR 0022 estimates 3× against what Esker already stores, and warns that the published 3–10×
//! figures are usually quoted against an *uncompressed* baseline. This test measures both, on the
//! realistic mixed batch in `tests/corpus.rs`, so the claim in the ADR is a number somebody can
//! re-run rather than a citation:
//!
//! * **raw rows** — the row value format (`esker_sql::row`) with no compression at all. This is
//!   the number the marketing figures are against.
//! * **LZ4 rows** — the same bytes cut into 4 KiB blocks and LZ4-compressed per block, which is
//!   what an SST already does (`esker_engine::Compression`). This is the honest baseline.
//!
//! Two deliberate handicaps keep the comparison from flattering the columnar side. The row
//! baseline counts **values only** — no key, though every row in an SST carries one — and the
//! corpus contains a column of random bytes, which nothing can compress and which therefore puts
//! a hard floor under every ratio here. A synthetic corpus without one would produce a much
//! larger number and mean much less.
//!
//! The assertions are floors well below what is measured, so this is a regression guard rather
//! than a benchmark: an encoding that silently stopped being chosen would trip it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::cast_precision_loss)]

use std::path::Path;

use esker_base::varint;
use esker_columnar::{Compression, Value, Writer, WriterOptions};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;

#[path = "corpus.rs"]
mod corpus;

/// Rows measured. Large enough for the per-file overhead to disappear into the noise.
const ROWS: usize = 50_000;

/// The block an SST compresses one at a time (`esker_engine::options::block_size`).
const BLOCK: usize = 4096;

/// One row in the row value format: `version ++ columns:varint ++ null bitmap ++ values`.
///
/// Written out here rather than linked, because `esker-sql` is not a dependency of this crate and
/// must not become one (ADR 0022). It mirrors `esker_sql::row::encode_row` at
/// `ROW_FORMAT_VERSION` 2; if that format changes, this baseline is measuring the wrong thing and
/// the number should be recomputed.
fn encode_row(values: &[Value]) -> Vec<u8> {
    let mut out = vec![2u8];
    varint::put_u64(values.len() as u64, &mut out);
    let bitmap_at = out.len();
    out.resize(bitmap_at + values.len().div_ceil(8), 0);

    for (index, value) in values.iter().enumerate() {
        match value {
            Value::Null => out[bitmap_at + index / 8] |= 1 << (index % 8),
            Value::Int8(v) | Value::TimestampTz(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Int4(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Double(v) => out.extend_from_slice(&v.to_le_bytes()),
            Value::Bool(v) => out.push(u8::from(*v)),
            Value::Text(v) => {
                varint::put_u64(v.len() as u64, &mut out);
                out.extend_from_slice(v.as_bytes());
            }
            Value::Bytea(v) => {
                varint::put_u64(v.len() as u64, &mut out);
                out.extend_from_slice(v);
            }
        }
    }
    out
}

/// LZ4 per 4 KiB block, skipping the codec where it saves less than an eighth — the engine's rule.
fn lz4_by_block(bytes: &[u8]) -> usize {
    bytes
        .chunks(BLOCK)
        .map(|block| {
            let compressed = lz4_flex::block::compress(block).len();
            if compressed < block.len() - block.len() / 8 {
                compressed
            } else {
                block.len()
            }
        })
        .sum()
}

fn columnar_bytes(compression: Compression) -> usize {
    let fs = MemFileSystem::new();
    let path = Path::new("/m/measure.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();

    let options = WriterOptions {
        compression,
        ..WriterOptions::default()
    };
    let mut writer = Writer::create(&fs, path, corpus::schema(), options).unwrap();
    for row in corpus::rows(ROWS) {
        writer.append_row(&row).unwrap();
    }
    writer.finish().unwrap();
    fs.contents(path).unwrap().len()
}

#[test]
fn columnar_beats_both_row_baselines() {
    let rows = corpus::rows(ROWS);
    let row_bytes: Vec<u8> = rows.iter().flat_map(|row| encode_row(row)).collect();
    let raw = row_bytes.len();
    let lz4_rows = lz4_by_block(&row_bytes);

    let plain = columnar_bytes(Compression::None);
    let lz4 = columnar_bytes(Compression::Lz4);

    let against_raw = raw as f64 / lz4 as f64;
    let against_lz4_rows = lz4_rows as f64 / lz4 as f64;
    let encodings_alone = raw as f64 / plain as f64;

    println!(
        "\n{ROWS} rows of the mixed ledger corpus (values only, no keys)\n\
         \x20 raw rows          {raw:>10} bytes\n\
         \x20 lz4 rows (4 KiB)  {lz4_rows:>10} bytes   {:.2}x\n\
         \x20 columnar, plain   {plain:>10} bytes   {encodings_alone:.2}x vs raw \
         (the encodings alone)\n\
         \x20 columnar, lz4     {lz4:>10} bytes   {against_raw:.2}x vs raw, \
         {against_lz4_rows:.2}x vs lz4 rows\n",
        raw as f64 / lz4_rows as f64,
    );

    // Floors, well below what is measured: a regression guard, not a benchmark.
    assert!(
        against_raw > 2.0,
        "columnar is only {against_raw:.2}x smaller than raw rows"
    );
    assert!(
        against_lz4_rows > 1.5,
        "columnar is only {against_lz4_rows:.2}x smaller than lz4-compressed rows"
    );
    assert!(
        encodings_alone > 1.8,
        "the encodings alone give only {encodings_alone:.2}x, so one has stopped being chosen"
    );
}
