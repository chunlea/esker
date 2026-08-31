//! A frozen fragment, byte for byte, and every way one is refused.
//!
//! The fragment is a **format**, like everything else this project puts on a wire: a version
//! byte, hand-written bytes, a checksum and a golden (ADR 0002, and ADR 0022 decision 3 says so
//! about this message in particular — *"two nodes on different versions disagreeing about what a
//! filter means is a wrong answer, not a protocol error"*).
//!
//! The second half of this file is the more important one. Every unknown tag is checked to refuse
//! the **whole** fragment, and to refuse it as a *refusal* rather than as corruption — the bytes
//! passed their checksum, so an unknown aggregate is a build that does not implement one, not a
//! damaged message. Getting that backwards would report a rolling upgrade as data corruption, and
//! it is exactly the mistake a test is needed to prevent, because both answers look like "an
//! error" from a distance.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_base::crc32c;
use esker_columnar::fragment::{codec, expr::CompareOp};
use esker_columnar::{Aggregate, Expr, Fragment, KeyRange, Output, TableRef, Value};
use proptest::prelude::*;

/// One realistic fragment: `SELECT count(*), sum(c4), max(c0) FROM t
/// WHERE c0 > 100 AND c2 IS NOT NULL GROUP BY c2`, over the projection `[0, 2, 4]`.
fn golden() -> Fragment {
    Fragment {
        table: TableRef {
            tenant: 1,
            table_id: 7,
        },
        range: KeyRange::unbounded(),
        projection: vec![0, 2, 4],
        filter: Some(Expr::And(
            Box::new(Expr::compare(0, CompareOp::Gt, Value::Int8(100))),
            Box::new(Expr::IsNull {
                operand: Box::new(Expr::Column(1)),
                negated: true,
            }),
        )),
        output: Output::Aggregates {
            group_by: vec![1],
            aggregates: vec![Aggregate::CountStar, Aggregate::Sum(2), Aggregate::Max(0)],
        },
    }
}

/// Rebuilds a message around a mutated body, so that a tag change is tested rather than the
/// checksum that would otherwise catch it first.
fn reframe(mutate: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let encoded = codec::encode(&golden());
    let mut body = encoded[..encoded.len() - 4].to_vec();
    mutate(&mut body);
    let checksum = crc32c::checksum(&body);
    body.extend_from_slice(&checksum.to_le_bytes());
    body
}

/// The bytes are a wire format. They are written out by hand so that a layout change fails a test
/// rather than a cluster mid-upgrade.
#[test]
fn golden_fragment_bytes() {
    let bytes = codec::encode(&golden());

    #[rustfmt::skip]
    let expected: [u8; 42] = [
        0x01,                                            // format version 1
        0x01,                                            // tenant 1
        0x07,                                            // table id 7
        0x00,                                            // range.start: unbounded
        0x00,                                            // range.end: unbounded
        0x03, 0x00, 0x02, 0x04,                          // projection: columns 0, 2, 4
        0x01,                                            // a filter follows
          0x04,                                          //   And
            0x03,                                        //     Compare
              0x05,                                      //       op >
              0x01, 0x00,                                //       Column(slot 0)
              0x02, 0x01,                                //       Literal, int8
                0x64, 0x00, 0x00, 0x00,                  //         100, little-endian
                0x00, 0x00, 0x00, 0x00,
            0x07,                                        //     IsNull
              0x01,                                      //       negated: IS NOT NULL
              0x01, 0x01,                                //       Column(slot 1)
        0x01,                                            // output: aggregates
          0x01, 0x01,                                    //   group by slot 1
          0x03,                                          //   three aggregates
            0x01,                                        //     count(*)
            0x03, 0x02,                                  //     sum(slot 2)
            0x05, 0x00,                                  //     max(slot 0)
        0xa3, 0x92, 0x48, 0x9b,                          // crc32c of everything above, LE
    ];
    assert_eq!(
        bytes, expected,
        "the fragment layout changed; it is a wire format (ADR + version bump)"
    );
    assert_eq!(codec::decode(&bytes).unwrap(), golden());
}

#[test]
fn the_simplest_fragments_round_trip() {
    let cases = [
        Fragment::scan(TableRef::default(), Vec::new()),
        Fragment::scan(
            TableRef {
                tenant: u64::MAX,
                table_id: u64::MAX,
            },
            vec![0],
        ),
        Fragment {
            range: KeyRange {
                start: b"\x00\xff".to_vec(),
                end: b"z".to_vec(),
            },
            output: Output::Rows { limit: Some(0) },
            ..Fragment::scan(TableRef::default(), vec![3, 3, 3])
        },
        Fragment::aggregate(TableRef::default(), vec![1], vec![0], Vec::new()),
    ];
    for fragment in cases {
        let bytes = codec::encode(&fragment);
        assert_eq!(codec::decode(&bytes).unwrap(), fragment, "{fragment:?}");
    }
}

/// `LIMIT 0` is a real query and has to survive the encoding, which is why the limit carries a
/// presence byte rather than reserving zero for "unbounded".
#[test]
fn a_limit_of_zero_is_not_the_absence_of_a_limit() {
    let with = Fragment {
        output: Output::Rows { limit: Some(0) },
        ..Fragment::scan(TableRef::default(), vec![0])
    };
    let without = Fragment::scan(TableRef::default(), vec![0]);
    assert_ne!(codec::encode(&with), codec::encode(&without));
    assert_eq!(codec::decode(&codec::encode(&with)).unwrap(), with);
    assert_eq!(codec::decode(&codec::encode(&without)).unwrap(), without);
}

/// Every literal type, including the ones whose bits are awkward.
#[test]
fn every_literal_type_round_trips() {
    let literals = [
        Value::Null,
        Value::Int8(i64::MIN),
        Value::Int8(i64::MAX),
        Value::TimestampTz(-1),
        Value::Bool(true),
        Value::Bool(false),
        Value::Double(f64::NAN),
        Value::Double(-0.0),
        Value::Double(f64::NEG_INFINITY),
        Value::Text(String::new()),
        Value::Text("\u{1f600} unicode".into()),
        Value::Bytea(Vec::new()),
        Value::Bytea(vec![0x00, 0xff]),
    ];
    for literal in literals {
        let mut fragment = Fragment::scan(TableRef::default(), vec![0]);
        fragment.filter = Some(Expr::compare(0, CompareOp::Eq, literal.clone()));
        let decoded = codec::decode(&codec::encode(&fragment)).unwrap();
        let Some(Expr::Compare { right, .. }) = decoded.filter else {
            panic!("the filter changed shape");
        };
        let Expr::Literal(back) = *right else {
            panic!("the literal changed shape");
        };
        match (&literal, &back) {
            // NaN is not equal to itself, so a double is compared by bits.
            (Value::Double(before), Value::Double(after)) => {
                assert_eq!(before.to_bits(), after.to_bits(), "{literal:?}");
            }
            _ => assert_eq!(literal, back),
        }
    }
}

/// Damaged bytes are corruption. The checksum is what says so, and it runs first.
#[test]
fn damaged_bytes_are_corruption() {
    let bytes = codec::encode(&golden());
    assert!(codec::decode(&[]).unwrap_err().is_corruption(), "empty");
    assert!(
        codec::decode(&bytes[..4]).unwrap_err().is_corruption(),
        "too short"
    );

    for at in 0..bytes.len() {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0x01;
        let error = codec::decode(&damaged).unwrap_err();
        assert!(
            error.is_corruption(),
            "flipping byte {at} gave {error}, not corruption"
        );
    }

    // Bytes nobody read mean the two sides disagree about the layout.
    let trailing = reframe(|body| body.push(0));
    assert!(codec::decode(&trailing).unwrap_err().is_corruption());
}

/// Intact bytes this build does not understand are a **refusal**, so a newer sender causes a
/// fallback rather than an alarm. Each one refuses the whole fragment.
#[test]
fn unknown_tags_refuse_the_whole_fragment() {
    /// What to call the tag, and how to make it unknown.
    type Case = (&'static str, Box<dyn Fn(&mut Vec<u8>)>);

    let cases: [Case; 6] = [
        ("format version", Box::new(|body: &mut Vec<u8>| body[0] = 2)),
        (
            "expression node",
            Box::new(|body: &mut Vec<u8>| body[10] = 9),
        ),
        (
            "comparison operator",
            Box::new(|body: &mut Vec<u8>| body[12] = 9),
        ),
        (
            "literal of type tag",
            Box::new(|body: &mut Vec<u8>| body[16] = 9),
        ),
        ("output kind", Box::new(|body: &mut Vec<u8>| body[29] = 9)),
        (
            "aggregate kind",
            Box::new(|body: &mut Vec<u8>| body[33] = 9),
        ),
    ];
    for (what, mutate) in cases {
        let bytes = reframe(mutate);
        let error = codec::decode(&bytes).unwrap_err();
        assert!(
            error.is_refused(),
            "an unknown {what} gave {error}, which is not a refusal"
        );
        assert!(
            error.to_string().contains(what),
            "the refusal does not name the {what}: {error}"
        );
    }
}

/// A message of a few hundred bytes can describe an expression thousands deep. The decoder
/// refuses on the way down, before it has built anything and before the stack is at risk.
#[test]
fn a_deeply_nested_filter_is_refused_rather_than_overflowing_the_stack() {
    let mut deep = Expr::Column(0);
    for _ in 0..5_000 {
        deep = Expr::Not(Box::new(deep));
    }
    let mut fragment = Fragment::scan(TableRef::default(), vec![0]);
    fragment.filter = Some(deep);

    let bytes = codec::encode(&fragment);
    assert!(bytes.len() < 6_000, "5000 NOTs is a small message");
    let error = codec::decode(&bytes).unwrap_err();
    assert!(error.is_refused(), "{error}");
    assert!(error.to_string().contains("nested past"), "{error}");
}

/// Malformed values inside an otherwise intact message are corruption, not a refusal: a boolean
/// that is neither true nor false is not a feature this build lacks.
#[test]
fn malformed_values_are_corruption() {
    /// Where two fragments that differ only in one literal differ in their bytes — which is the
    /// literal's payload, found rather than counted, so the test does not encode a byte offset
    /// that a layout change would quietly invalidate.
    fn payload_at(left: &Fragment, right: &Fragment) -> usize {
        let (left, right) = (codec::encode(left), codec::encode(right));
        left.iter()
            .zip(&right)
            .position(|(a, b)| a != b)
            .expect("the two fragments encode identically")
    }

    fn with_literal(value: Value) -> Fragment {
        let mut fragment = Fragment::scan(TableRef::default(), vec![0]);
        fragment.filter = Some(Expr::compare(0, CompareOp::Eq, value));
        fragment
    }

    fn reframed(fragment: &Fragment, at: usize, byte: u8) -> Vec<u8> {
        let encoded = codec::encode(fragment);
        let mut body = encoded[..encoded.len() - 4].to_vec();
        body[at] = byte;
        let checksum = crc32c::checksum(&body);
        body.extend_from_slice(&checksum.to_le_bytes());
        body
    }

    // A text literal whose bytes are not UTF-8. 0xff is valid nowhere in UTF-8.
    let fragment = with_literal(Value::Text("ab".into()));
    let at = payload_at(&fragment, &with_literal(Value::Text("ac".into())));
    let error = codec::decode(&reframed(&fragment, at, 0xff)).unwrap_err();
    assert!(error.is_corruption(), "{error}");
    assert!(error.to_string().contains("utf-8"), "{error}");

    // A boolean literal that is neither true nor false.
    let fragment = with_literal(Value::Bool(true));
    let at = payload_at(&fragment, &with_literal(Value::Bool(false)));
    let error = codec::decode(&reframed(&fragment, at, 2)).unwrap_err();
    assert!(error.is_corruption(), "{error}");
    assert!(error.to_string().contains("boolean byte"), "{error}");
}

fn any_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<i64>().prop_map(Value::Int8),
        any::<i64>().prop_map(Value::TimestampTz),
        any::<bool>().prop_map(Value::Bool),
        any::<u64>().prop_map(|bits| Value::Double(f64::from_bits(bits))),
        ".{0,12}".prop_map(Value::Text),
        prop::collection::vec(any::<u8>(), 0..12).prop_map(Value::Bytea),
    ]
}

fn any_expr(depth: u32) -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        (0u32..8).prop_map(Expr::Column),
        any_value().prop_map(Expr::Literal),
    ];
    leaf.prop_recursive(depth, 24, 2, |inner| {
        prop_oneof![
            (1u8..=6, inner.clone(), inner.clone()).prop_map(|(op, left, right)| Expr::Compare {
                op: CompareOp::from_u8(op).unwrap(),
                left: Box::new(left),
                right: Box::new(right),
            }),
            (inner.clone(), inner.clone()).prop_map(|(l, r)| Expr::And(Box::new(l), Box::new(r))),
            (inner.clone(), inner.clone()).prop_map(|(l, r)| Expr::Or(Box::new(l), Box::new(r))),
            inner.clone().prop_map(|e| Expr::Not(Box::new(e))),
            (inner, any::<bool>()).prop_map(|(operand, negated)| Expr::IsNull {
                operand: Box::new(operand),
                negated,
            }),
        ]
    })
}

fn any_fragment() -> impl Strategy<Value = Fragment> {
    (
        any::<u64>(),
        any::<u64>(),
        prop::collection::vec(0u32..16, 0..8),
        prop::option::of(any_expr(4)),
        prop_oneof![
            prop::option::of(any::<u64>()).prop_map(|limit| Output::Rows { limit }),
            (
                prop::collection::vec(0u32..8, 0..4),
                prop::collection::vec(
                    prop_oneof![
                        Just(Aggregate::CountStar),
                        (0u32..8).prop_map(Aggregate::Count),
                        (0u32..8).prop_map(Aggregate::Sum),
                        (0u32..8).prop_map(Aggregate::Min),
                        (0u32..8).prop_map(Aggregate::Max),
                    ],
                    0..5,
                ),
            )
                .prop_map(|(group_by, aggregates)| Output::Aggregates {
                    group_by,
                    aggregates
                }),
        ],
    )
        .prop_map(|(tenant, table_id, projection, filter, output)| Fragment {
            table: TableRef { tenant, table_id },
            range: KeyRange::unbounded(),
            projection,
            filter,
            output,
        })
}

proptest! {
    /// Whatever a fragment says, its bytes say the same thing back.
    #[test]
    fn fragments_round_trip(fragment in any_fragment()) {
        let bytes = codec::encode(&fragment);
        let decoded = codec::decode(&bytes).unwrap();
        // Doubles inside literals make `==` unreliable, so compare the bytes instead: two
        // fragments that encode identically are the same fragment by definition.
        prop_assert_eq!(codec::encode(&decoded), bytes);
    }

    /// Arbitrary bytes: an error of one of the two kinds, and never a panic.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        if let Err(error) = codec::decode(&bytes) {
            prop_assert!(error.is_corruption() || error.is_refused(), "{}", error);
        }
    }
}
