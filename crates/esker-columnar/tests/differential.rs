//! The columnar evaluator against a reference that does the same thing the obvious way.
//!
//! ADR 0022 names the two engines disagreeing as the worst failure this feature can have,
//! *"because it is silent"*, and says the only defence is a differential test that has to be built
//! **before** the routing rule and not after. This is that defence, one milestone before there is
//! a routing rule to defend.
//!
//! # What is actually independent
//!
//! The reference evaluates the same fragment over the `Vec<Vec<Value>>` the file was **written
//! from** — it never opens the file. So a disagreement can come from anywhere in the path: the
//! encodings, the null mask, the stripe index, the projection, the pruner, the grouping, the
//! accumulation order. That is the point; a reference that read the same file through the same
//! reader would only ever check the last of those.
//!
//! The two share exactly one thing: [`Value::pg_cmp`], which is the *specification* — the order
//! `WHERE`, `GROUP BY`, `min` and `max` are defined in. Sharing a specification is not sharing an
//! implementation. Everything else here is written twice: the reference loops over rows, keeps its
//! groups in a `Vec` it searches linearly, and evaluates expressions with its own recursive
//! interpreter over owned values.
//!
//! # Errors are part of the answer
//!
//! Both sides are compared as `Result`, not as values. An `int8` sum that overflows must fail on
//! both, and a fragment that one side refuses the other must refuse too — a harness that only
//! compared successes would let "returns an error" and "returns 4" agree by not looking.
//!
//! # Floating point
//!
//! Compared by bits, never by `==`. `NaN != NaN` would make a comparison of two `NaN` answers
//! pass without looking, and `-0.0 == 0.0` would let a sign difference through. Both cases are in
//! the corpus on purpose.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]

use std::path::Path;
use std::time::{Duration, Instant};

use esker_base::rng::Pcg32;
use esker_columnar::fragment::expr::CompareOp;
use esker_columnar::{
    Aggregate, ColumnDef, ColumnType, Error, Expr, Fragment, FragmentOutput, Group, Output,
    Partial, Reader, Result, ScanOptions, Schema, TableRef, Value, Writer, WriterOptions,
    evaluate_with,
};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use proptest::prelude::*;

#[path = "compare.rs"]
mod compare;

use compare::same_output;

/// Seed of the generated campaign. Printed on failure so a case reproduces.
const SEED: u64 = 0xD1FF_2026;

fn budget() -> Duration {
    let seconds = std::env::var("ESKER_DIFF_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(2);
    Duration::from_secs(seconds)
}

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

/// The corpus, and every awkward value in it on purpose: NULLs in every column, `NaN`, both
/// zeroes, both infinities, the `i64` extremes, empty strings and empty byte strings, and a
/// low-cardinality label so that grouping produces groups worth comparing.
fn corpus(count: usize, seed: u64) -> Vec<Vec<Value>> {
    let mut rng = Pcg32::from_seed(seed);
    let kinds = ["", "alpha", "beta", "\u{1f600}"];
    let doubles = [
        0.0,
        -0.0,
        1.5,
        -1.5,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MIN,
        f64::MAX,
    ];
    let ints = [0i64, 1, -1, i64::MIN, i64::MAX, 42, -42];

    (0..count)
        .map(|index| {
            // Each value is computed before it is made nullable: the generator is a single
            // borrow of the RNG at a time, and the draw order is what makes the corpus
            // reproducible from its seed.
            let id = if rng.chance(0.3) {
                Value::Int8(ints[rng.below(ints.len() as u32) as usize])
            } else {
                Value::Int8(i64::try_from(index).unwrap_or(i64::MAX))
            };
            let at = Value::TimestampTz(757_382_400_000_000 + i64::from(rng.below(10_000)));
            let kind = Value::Text(kinds[rng.below(kinds.len() as u32) as usize].to_owned());
            let live = Value::Bool(rng.chance(0.5));
            let amount = Value::Double(doubles[rng.below(doubles.len() as u32) as usize]);
            let blob = {
                let len = rng.below(4) as usize;
                let mut bytes = vec![0u8; len];
                rng.fill_bytes(&mut bytes);
                Value::Bytea(bytes)
            };

            [id, at, kind, live, amount, blob]
                .into_iter()
                .map(|value| if rng.chance(0.18) { Value::Null } else { value })
                .collect()
        })
        .collect()
}

fn write(rows: &[Vec<Value>], stripe_rows: usize) -> (MemFileSystem, Reader) {
    let fs = MemFileSystem::new();
    let path = Path::new("/d/diff.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    let options = WriterOptions {
        stripe_rows,
        ..WriterOptions::default()
    };
    let mut writer = Writer::create(&fs, path, schema(), options).unwrap();
    for row in rows {
        writer.append_row(row).unwrap();
    }
    writer.finish().unwrap();
    let reader = Reader::open(&fs, path).unwrap();
    (fs, reader)
}

// ---------------------------------------------------------------------------------------------
// The reference: the same fragment, over the same rows, the obvious way.
// ---------------------------------------------------------------------------------------------

/// Evaluates an expression against one projected row, in the same three-valued logic and written
/// independently of the crate's evaluator. `Value::Null` is *unknown*.
fn eval(expr: &Expr, row: &[Value]) -> Value {
    fn truth(value: &Value) -> Option<bool> {
        match value {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }
    match expr {
        Expr::Column(slot) => row.get(*slot as usize).cloned().unwrap_or(Value::Null),
        Expr::Literal(value) => value.clone(),
        Expr::IsNull { operand, negated } => Value::Bool(eval(operand, row).is_null() != *negated),
        Expr::Not(operand) => match truth(&eval(operand, row)) {
            Some(value) => Value::Bool(!value),
            None => Value::Null,
        },
        Expr::And(left, right) => match (truth(&eval(left, row)), truth(&eval(right, row))) {
            (Some(false), _) | (_, Some(false)) => Value::Bool(false),
            (Some(true), Some(true)) => Value::Bool(true),
            _ => Value::Null,
        },
        Expr::Or(left, right) => match (truth(&eval(left, row)), truth(&eval(right, row))) {
            (Some(true), _) | (_, Some(true)) => Value::Bool(true),
            (Some(false), Some(false)) => Value::Bool(false),
            _ => Value::Null,
        },
        Expr::Compare { op, left, right } => {
            let (left, right) = (eval(left, row), eval(right, row));
            if left.is_null() || right.is_null() {
                return Value::Null;
            }
            Value::Bool(op.holds(left.pg_cmp(&right)))
        }
    }
}

/// One aggregate, folded the obvious way over a group's rows in row order.
fn fold(aggregate: Aggregate, rows: &[Vec<Value>]) -> Result<Partial> {
    let column = |row: &Vec<Value>, slot: u32| row[slot as usize].clone();
    Ok(match aggregate {
        Aggregate::CountStar => Partial::Count(rows.len() as u64),
        Aggregate::Count(slot) => Partial::Count(
            rows.iter()
                .filter(|row| !column(row, slot).is_null())
                .count() as u64,
        ),
        Aggregate::Sum(slot) => {
            let mut total: Option<Value> = None;
            for row in rows {
                let value = column(row, slot);
                if value.is_null() {
                    continue;
                }
                total = Some(match (total, value) {
                    (None, value) => value,
                    (Some(Value::Int8(a)), Value::Int8(b)) => Value::Int8(
                        a.checked_add(b)
                            .ok_or_else(|| Error::Overflow(format!("{a} + {b}")))?,
                    ),
                    (Some(Value::Double(a)), Value::Double(b)) => Value::Double(a + b),
                    (Some(a), b) => panic!("summing {a:?} with {b:?}"),
                });
            }
            Partial::Sum(total)
        }
        Aggregate::Min(slot) | Aggregate::Max(slot) => {
            let wanted = if matches!(aggregate, Aggregate::Min(_)) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
            let mut best: Option<Value> = None;
            for row in rows {
                let value = column(row, slot);
                if value.is_null() {
                    continue;
                }
                if best
                    .as_ref()
                    .is_none_or(|current| value.pg_cmp(current) == wanted)
                {
                    best = Some(value);
                }
            }
            if wanted.is_lt() {
                Partial::Min(best)
            } else {
                Partial::Max(best)
            }
        }
    })
}

/// The whole fragment, over the rows it was written from.
fn reference(rows: &[Vec<Value>], fragment: &Fragment) -> Result<FragmentOutput> {
    fragment.validate(&schema())?;

    let projected: Vec<Vec<Value>> = rows
        .iter()
        .map(|row| {
            fragment
                .projection
                .iter()
                .map(|column| row[*column as usize].clone())
                .collect()
        })
        .filter(|row: &Vec<Value>| match &fragment.filter {
            None => true,
            Some(filter) => matches!(eval(filter, row), Value::Bool(true)),
        })
        .collect();

    Ok(match &fragment.output {
        Output::Rows { limit } => {
            let take = limit.map_or(projected.len(), |limit| limit as usize);
            FragmentOutput::Rows(projected.into_iter().take(take).collect())
        }
        Output::Aggregates {
            group_by,
            aggregates,
        } => {
            // A `Vec` searched linearly, deliberately: a different data structure from the
            // evaluator's `BTreeMap`, so a key-comparison bug cannot be shared.
            let mut buckets: Vec<(Vec<Value>, Vec<Vec<Value>>)> = Vec::new();
            for row in projected {
                let key: Vec<Value> = group_by
                    .iter()
                    .map(|slot| row[*slot as usize].clone())
                    .collect();
                match buckets.iter_mut().find(|(existing, _)| {
                    existing.len() == key.len()
                        && existing.iter().zip(&key).all(|(a, b)| a.pg_cmp(b).is_eq())
                }) {
                    Some((_, rows)) => rows.push(row),
                    None => buckets.push((key, vec![row])),
                }
            }
            if group_by.is_empty() && buckets.is_empty() {
                buckets.push((Vec::new(), Vec::new()));
            }
            buckets.sort_by(|(left, _), (right, _)| {
                left.iter()
                    .zip(right)
                    .map(|(a, b)| a.pg_cmp(b))
                    .find(|ordering| !ordering.is_eq())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            let mut groups = Vec::with_capacity(buckets.len());
            for (key, rows) in buckets {
                groups.push(Group {
                    key,
                    aggregates: aggregates
                        .iter()
                        .map(|aggregate| fold(*aggregate, &rows))
                        .collect::<Result<Vec<_>>>()?,
                });
            }
            FragmentOutput::Groups(groups)
        }
    })
}

// ---------------------------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------------------------

/// Runs one fragment both ways, with pruning on and off, and requires all three to agree.
fn agree(
    reader: &Reader,
    rows: &[Vec<Value>],
    fragment: &Fragment,
) -> std::result::Result<(), String> {
    let expected = reference(rows, fragment);
    for prune in [true, false] {
        let actual =
            evaluate_with(reader, fragment, ScanOptions { prune }).map(|result| result.output);
        match (&expected, &actual) {
            (Ok(expected), Ok(actual)) => {
                if !same_output(expected, actual) {
                    return Err(format!(
                        "prune={prune} {fragment:?}\n  reference {expected:?}\n  columnar  {actual:?}"
                    ));
                }
            }
            // Both must fail, and for the same reason: an error is an answer.
            (Err(expected), Err(actual)) => {
                let kinds = (
                    expected.is_overflow(),
                    expected.is_refused(),
                    actual.is_overflow(),
                    actual.is_refused(),
                );
                if (kinds.0, kinds.1) != (kinds.2, kinds.3) {
                    return Err(format!(
                        "prune={prune} {fragment:?}\n  reference failed {expected}\n  columnar failed {actual}"
                    ));
                }
            }
            (expected, actual) => {
                return Err(format!(
                    "prune={prune} {fragment:?}\n  reference {expected:?}\n  columnar {actual:?}"
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Generating fragments
// ---------------------------------------------------------------------------------------------

fn literal(rng: &mut Pcg32, ty: ColumnType) -> Value {
    if rng.chance(0.08) {
        return Value::Null;
    }
    match ty {
        ColumnType::Int8 => {
            let pool = [0i64, 1, -1, i64::MIN, i64::MAX, 42, -42, 7];
            Value::Int8(pool[rng.below(pool.len() as u32) as usize])
        }
        ColumnType::TimestampTz => {
            Value::TimestampTz(757_382_400_000_000 + i64::from(rng.below(10_000)))
        }
        ColumnType::Text => {
            let pool = ["", "alpha", "beta", "\u{1f600}", "zzz"];
            Value::Text(pool[rng.below(pool.len() as u32) as usize].to_owned())
        }
        ColumnType::Bool => Value::Bool(rng.chance(0.5)),
        ColumnType::Double => {
            let pool = [
                0.0,
                -0.0,
                1.5,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MIN,
                f64::MAX,
            ];
            Value::Double(pool[rng.below(pool.len() as u32) as usize])
        }
        ColumnType::Bytea => {
            let len = rng.below(3) as usize;
            let mut bytes = vec![0u8; len];
            rng.fill_bytes(&mut bytes);
            Value::Bytea(bytes)
        }
    }
}

/// One leaf of a filter, always well typed against the slot it names.
fn predicate(rng: &mut Pcg32, slots: &[ColumnType]) -> Expr {
    let slot = rng.below(slots.len() as u32);
    let ty = slots[slot as usize];
    match rng.below(12) {
        0 | 1 => Expr::IsNull {
            operand: Box::new(Expr::Column(slot)),
            negated: rng.chance(0.5),
        },
        2 if ty == ColumnType::Bool => Expr::Column(slot),
        _ => {
            let op = CompareOp::from_u8(1 + rng.below(6) as u8).unwrap();
            let value = literal(rng, ty);
            if rng.chance(0.2) {
                // The literal on the left, which the pruner has to flip.
                Expr::Compare {
                    op,
                    left: Box::new(Expr::Literal(value)),
                    right: Box::new(Expr::Column(slot)),
                }
            } else {
                Expr::compare(slot, op, value)
            }
        }
    }
}

fn condition(rng: &mut Pcg32, slots: &[ColumnType], depth: u32) -> Expr {
    if depth == 0 || rng.chance(0.45) {
        return predicate(rng, slots);
    }
    match rng.below(3) {
        0 => Expr::And(
            Box::new(condition(rng, slots, depth - 1)),
            Box::new(condition(rng, slots, depth - 1)),
        ),
        1 => Expr::Or(
            Box::new(condition(rng, slots, depth - 1)),
            Box::new(condition(rng, slots, depth - 1)),
        ),
        _ => Expr::Not(Box::new(condition(rng, slots, depth - 1))),
    }
}

fn random_fragment(rng: &mut Pcg32, schema: &Schema) -> Fragment {
    let width = schema.len() as u32;
    let projection: Vec<u32> = (0..rng.below(width + 1))
        .map(|_| rng.below(width))
        .collect();
    let slots: Vec<ColumnType> = projection
        .iter()
        .map(|column| schema.columns()[*column as usize].ty)
        .collect();

    let filter = if slots.is_empty() || rng.chance(0.2) {
        None
    } else {
        Some(condition(rng, &slots, 2))
    };

    let output = if rng.chance(0.45) {
        Output::Rows {
            limit: if rng.chance(0.25) {
                Some(u64::from(rng.below(40)))
            } else {
                None
            },
        }
    } else {
        let slot_count = slots.len() as u32;
        let group_by: Vec<u32> = if slot_count == 0 {
            Vec::new()
        } else {
            (0..rng.below(3)).map(|_| rng.below(slot_count)).collect()
        };
        let aggregates: Vec<Aggregate> = (0..rng.below(4))
            .map(|_| {
                if slot_count == 0 {
                    return Aggregate::CountStar;
                }
                let slot = rng.below(slot_count);
                match rng.below(5) {
                    0 => Aggregate::CountStar,
                    1 => Aggregate::Count(slot),
                    // Deliberately unrestricted: a sum over text must be refused by both sides.
                    2 => Aggregate::Sum(slot),
                    3 => Aggregate::Min(slot),
                    _ => Aggregate::Max(slot),
                }
            })
            .collect();
        Output::Aggregates {
            group_by,
            aggregates,
        }
    };

    let mut fragment = Fragment {
        table: TableRef {
            tenant: 1,
            table_id: 1,
        },
        range: esker_columnar::KeyRange::unbounded(),
        projection,
        filter,
        output,
    };
    // Occasionally ask for something no build can do, so the refusal path is compared too.
    if rng.chance(0.03) {
        fragment.range = esker_columnar::KeyRange {
            start: b"a".to_vec(),
            end: Vec::new(),
        };
    }
    fragment
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------

/// Hand-written fragments over the awkward corpus, so the cases that matter always run.
#[test]
fn the_awkward_cases_agree() {
    let rows = corpus(300, SEED);
    let (_fs, reader) = write(&rows, 32);
    let table = TableRef {
        tenant: 1,
        table_id: 1,
    };

    let mut fragments = vec![
        Fragment::scan(table, vec![0, 1, 2, 3, 4, 5]),
        Fragment::scan(table, Vec::new()),
        Fragment::aggregate(table, Vec::new(), Vec::new(), vec![Aggregate::CountStar]),
        Fragment::aggregate(
            table,
            vec![0, 4],
            Vec::new(),
            vec![
                Aggregate::CountStar,
                Aggregate::Count(0),
                Aggregate::Sum(1),
                Aggregate::Min(1),
                Aggregate::Max(1),
            ],
        ),
        // Grouping on a column full of NULLs, on a double column holding NaN and both zeroes,
        // and on two columns at once.
        Fragment::aggregate(table, vec![2], vec![0], vec![Aggregate::CountStar]),
        Fragment::aggregate(table, vec![4], vec![0], vec![Aggregate::CountStar]),
        Fragment::aggregate(table, vec![2, 3], vec![0, 1], vec![Aggregate::CountStar]),
        // A sum that must be refused on both sides.
        Fragment::aggregate(table, vec![2], Vec::new(), vec![Aggregate::Sum(0)]),
    ];

    // Every comparison against every type, including the ones that are always unknown.
    for (slot, column) in [(0u32, 0u32), (1, 2), (2, 3), (3, 4), (4, 5)] {
        let ty = schema().columns()[column as usize].ty;
        for op in [
            CompareOp::Eq,
            CompareOp::NotEq,
            CompareOp::Lt,
            CompareOp::LtEq,
            CompareOp::Gt,
            CompareOp::GtEq,
        ] {
            for value in [
                Value::Null,
                literal(&mut Pcg32::from_seed(u64::from(op.as_u8())), ty),
            ] {
                let mut fragment = Fragment::scan(table, vec![column]);
                let _ = slot;
                fragment.filter = Some(Expr::compare(0, op, value));
                fragments.push(fragment);
            }
        }
    }

    for fragment in &fragments {
        if let Err(why) = agree(&reader, &rows, fragment) {
            panic!("{why}");
        }
    }
}

/// Stripe boundaries must not change any answer — including a `sum(double)`, which is why the
/// evaluator folds across stripes into one accumulator rather than combining per-stripe partials.
#[test]
fn the_answer_does_not_depend_on_where_stripes_fall() {
    let rows = corpus(200, SEED ^ 0x99);
    let table = TableRef {
        tenant: 1,
        table_id: 1,
    };
    let fragment = Fragment::aggregate(
        table,
        vec![4, 2],
        vec![1],
        vec![Aggregate::Sum(0), Aggregate::CountStar, Aggregate::Max(0)],
    );

    let mut answers = Vec::new();
    for stripe_rows in [1usize, 2, 7, 64, 1000] {
        let (_fs, reader) = write(&rows, stripe_rows);
        assert!(agree(&reader, &rows, &fragment).is_ok());
        answers.push(
            evaluate_with(&reader, &fragment, ScanOptions::default())
                .unwrap()
                .output,
        );
    }
    for answer in &answers[1..] {
        assert!(
            same_output(&answers[0], answer),
            "a stripe size changed the answer"
        );
    }
}

/// The campaign. Generated fragments until the budget runs out, each compared both ways.
#[test]
fn generated_fragments_agree() {
    let deadline = Instant::now() + budget();
    let mut rng = Pcg32::from_seed(SEED);
    let mut cases = 0u64;
    let mut evaluated = 0u64;
    let schema = schema();

    while Instant::now() < deadline {
        let rows = corpus(60 + rng.below(120) as usize, rng.next_u64());
        let (_fs, reader) = write(&rows, 1 + rng.below(40) as usize);
        for _ in 0..40 {
            let fragment = random_fragment(&mut rng, &schema);
            if let Err(why) = agree(&reader, &rows, &fragment) {
                panic!("seed {SEED:#x}, case {cases}: {why}");
            }
            if reference(&rows, &fragment).is_ok() {
                evaluated += 1;
            }
            cases += 1;
        }
    }

    println!("differential: {cases} fragments, {evaluated} of them evaluable");
    assert!(cases > 400, "the campaign only managed {cases} fragments");
    // A campaign of nothing but refusals would prove nothing at all.
    assert!(
        evaluated * 2 > cases,
        "only {evaluated} of {cases} fragments were actually evaluated"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// The same, driven by proptest's seeds rather than one campaign's, so a failure shrinks to a
    /// seed a developer can re-run.
    #[test]
    fn seeded_fragments_agree(seed in any::<u64>(), stripe_rows in 1usize..48) {
        let mut rng = Pcg32::from_seed(seed);
        let rows = corpus(80, seed);
        let (_fs, reader) = write(&rows, stripe_rows);
        let schema = schema();
        for _ in 0..12 {
            let fragment = random_fragment(&mut rng, &schema);
            if let Err(why) = agree(&reader, &rows, &fragment) {
                prop_assert!(false, "{}", why);
            }
        }
    }
}
