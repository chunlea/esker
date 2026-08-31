//! What the footer claims about each column chunk, checked against the chunk itself.
//!
//! Nothing consumes these statistics yet — a predicate evaluator is ADR 0022's milestone 2 — and
//! that is exactly why they are tested this hard now. A bound that is subtly false does not
//! produce an error when something finally reads it; it produces a *missing row*, months later,
//! in a query nobody can reproduce. The cheapest moment to prove they are true is before anything
//! depends on them.
//!
//! So every assertion here recomputes the truth from the decoded values and compares, rather than
//! trusting the accumulator that wrote them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]

use std::path::Path;

use esker_columnar::{
    ColumnDef, ColumnStats, ColumnType, Reader, Schema, Value, Writer, WriterOptions,
};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;

fn schema() -> Schema {
    Schema::new(vec![
        ColumnDef::new("id", ColumnType::Int8),
        ColumnDef::new("at", ColumnType::TimestampTz),
        ColumnDef::new("kind", ColumnType::Text),
        ColumnDef::new("live", ColumnType::Bool),
        ColumnDef::new("amount", ColumnType::Double),
        ColumnDef::new("blob", ColumnType::Bytea),
    ])
    .unwrap()
}

fn row(id: i64) -> Vec<Value> {
    vec![
        Value::Int8(id),
        Value::TimestampTz(757_382_400_000_000 + id * 1_000),
        match id % 4 {
            0 => Value::Null,
            n => Value::Text(format!("kind-{n}")),
        },
        Value::Bool(id % 3 == 0),
        if id % 11 == 0 {
            Value::Double(f64::NAN)
        } else {
            Value::Double(f64::from(i32::try_from(id).unwrap()) / 4.0)
        },
        Value::Bytea(vec![
            u8::try_from(id % 251).unwrap();
            usize::try_from(id % 7).unwrap()
        ]),
    ]
}

fn write(rows: i64, stripe_rows: usize) -> (MemFileSystem, &'static Path) {
    let fs = MemFileSystem::new();
    let path = Path::new("/c/stats.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    let options = WriterOptions {
        stripe_rows,
        ..WriterOptions::default()
    };
    let mut writer = Writer::create(&fs, path, schema(), options).unwrap();
    for id in 0..rows {
        writer.append_row(&row(id)).unwrap();
    }
    writer.finish().unwrap();
    (fs, path)
}

/// The footer's statistics are exactly the statistics of the chunk they describe.
#[test]
fn every_chunk_entry_describes_its_own_chunk() {
    let (fs, path) = write(500, 64);
    let reader = Reader::open(&fs, path).unwrap();
    assert!(reader.stripes().len() >= 7);

    let mut nulls_seen = 0u64;
    for (index, stripe) in reader.stripes().iter().enumerate() {
        for (column, chunk) in stripe.columns.iter().enumerate() {
            let decoded = reader.read_column(index, column).unwrap();
            assert_eq!(
                chunk.stats,
                ColumnStats::of(&decoded),
                "stripe {index} column {column}"
            );
            assert_eq!(
                chunk.stats.null_count,
                decoded.nulls().nulls() as u64,
                "stripe {index} column {column} null count"
            );
            assert!(chunk.stats.fit(decoded.ty()));
            nulls_seen += chunk.stats.null_count;
        }
    }
    // A quarter of the `kind` column is NULL and nothing else ever is.
    assert_eq!(nulls_seen, 500 / 4);
}

/// The statistics milestone 2 will prune with: exactly one stripe can contain a given id.
#[test]
fn an_id_lands_in_exactly_one_stripes_range() {
    let (fs, path) = write(1000, 100);
    let reader = Reader::open(&fs, path).unwrap();
    assert_eq!(reader.stripes().len(), 10);

    for id in [0i64, 1, 99, 100, 500, 999] {
        let holding: Vec<usize> = reader
            .stripes()
            .iter()
            .enumerate()
            .filter(|(_, stripe)| {
                let stats = &stripe.columns[0].stats;
                let low = stats.min.as_ref().and_then(esker_columnar::Bound::as_i64);
                let high = stats.max.as_ref().and_then(esker_columnar::Bound::as_i64);
                low.is_some_and(|low| low <= id) && high.is_some_and(|high| id <= high)
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(holding, vec![usize::try_from(id).unwrap() / 100], "id {id}");
    }

    // And one outside every range: the pruner would read nothing at all.
    let outside = reader
        .stripes()
        .iter()
        .filter(|stripe| {
            let stats = &stripe.columns[0].stats;
            stats
                .min
                .as_ref()
                .and_then(esker_columnar::Bound::as_i64)
                .is_some_and(|low| low <= 5000)
                && stats
                    .max
                    .as_ref()
                    .and_then(esker_columnar::Bound::as_i64)
                    .is_some_and(|high| 5000 <= high)
        })
        .count();
    assert_eq!(outside, 0);
}

/// A column that is entirely NULL in one stripe still reports its count, and no range.
#[test]
fn a_stripe_of_nothing_but_nulls_reports_a_count_and_no_range() {
    let fs = MemFileSystem::new();
    let path = Path::new("/c/nulls.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();

    let schema = Schema::new(vec![
        ColumnDef::new("id", ColumnType::Int8),
        ColumnDef::new("note", ColumnType::Text),
    ])
    .unwrap();
    let options = WriterOptions {
        stripe_rows: 10,
        ..WriterOptions::default()
    };
    let mut writer = Writer::create(&fs, path, schema, options).unwrap();
    for id in 0..20i64 {
        // The second stripe's `note` column is entirely NULL.
        let note = if id < 10 {
            Value::Text(format!("n{id}"))
        } else {
            Value::Null
        };
        writer.append_row(&[Value::Int8(id), note]).unwrap();
    }
    writer.finish().unwrap();

    let reader = Reader::open(&fs, path).unwrap();
    let first = &reader.stripes()[0].columns[1].stats;
    assert_eq!(first.null_count, 0);
    assert!(first.min.is_some() && first.max.is_some());

    let second = &reader.stripes()[1].columns[1].stats;
    assert_eq!(second.null_count, 10);
    assert!(
        second.min.is_none() && second.max.is_none(),
        "a chunk with no values has no range"
    );
    // The id column of the same stripe is unaffected.
    assert_eq!(reader.stripes()[1].columns[0].stats.null_count, 0);
}
