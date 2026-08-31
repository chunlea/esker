//! Contract C3 for order: an index scan must return rows in the order PostgreSQL would.
//!
//! The property tests in `crate::row` check that byte order equals `Datum::pg_cmp`. That is only
//! worth having if `pg_cmp` is right, and this is what makes it so: `tests/corpus/pg19_order.txt`
//! is a real PostgreSQL 19beta1's own `ORDER BY` output, and the keys we build for those values
//! have to sort the same way as plain bytes.
//!
//! The one place the two disagree on purpose is `text`, and the fixture says so: it was captured
//! with `COLLATE "C"`, because a byte-ordered key space is what a project that compiles no C can
//! honestly offer (`crate::row` has the argument). Capturing it under the database's `en_US.utf8`
//! default and calling the difference a rounding error is the thing this file exists to prevent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::row::index_key;
use esker_sql::value::{ColumnType, Datum};
use esker_sql::value::{PgDatum};

#[test]
fn encoded_keys_sort_the_way_postgresql_sorts_the_values() {
    let mut types_seen = Vec::new();

    for (line_number, name, values) in fixture() {
        let column_type = column_type(&name);
        types_seen.push(column_type);

        let keyed: Vec<(Datum, Vec<u8>)> = values
            .iter()
            .map(|text| {
                let datum = Datum::from_text(column_type, text).unwrap_or_else(|error| {
                    panic!("line {line_number}: {name} {text:?} did not read back: {error}")
                });
                let key = index_key(1, 2, 3, std::slice::from_ref(&datum), None).unwrap();
                (datum, key)
            })
            .collect();

        for pair in keyed.windows(2) {
            let ((left, left_key), (right, right_key)) = (&pair[0], &pair[1]);
            // PostgreSQL returned these in ascending order, and values it considers equal may
            // come back in either order -- so the requirement is non-decreasing, plus the
            // stronger property that a tie encodes to one key.
            assert!(
                left_key <= right_key,
                "line {line_number}: {name} sorts {left:?} before {right:?}, our keys do not"
            );
            if left.pg_cmp(right).is_eq() {
                assert_eq!(
                    left_key, right_key,
                    "line {line_number}: {left:?} and {right:?} are one value and need one key"
                );
            }
        }
    }

    for ty in ColumnType::ALL {
        assert!(
            types_seen.contains(&ty),
            "{ty:?} has no ordering fixture; a type whose key order is unchecked is a type whose \
             range scans are unchecked"
        );
    }
}

fn column_type(name: &str) -> ColumnType {
    match name {
        "int8" => ColumnType::Int8,
        "text" => ColumnType::Text,
        "bool" => ColumnType::Bool,
        "bytea" => ColumnType::Bytea,
        "timestamptz" => ColumnType::TimestampTz,
        "float8" => ColumnType::Double,
        other => panic!("the fixture names a type this crate does not have: {other}"),
    }
}

fn fixture() -> impl Iterator<Item = (usize, String, Vec<String>)> {
    include_str!("corpus/pg19_order.txt")
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#') && !line.trim().is_empty())
        .map(|(index, line)| {
            let mut fields = line.split('\t');
            let name = fields.next().expect("a type name").to_owned();
            (index + 1, name, fields.map(str::to_owned).collect())
        })
}
