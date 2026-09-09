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
    index_range, index_value_range, row_key, table_row_range, unique_index_key_is_unique_by_value,
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
    #[allow(
        clippy::too_many_lines,
        reason = "one strategy per column type; the list is the vocabulary"
    )]
    fn values_of(ty: ColumnType) -> BoxedStrategy<Datum> {
        use proptest::prelude::*;
        let values: BoxedStrategy<Datum> = match ty {
            // Any oid with any name: the two are independent, which is what the round trip has to
            // preserve — an oid that is no type carries its own digits, and two spellings of one
            // type carry different names for one value.
            ColumnType::RegType => (any::<u32>(), "[a-z ]{0,12}")
                .prop_map(|(oid, name)| Datum::RegType {
                    oid,
                    name: name.into(),
                })
                .boxed(),
            // The same for a `regclass`, whose name may carry a schema: what resolved it decides
            // whether it is qualified, so the codec carries the string it was handed.
            // Space-separated numbers, which is all a vector holds.
            ColumnType::Int2Vector | ColumnType::OidVector => {
                proptest::sample::select(vec![
                    Datum::Text(String::new()),
                    Datum::Text("1".to_owned()),
                    Datum::Text("1 2 3".to_owned()),
                ])
                .boxed()
            }
            ColumnType::RegClass => (any::<i64>(), "[a-z. ]{0,12}")
                .prop_map(|(oid, name)| Datum::RegClass {
                    oid,
                    name: name.into(),
                })
                .boxed(),
            ColumnType::Int8 => any::<i64>().prop_map(Datum::Int8).boxed(),
            // Every `f64`, `NaN` included: the round trip is over the bits.
            ColumnType::Point => (any::<f64>(), any::<f64>())
                .prop_map(|(x, y)| Datum::Point { x, y })
                .boxed(),
            // An array of the element type's own values, NULL elements and a lower bound that is
            // sometimes not one — the parts of the value an index key has to order by.
            ColumnType::Int8Array
            | ColumnType::Int4Array
        | ColumnType::Int2Array
            | ColumnType::NumericArray
            | ColumnType::TextArray
            | ColumnType::HstoreArray
            | ColumnType::TsVectorArray
            | ColumnType::TsQueryArray
            | ColumnType::TsRangeArray | ColumnType::TstzRangeArray | ColumnType::Int4RangeArray | ColumnType::DateRangeArray | ColumnType::NumRangeArray | ColumnType::Int8RangeArray | ColumnType::PointArray
            | ColumnType::BoolArray
            | ColumnType::ByteaArray
            | ColumnType::BpcharArray
            | ColumnType::VarcharArray
            | ColumnType::DateArray
            | ColumnType::TimeArray
            | ColumnType::TimestampArray
            | ColumnType::TimestampTzArray
            | ColumnType::IntervalArray
            | ColumnType::RealArray
            | ColumnType::DoubleArray
            | ColumnType::UuidArray
            | ColumnType::JsonArray
            | ColumnType::JsonbArray
            | ColumnType::OidArray
            | ColumnType::RegTypeArray
            | ColumnType::CitextArray
            | ColumnType::MoneyArray
            | ColumnType::InetArray
            | ColumnType::CidrArray
            | ColumnType::MacAddrArray
            | ColumnType::BitArray
            | ColumnType::VarBitArray
            | ColumnType::XmlArray
            | ColumnType::LtreeArray
            => {
                let element = esker_keys::array::ArrayValue::element_of(ty)
                    .unwrap_or(ColumnType::Text);
                (
                    proptest::collection::vec(
                        proptest::option::of(values_of(element).prop_filter(
                            "a NULL element is the `None`, not a `Datum::Null`",
                            |value| !matches!(value, Datum::Null),
                        )),
                        0..5,
                    ),
                    -2i32..3,
                )
                    .prop_map(move |(values, lower)| {
                        Datum::Array(esker_keys::array::ArrayValue::one_dimensional(
                            element, lower, values,
                        ))
                    })
                    .boxed()
            }
            // The whole closed range, `24:00:00` included.
            ColumnType::Time => (0i64..=86_400_000_000).prop_map(Datum::Time).boxed(),
            ColumnType::Uuid => any::<[u8; 16]>().prop_map(Datum::Uuid).boxed(),
            ColumnType::Oid => any::<u32>().prop_map(Datum::Oid).boxed(),
            ColumnType::Interval => (-100_000i32..100_000, -100_000i32..100_000, any::<i32>())
                .prop_map(|(months, days, micros)| Datum::Interval {
                    months,
                    days,
                    micros: i64::from(micros),
                })
                .boxed(),
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
            // A `name` is text here: the 63-byte truncation belongs to the cast, where the
            // character boundary is known, and the codec round-trips whatever it is handed.
            ColumnType::Text | ColumnType::Varchar | ColumnType::Name | ColumnType::Bpchar => {
                ".{0,32}".prop_map(Datum::Text).boxed()
            }
            // Documents, because that is what these columns hold — the row codec is only ever
            // handed a value the SQL layer has already validated or canonicalised.
            // An hstore is a `Datum::Text` holding the canonical form; the ordering property
            // these tests state is the text's, which is exactly the claim.
            ColumnType::Hstore => ".*".prop_map(Datum::Hstore).boxed(),
            ColumnType::TsVector => ".*".prop_map(Datum::TsVector).boxed(),
            ColumnType::TsQuery => ".*".prop_map(Datum::TsQuery).boxed(),
            // A range's stored form is its canonical text; `empty` is the one value every subtype
            // has, which is enough to state the ordering property these tests are for.
            // Cents, the whole `i64` of them: both ends of the range are values a client can
            // write, and the ordering property is exactly the integer's.
            ColumnType::Money => proptest::num::i64::ANY.prop_map(Datum::Money).boxed(),
            // The ordering property is the family, then the address, then the prefix — so the
            // strategy has to reach both families and both types.
            ColumnType::Inet | ColumnType::Cidr => (
                prop::sample::select(vec![
                    esker_keys::value::INET_V4,
                    esker_keys::value::INET_V6,
                ]),
                prop::array::uniform16(any::<u8>()),
                // The flag is the column's: a value whose `cidr` disagrees with its column does
                // not `fit` it, which is the round-trip property working rather than failing.
                Just(ty == ColumnType::Cidr),
            )
                .prop_map(|(family, addr, cidr)| Datum::Inet {
                    family,
                    bits: if family == esker_keys::value::INET_V6 { 128 } else { 32 },
                    cidr,
                    addr: if family == esker_keys::value::INET_V6 {
                        addr
                    } else {
                        let mut narrow = [0u8; 16];
                        narrow[..4].copy_from_slice(&addr[..4]);
                        narrow
                    },
                })
                .boxed(),
            ColumnType::MacAddr => prop::array::uniform6(any::<u8>())
                .prop_map(Datum::MacAddr)
                .boxed(),
            // The flag is the column's, for the reason `inet`'s is.
            ColumnType::Lseg
            | ColumnType::Box
            | ColumnType::Path
            | ColumnType::Polygon
            | ColumnType::Circle
            | ColumnType::Line => Just(Datum::Geometry {
                kind: Box::new(ty),
                text: "canonical".to_owned(),
            })
            .boxed(),
            ColumnType::Bit | ColumnType::VarBit => "[01]*"
                .prop_map(move |bits: String| Datum::Bit {
                    varying: ty == ColumnType::VarBit,
                    bits,
                })
                .boxed(),
            ColumnType::TsRange | ColumnType::TstzRange | ColumnType::Int4Range | ColumnType::DateRange | ColumnType::NumRange | ColumnType::Int8Range
            | ColumnType::FloatRange | ColumnType::VarcharRange => {
                Just(Datum::Range {
                    subtype: Box::new(crate::value::range_subtype(ty)),
                    text: "empty".to_owned(),
                })
                .boxed()
            }
            // Folded, because a citext's *key* is its folded value: the ordering property these
            // tests state is the folded text's, which is exactly the claim.
            ColumnType::Citext => ".*"
                .prop_map(|text: String| Datum::Citext(text.to_lowercase()))
                .boxed(),
            // Well-formed content, kept **unchanged**: there is no canonical form to normalise
            // towards, so what a round trip has to reproduce is the characters as written.
            // The key rewrite's own fixtures: a dot beside a `-` is what makes the order differ
            // from the bytes', and the ordering property here is the one that would catch it.
            ColumnType::Ltree => proptest::sample::select(vec![
                "a.b", "a-b", "ab", "a", "a.a", "b", "A", "a.B", "1.2.3", "",
            ])
            .prop_map(|text| Datum::Ltree(text.to_owned()))
            .boxed(),
            ColumnType::LQuery => proptest::sample::select(vec!["a.*", "*", "a|b", "!a", "a@"])
                .prop_map(|text| Datum::Text(text.to_owned()))
                .boxed(),
            ColumnType::Xml => proptest::sample::select(vec![
                "<foo>bar</foo>",
                "  <a/>  ",
                "<a></a>",
                "plain text",
                "",
            ])
            .prop_map(|text| Datum::Text(text.to_owned()))
            .boxed(),
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
        // `ALL` and the user-range representations beside it: the second list is not in the
        // first for the reason `ColumnType::USER_RANGES` gives, and a codec property that
        // skipped it would leave two stored types unchecked.
        let every: Vec<ColumnType> = ColumnType::ALL
            .into_iter()
            .chain(ColumnType::USER_RANGES)
            .collect();
        proptest::collection::vec(proptest::sample::select(every), columns).prop_flat_map(
            move |types| {
                let row: Vec<_> = types.iter().map(|ty| values_of(*ty)).collect();
                (Just(types), proptest::collection::vec(row, rows..=rows))
            },
        )
    }

    /// [`schema_and_rows`] over the types that can actually be **index columns**.
    ///
    /// **The two key properties were asking about types that have no key.** `encode_key_column`
    /// writes nothing for a `point`, a geometry or a `regtype` — they are not index keys and
    /// `esker_keys::row::is_index_key` says so — so two distinct values of one produce the same
    /// bytes, and `a_composite_key_compares_field_by_field` compares `Equal` against a `pg_cmp`
    /// that said `Less`. It passed only because drawing two of those into one column was rare;
    /// adding `regtype`, whose two values differ *only* in the parts the key does not write, made
    /// it certain. The row codec still covers every type — that is `schema_and_rows` — and this is
    /// the key half asking only what a key can answer.
    fn key_schema_and_rows(
        columns: std::ops::Range<usize>,
        rows: usize,
    ) -> impl Strategy<Value = (Vec<ColumnType>, Vec<Vec<Datum>>)> {
        use proptest::prelude::*;
        let every: Vec<ColumnType> = ColumnType::ALL
            .into_iter()
            .chain(ColumnType::USER_RANGES)
            .filter(|ty| esker_keys::row::is_index_key(*ty))
            .collect();
        proptest::collection::vec(proptest::sample::select(every), columns).prop_flat_map(
            move |types| {
                let row: Vec<_> = types.iter().map(|ty| values_of(*ty)).collect();
                (Just(types), proptest::collection::vec(row, rows..=rows))
            },
        )
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
        fn key_order_is_value_order((_types, rows) in key_schema_and_rows(1..2, 2)) {
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
        fn a_composite_key_compares_field_by_field((_types, rows) in key_schema_and_rows(1..4, 2)) {
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
