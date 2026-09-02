//! Whole files, written and read back.
//!
//! The per-encoding proptests next to each encoding prove that a *column* survives its own
//! codec. This file proves the layer above: that a generated schema and a generated batch of rows
//! survive a writer, a footer, a rename and a reader — under every stripe size, and read back
//! through every projection.
//!
//! Two things are checked that a single "does it round-trip" assertion would miss. The rows must
//! come back **in order and in the right stripes**, because a stripe index that is subtly wrong
//! produces a file that reads without error and answers the wrong rows. And a column read on its
//! own must equal the same column read as part of a projection, because that is the equivalence
//! the whole point of a columnar layout rests on.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]

use std::path::Path;

use esker_columnar::{ColumnDef, ColumnType, Reader, Schema, Value, Writer, WriterOptions};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use proptest::prelude::*;

/// Every row of a file, in order, as owned values.
fn read_all(reader: &Reader) -> Vec<Vec<Value>> {
    let width = reader.schema().len();
    let projection: Vec<usize> = (0..width).collect();
    let mut rows = Vec::new();
    for (index, stripe) in reader.stripes().iter().enumerate() {
        let columns = reader.read_stripe(index, &projection).unwrap();
        let decoded: Vec<Vec<Value>> = columns
            .iter()
            .map(|column| column.to_values().unwrap())
            .collect();
        for offset in 0..usize::try_from(stripe.rows).unwrap() {
            rows.push(
                decoded
                    .iter()
                    .map(|values| values[offset].clone())
                    .collect(),
            );
        }
    }
    rows
}

fn write_and_read(schema: &Schema, rows: &[Vec<Value>], options: WriterOptions) -> Vec<Vec<Value>> {
    let fs = MemFileSystem::new();
    fs.create_dir_all(Path::new("/c")).unwrap();
    let path = Path::new("/c/round.col");

    let mut writer = Writer::create(&fs, path, schema.clone(), options).unwrap();
    for row in rows {
        writer.append_row(row).unwrap();
    }
    let summary = writer.finish().unwrap();
    assert_eq!(summary.rows, rows.len() as u64);

    let reader = Reader::open(&fs, path).unwrap();
    assert_eq!(reader.schema(), schema);
    assert_eq!(reader.rows(), rows.len() as u64);
    assert_eq!(
        reader.stripes().iter().map(|s| s.rows).sum::<u64>(),
        rows.len() as u64,
        "the stripe index does not cover every row"
    );

    // Each column alone must equal itself inside a projection: the equivalence a columnar layout
    // exists for. `identical` rather than `==`, so a NaN column is actually compared.
    for (index, _) in reader.stripes().iter().enumerate() {
        let all: Vec<usize> = (0..schema.len()).collect();
        let together = reader.read_stripe(index, &all).unwrap();
        for column in &all {
            let alone = reader.read_column(index, *column).unwrap();
            assert!(
                alone.identical(&together[*column]),
                "column {column} differs alone"
            );
        }
    }

    read_all(&reader)
}

fn value_of(ty: ColumnType) -> impl Strategy<Value = Value> {
    let present = match ty {
        ColumnType::Int8 => any::<i64>().prop_map(Value::Int8).boxed(),
        ColumnType::Int4 => any::<i32>().prop_map(Value::Int4).boxed(),
        ColumnType::Int2 => any::<i16>().prop_map(Value::Int2).boxed(),
        ColumnType::Timestamp => (-5_000i64..5_000)
            .prop_map(|d| Value::Timestamp(757_382_400_000_000 + d * 1_000))
            .boxed(),
        ColumnType::TimestampTz => (-5_000i64..5_000)
            .prop_map(|d| Value::TimestampTz(757_382_400_000_000 + d * 1_000))
            .boxed(),
        ColumnType::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
        ColumnType::Double => prop_oneof![
            Just(Value::Double(f64::NAN)),
            Just(Value::Double(-0.0)),
            any::<f64>().prop_map(Value::Double),
        ]
        .boxed(),
        ColumnType::Text | ColumnType::Varchar => (0usize..5)
            .prop_map(|pick| Value::Text(["", "a", "beta", "gamma", "\u{1f600}"][pick].to_owned()))
            .boxed(),
        ColumnType::Bytea => prop::collection::vec(any::<u8>(), 0..8)
            .prop_map(Value::Bytea)
            .boxed(),
    };
    prop_oneof![1 => Just(Value::Null), 6 => present]
}

fn schema_and_rows() -> impl Strategy<Value = (Schema, Vec<Vec<Value>>)> {
    prop::collection::vec(prop::sample::select(ColumnType::ALL.as_slice()), 1..5).prop_flat_map(
        |types| {
            let schema = Schema::new(
                types
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| ColumnDef::new(format!("c{index}"), *ty))
                    .collect(),
            )
            .unwrap();
            let row = types
                .iter()
                .map(|ty| value_of(*ty))
                .collect::<Vec<_>>()
                .prop_map(|values| values);
            (Just(schema), prop::collection::vec(row, 0..60))
        },
    )
}

/// The two extremes of the awkward-value space, written by hand so they always run.
#[test]
fn the_awkward_values_survive_a_whole_file() {
    let schema = Schema::new(vec![
        ColumnDef::new("i", ColumnType::Int8),
        ColumnDef::new("t", ColumnType::Text),
        ColumnDef::new("b", ColumnType::Bool),
        ColumnDef::new("y", ColumnType::Bytea),
        ColumnDef::new("ts", ColumnType::TimestampTz),
        ColumnDef::new("d", ColumnType::Double),
    ])
    .unwrap();

    let rows = vec![
        vec![
            Value::Int8(i64::MIN),
            Value::Text(String::new()),
            Value::Bool(false),
            Value::Bytea(Vec::new()),
            Value::TimestampTz(i64::MIN),
            Value::Double(-0.0),
        ],
        vec![Value::Null; 6],
        vec![
            Value::Int8(i64::MAX),
            Value::Text("\u{1f600} unicode".into()),
            Value::Bool(true),
            Value::Bytea(vec![0x00, 0xff, 0x00]),
            Value::TimestampTz(i64::MAX),
            Value::Double(f64::INFINITY),
        ],
    ];

    for stripe_rows in [1usize, 2, 3, 64] {
        let options = WriterOptions {
            stripe_rows,
            ..WriterOptions::default()
        };
        let back = write_and_read(&schema, &rows, options);
        assert_eq!(back.len(), rows.len());
        for (before, after) in rows.iter().zip(&back) {
            for (index, (a, b)) in before.iter().zip(after).enumerate() {
                // NaN is checked by the proptest through `Column::identical`; here every value is
                // comparable, so ordinary equality is the stronger statement.
                assert_eq!(a, b, "column {index} at stripe size {stripe_rows}");
            }
        }
    }
}

/// A file large enough to be cut into many stripes still reads back in row order.
#[test]
fn many_stripes_read_back_in_order() {
    let schema = Schema::new(vec![
        ColumnDef::new("id", ColumnType::Int8),
        ColumnDef::new("tag", ColumnType::Text),
    ])
    .unwrap();
    let rows: Vec<Vec<Value>> = (0..5000i64)
        .map(|id| vec![Value::Int8(id), Value::Text(format!("tag-{}", id % 11))])
        .collect();

    let options = WriterOptions {
        stripe_rows: 128,
        ..WriterOptions::default()
    };
    let back = write_and_read(&schema, &rows, options);
    assert_eq!(back, rows);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A generated schema and batch survive the whole path, at every stripe size.
    #[test]
    fn generated_files_round_trip(
        (schema, rows) in schema_and_rows(),
        stripe_rows in 1usize..40,
    ) {
        let options = WriterOptions { stripe_rows, ..WriterOptions::default() };
        let back = write_and_read(&schema, &rows, options);
        prop_assert_eq!(back.len(), rows.len());
        for (before, after) in rows.iter().zip(&back) {
            for (a, b) in before.iter().zip(after) {
                let same = match (a, b) {
                    // NaN is not equal to itself, so a double is compared by bits.
                    (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
                    _ => a == b,
                };
                prop_assert!(same, "{:?} became {:?}", a, b);
            }
        }
    }

    /// The same, uncompressed: the codec must not be load-bearing for correctness.
    #[test]
    fn generated_files_round_trip_without_compression(
        (schema, rows) in schema_and_rows(),
    ) {
        let options = WriterOptions {
            stripe_rows: 7,
            compression: esker_columnar::Compression::None,
            ..WriterOptions::default()
        };
        let back = write_and_read(&schema, &rows, options);
        prop_assert_eq!(back.len(), rows.len());
    }
}
