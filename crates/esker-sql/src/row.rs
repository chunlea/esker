//! How a table's tuples become bytes — re-exported from the crate that owns the format.
//!
//! The codec itself is [`esker_keys::row`]. It moved there so that a layer below `esker-sql` can
//! read a stored row without inverting the dependency: `esker-store` has to turn a committed row
//! into typed columns for a columnar learner, and `esker-sql` sits above it
//! ([ADR 0029](../../docs/adr/0030-the-row-codec-moves-down.md)).
//!
//! What stays here is what a row *means* in SQL rather than what its bytes are: the error
//! mapping, and the property tests that need PostgreSQL's ordering to state at all.

pub use esker_keys::row::{
    ROW_FORMAT_VERSION, RowError, RowSchema, decode_key_columns, decode_row, encode_row, index_key,
    index_range, row_key, table_row_range, unique_index_key_is_unique_by_value,
};

#[cfg(test)]
mod tests {
    //! The properties that need **both** halves: the key encoding, which is
    //! [`esker_keys::row`]'s, and PostgreSQL's ordering, which is this crate's.
    //!
    //! They live here rather than beside the codec because `esker-keys` cannot state them — it
    //! has no `pg_cmp` and must not grow one. That is the seam being useful rather than merely
    //! tidy: the codec's own tests ask whether bytes round-trip, and these ask whether the bytes
    //! sort the way a user's query says they should, which is a different question with a
    //! different owner.

    use proptest::prelude::*;

    use super::index_key;
    use crate::value::{
        ColumnType, Datum, MAX_MICROS, MIN_MICROS, NEG_INFINITY, POS_INFINITY, PgDatum,
    };

    /// Every value a column of `ty` can hold, NULL included.
    fn values_of(ty: ColumnType) -> BoxedStrategy<Datum> {
        use proptest::prelude::*;
        let values: BoxedStrategy<Datum> = match ty {
            ColumnType::Int8 => any::<i64>().prop_map(Datum::Int8).boxed(),
            // The whole closed range, `24:00:00` included.
            ColumnType::Time => (0i64..=86_400_000_000).prop_map(Datum::Time).boxed(),
            ColumnType::Int4 => any::<i32>().prop_map(Datum::Int4).boxed(),
            ColumnType::Int2 => any::<i16>().prop_map(Datum::Int2).boxed(),
            // Weighted towards the ties, as `Double` is: PostgreSQL has fewer floats than IEEE
            // does at either width, and it is the ties an encoding gets wrong.
            ColumnType::Real => prop_oneof![
                7 => any::<f32>().prop_map(Datum::Real),
                3 => proptest::sample::select(vec![
                    0.0f32, -0.0, f32::NAN, -f32::NAN, f32::INFINITY, f32::NEG_INFINITY,
                ])
                .prop_map(Datum::Real),
            ]
            .boxed(),
            // Weighted towards the ends and the infinities, which is where a key encoding that
            // widened a day into an `i64` the wrong way would show it.
            ColumnType::Date => prop_oneof![
                7 => any::<i32>().prop_map(Datum::Date),
                3 => proptest::sample::select(vec![
                    crate::value::date::MIN_DAY,
                    crate::value::date::MAX_DAY,
                    crate::value::date::POS_INFINITY,
                    crate::value::date::NEG_INFINITY,
                    0,
                ])
                .prop_map(Datum::Date),
            ]
            .boxed(),
            // The two spellings of one number are in here on purpose: `1.0` and `1.00` compare
            // equal and **must** encode to one key, which is the property this test exists for.
            ColumnType::Numeric => prop_oneof![
                6 => (any::<bool>(), proptest::collection::vec(0u8..=9, 1..10), -4i32..6)
                    .prop_map(|(negative, digits, scale)| Datum::Numeric(
                        esker_keys::numeric::Numeric::Finite(esker_keys::numeric::Decimal {
                            negative: negative && !digits.iter().all(|d| *d == 0),
                            digits,
                            scale,
                        })
                    )),
                4 => proptest::sample::select(vec![
                    "NaN", "Infinity", "-Infinity", "0", "0.0", "1.0", "1.00", "-1.0", "1e3",
                ])
                .prop_map(|text| Datum::from_text(ColumnType::Numeric, text).unwrap_or(Datum::Null)),
            ]
            .boxed(),
            ColumnType::Text | ColumnType::Varchar | ColumnType::Bpchar => {
                ".{0,32}".prop_map(Datum::Text).boxed()
            }
            // Documents, because that is what these columns hold — the row codec is only ever
            // handed a value the SQL layer has already validated or canonicalised.
            ColumnType::Json | ColumnType::Jsonb => proptest::sample::select(vec![
                "null",
                "true",
                "1",
                "1.00",
                "\"s\"",
                "[]",
                "[1, 2]",
                "{}",
                "{\"a\": 1}",
            ])
            .prop_map(|text| Datum::Text(text.to_owned()))
            .boxed(),
            ColumnType::Bool => any::<bool>().prop_map(Datum::Bool).boxed(),
            ColumnType::Bytea => proptest::collection::vec(any::<u8>(), 0..32)
                .prop_map(Datum::Bytea)
                .boxed(),
            ColumnType::Timestamp => (MIN_MICROS..=MAX_MICROS).prop_map(Datum::Timestamp).boxed(),
            ColumnType::TimestampTz => prop_oneof![
                9 => (MIN_MICROS..=MAX_MICROS).prop_map(Datum::TimestampTz),
                1 => proptest::sample::select(vec![NEG_INFINITY, POS_INFINITY])
                    .prop_map(Datum::TimestampTz),
            ]
            .boxed(),
            // Weighted towards the values with a special ordering: PostgreSQL has fewer floats
            // than IEEE does, and it is the ties that an encoding gets wrong.
            ColumnType::Double => prop_oneof![
                7 => any::<f64>().prop_map(Datum::Double),
                3 => proptest::sample::select(vec![
                    0.0, -0.0, f64::NAN, -f64::NAN, f64::INFINITY, f64::NEG_INFINITY,
                ])
                .prop_map(Datum::Double),
            ]
            .boxed(),
        };
        prop_oneof![9 => values, 1 => Just(Datum::Null)].boxed()
    }

    /// A schema, and as many rows of it as asked for.
    fn schema_and_rows(
        columns: std::ops::Range<usize>,
        rows: usize,
    ) -> impl Strategy<Value = (Vec<ColumnType>, Vec<Vec<Datum>>)> {
        use proptest::prelude::*;
        proptest::collection::vec(
            proptest::sample::select(ColumnType::ALL.as_slice()),
            columns,
        )
        .prop_flat_map(move |types| {
            let row: Vec<_> = types.iter().map(|ty| values_of(*ty)).collect();
            (Just(types), proptest::collection::vec(row, rows..=rows))
        })
    }

    /// Two values PostgreSQL considers equal. Enumerated rather than filtered, because the
    /// interesting pairs are rare enough that a filter rejects almost everything.
    fn equal_pair() -> impl Strategy<Value = (Datum, Datum)> {
        use proptest::prelude::*;
        prop_oneof![
            proptest::sample::select(vec![
                (Datum::Double(0.0), Datum::Double(-0.0)),
                (Datum::Double(f64::NAN), Datum::Double(-f64::NAN)),
                (
                    Datum::Double(f64::NAN),
                    // A quiet NaN carrying a payload: still one value to PostgreSQL.
                    Datum::Double(f64::from_bits(0x7ff8_0000_dead_beef)),
                ),
                (Datum::Null, Datum::Null),
            ]),
            schema_and_rows(1..2, 1).prop_map(|(_, rows)| (rows[0][0].clone(), rows[0][0].clone())),
        ]
    }

    proptest! {
        /// Byte order is value order. This is the property the whole key encoding exists for: get
        /// it wrong and a range scan silently returns the wrong rows.
        #[test]
        fn key_order_is_value_order((_types, rows) in schema_and_rows(1..2, 2)) {
            let (left, right) = (&rows[0][0], &rows[1][0]);
            let key = |value: &Datum| index_key(1, 2, 3, std::slice::from_ref(value), None).unwrap();
            proptest::prop_assert_eq!(
                key(left).cmp(&key(right)),
                left.pg_cmp(right),
                "{:?} vs {:?}", left, right
            );
        }

        /// A composite key compares field by field, which needs every field encoding to be
        /// fixed-width or prefix-free -- the property `codec` provides and this one
        /// checks is still true once a NULL marker is in front of each field.
        #[test]
        fn a_composite_key_compares_field_by_field((_types, rows) in schema_and_rows(1..4, 2)) {
            let (left, right) = (&rows[0], &rows[1]);
            let expected = left
                .iter()
                .zip(right)
                .map(|(a, b)| a.pg_cmp(b))
                .find(|ordering| !ordering.is_eq())
                .unwrap_or(std::cmp::Ordering::Equal);
            proptest::prop_assert_eq!(
                index_key(1, 2, 3, left, None).unwrap()
                    .cmp(&index_key(1, 2, 3, right, None).unwrap()),
                expected
            );
        }

        /// Values PostgreSQL calls equal must encode identically, or a unique index would admit
        /// two entries for one value. Every pair that is equal without being identical is a float:
        /// the two zeros, and any two NaNs.
        #[test]
        fn values_that_compare_equal_encode_identically((left, right) in equal_pair()) {
            proptest::prop_assert!(left.pg_cmp(&right).is_eq(), "the strategy is wrong");
            proptest::prop_assert_eq!(
                index_key(1, 2, 3, std::slice::from_ref(&left), None).unwrap(),
                index_key(1, 2, 3, std::slice::from_ref(&right), None).unwrap(),
                "{:?} and {:?} are one value to PostgreSQL", left, right
            );
        }

    }
}
