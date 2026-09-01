//! What a scan reads, and what it is allowed to skip.
//!
//! Three claims that no correctness test can make, because a scan that reads everything still
//! returns the right answers:
//!
//! * **Only the named columns are decoded.** Counted, not asserted about the code — a refactor
//!   that quietly read every column would pass every other test in this crate.
//! * **Pruning actually happens.** A pruner that silently stopped pruning is a performance bug
//!   nothing else would notice.
//! * **Pruning changes nothing but the work.** The same fragments, run with the switch off, must
//!   give the same answers. This is the half that can lose a row.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]

use std::path::Path;

use esker_columnar::fragment::expr::CompareOp;
use esker_columnar::{
    Aggregate, ColumnDef, ColumnType, Expr, Fragment, FragmentOutput, Output, Partial, Reader,
    ScanOptions, Schema, TableRef, Value, Writer, WriterOptions, evaluate, evaluate_with,
};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;

const STRIPE_ROWS: usize = 50;
const ROWS: i64 = 500;

fn schema() -> Schema {
    Schema::new(vec![
        ColumnDef::new("id", ColumnType::Int8),
        ColumnDef::new("kind", ColumnType::Text),
        ColumnDef::new("amount", ColumnType::Double),
        ColumnDef::new("live", ColumnType::Bool),
        ColumnDef::new("note", ColumnType::Text),
        ColumnDef::new("blob", ColumnType::Bytea),
    ])
    .unwrap()
}

/// Ascending ids, so a range predicate lands in exactly one stripe; a `note` column that is
/// always NULL, so the null rules have something to bite on.
fn row(id: i64) -> Vec<Value> {
    vec![
        Value::Int8(id),
        Value::Text(format!("kind-{}", id % 4)),
        Value::Double(f64::from(id as i32) / 2.0),
        Value::Bool(id % 3 == 0),
        Value::Null,
        Value::Bytea(vec![u8::try_from(id % 251).unwrap()]),
    ]
}

fn open() -> (MemFileSystem, Reader) {
    let fs = MemFileSystem::new();
    let path = Path::new("/s/scan.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    let options = WriterOptions {
        stripe_rows: STRIPE_ROWS,
        ..WriterOptions::default()
    };
    let mut writer = Writer::create(&fs, path, schema(), options).unwrap();
    for id in 0..ROWS {
        writer.append_row(&row(id)).unwrap();
    }
    writer.finish().unwrap();
    let reader = Reader::open(&fs, path).unwrap();
    (fs, reader)
}

fn table() -> TableRef {
    TableRef {
        tenant: 1,
        table_id: 1,
    }
}

/// The extreme case of the property this crate exists for: an answer with no I/O past the footer.
#[test]
fn count_star_decodes_nothing_at_all() {
    let (_fs, reader) = open();
    reader.reset_counters();

    let fragment = Fragment::aggregate(table(), Vec::new(), Vec::new(), vec![Aggregate::CountStar]);
    let result = evaluate(&reader, &fragment).unwrap();

    assert_eq!(
        result.output,
        FragmentOutput::Groups(vec![esker_columnar::Group {
            key: Vec::new(),
            aggregates: vec![Partial::Count(ROWS as u64)],
        }])
    );
    assert_eq!(reader.counters().columns_decoded, 0, "a chunk was decoded");
    assert_eq!(reader.counters().bytes_read, 0);
    assert_eq!(result.stats.chunks_decoded, 0);
    assert_eq!(result.stats.stripes_read, result.stats.stripes_considered);
}

/// One aggregate over one column of six decodes one chunk per stripe, not six.
#[test]
fn only_the_columns_something_names_are_decoded() {
    let (_fs, reader) = open();
    reader.reset_counters();

    // Project four columns but only ever read one of them.
    let fragment = Fragment::aggregate(
        table(),
        vec![0, 1, 2, 4],
        Vec::new(),
        vec![Aggregate::Sum(2)],
    );
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(
        reader.counters().columns_decoded,
        result.stats.stripes_read,
        "one chunk per stripe, not one per projected column"
    );

    // A filter on another slot adds exactly that column.
    reader.reset_counters();
    let mut fragment = fragment.clone();
    fragment.filter = Some(Expr::compare(0, CompareOp::Gt, Value::Int8(-1)));
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(
        reader.counters().columns_decoded,
        2 * result.stats.stripes_read
    );

    // A row output needs every projected column.
    reader.reset_counters();
    let fragment = Fragment::scan(table(), vec![0, 1, 2, 4]);
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(
        reader.counters().columns_decoded,
        4 * result.stats.stripes_read
    );
}

/// A predicate on the ascending id column reaches one stripe out of ten.
#[test]
fn pruning_skips_the_stripes_it_can() {
    let (_fs, reader) = open();
    let stripes = reader.stripes().len() as u64;
    assert_eq!(stripes, (ROWS as usize).div_ceil(STRIPE_ROWS) as u64);

    let mut fragment = Fragment::scan(table(), vec![0]);
    fragment.filter = Some(Expr::compare(0, CompareOp::Eq, Value::Int8(120)));
    let result = evaluate(&reader, &fragment).unwrap();

    assert_eq!(result.stats.stripes_considered, stripes);
    assert_eq!(result.stats.stripes_read, 1, "one stripe holds id 120");
    assert_eq!(result.stats.stripes_pruned(), stripes - 1);
    assert_eq!(
        result.output,
        FragmentOutput::Rows(vec![vec![Value::Int8(120)]])
    );

    // A range across two stripes reads two.
    let mut fragment = Fragment::scan(table(), vec![0]);
    fragment.filter = Some(Expr::And(
        Box::new(Expr::compare(0, CompareOp::GtEq, Value::Int8(45))),
        Box::new(Expr::compare(0, CompareOp::Lt, Value::Int8(55))),
    ));
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(result.stats.stripes_read, 2);
    assert_eq!(result.stats.rows_matched, 10);

    // And a predicate nothing can satisfy reads nothing at all.
    reader.reset_counters();
    let mut fragment = Fragment::scan(table(), vec![0]);
    fragment.filter = Some(Expr::compare(0, CompareOp::Gt, Value::Int8(ROWS)));
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(result.stats.stripes_read, 0);
    assert_eq!(reader.counters().columns_decoded, 0);
}

/// The null rules, which need no bounds at all.
#[test]
fn pruning_uses_the_null_count() {
    let (_fs, reader) = open();
    let stripes = reader.stripes().len() as u64;

    // `note` is NULL in every row: IS NOT NULL can never match.
    let mut fragment = Fragment::scan(table(), vec![4]);
    fragment.filter = Some(Expr::IsNull {
        operand: Box::new(Expr::Column(0)),
        negated: true,
    });
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(result.stats.stripes_read, 0);

    // And IS NULL matches every row of every stripe.
    fragment.filter = Some(Expr::IsNull {
        operand: Box::new(Expr::Column(0)),
        negated: false,
    });
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(result.stats.stripes_read, stripes);
    assert_eq!(result.stats.rows_matched, ROWS as u64);

    // `id` is never NULL, so IS NULL on it prunes everything.
    let mut fragment = Fragment::scan(table(), vec![0]);
    fragment.filter = Some(Expr::IsNull {
        operand: Box::new(Expr::Column(0)),
        negated: false,
    });
    assert_eq!(evaluate(&reader, &fragment).unwrap().stats.stripes_read, 0);

    // A comparison against a column that is entirely NULL matches nothing either.
    let mut fragment = Fragment::scan(table(), vec![4]);
    fragment.filter = Some(Expr::compare(0, CompareOp::Eq, Value::Text("x".into())));
    assert_eq!(evaluate(&reader, &fragment).unwrap().stats.stripes_read, 0);
}

/// `x = NULL` is unknown for every row, so no stripe can match it.
#[test]
fn a_comparison_with_null_prunes_everything() {
    let (_fs, reader) = open();
    let mut fragment = Fragment::scan(table(), vec![0]);
    fragment.filter = Some(Expr::compare(0, CompareOp::Eq, Value::Null));
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(result.stats.stripes_read, 0);
    assert_eq!(result.output, FragmentOutput::Rows(Vec::new()));
}

/// A comparison under an `OR` constrains nothing, so nothing may be pruned for it.
#[test]
fn a_disjunction_prunes_nothing() {
    let (_fs, reader) = open();
    let stripes = reader.stripes().len() as u64;

    let mut fragment = Fragment::scan(table(), vec![0]);
    fragment.filter = Some(Expr::Or(
        Box::new(Expr::compare(0, CompareOp::Eq, Value::Int8(1))),
        Box::new(Expr::compare(0, CompareOp::Eq, Value::Int8(499))),
    ));
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(result.stats.stripes_read, stripes, "an OR was pruned on");
    assert_eq!(result.stats.rows_matched, 2);
}

/// Pruning may only ever remove work. The same answer, both ways, is the whole guarantee.
#[test]
fn pruning_changes_nothing_but_the_work() {
    let (_fs, reader) = open();
    let filters = [
        Expr::compare(0, CompareOp::Eq, Value::Int8(300)),
        Expr::compare(0, CompareOp::Lt, Value::Int8(75)),
        Expr::compare(0, CompareOp::GtEq, Value::Int8(475)),
        Expr::compare(0, CompareOp::NotEq, Value::Int8(3)),
        Expr::compare(1, CompareOp::Eq, Value::Text("kind-2".into())),
        Expr::compare(2, CompareOp::Gt, Value::Double(200.0)),
        Expr::compare(3, CompareOp::Eq, Value::Bool(true)),
        Expr::And(
            Box::new(Expr::compare(0, CompareOp::GtEq, Value::Int8(100))),
            Box::new(Expr::compare(0, CompareOp::Lt, Value::Int8(140))),
        ),
        Expr::Not(Box::new(Expr::compare(0, CompareOp::Lt, Value::Int8(490)))),
    ];

    for filter in filters {
        let mut fragment = Fragment::scan(table(), vec![0, 1, 2, 3]);
        fragment.filter = Some(filter.clone());
        let pruned = evaluate(&reader, &fragment).unwrap();
        let whole = evaluate_with(
            &reader,
            &fragment,
            &ScanOptions {
                prune: false,
                widening: None,
                visibility: None,
            },
        )
        .unwrap();
        assert_eq!(pruned.output, whole.output, "pruning changed {filter:?}");
        assert!(
            pruned.stats.stripes_read <= whole.stats.stripes_read,
            "pruning read more stripes"
        );
        assert_eq!(pruned.stats.rows_matched, whole.stats.rows_matched);
    }
}

/// A limit is a bound on work: it stops the scan, not just the output.
#[test]
fn a_limit_stops_the_scan() {
    let (_fs, reader) = open();
    let fragment = Fragment {
        output: Output::Rows { limit: Some(3) },
        ..Fragment::scan(table(), vec![0])
    };
    let result = evaluate(&reader, &fragment).unwrap();

    let FragmentOutput::Rows(rows) = &result.output else {
        panic!("not rows");
    };
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], vec![Value::Int8(0)]);
    assert_eq!(result.stats.stripes_read, 1, "the scan read past its limit");

    // A limit of zero reads a stripe and keeps nothing, which is what `LIMIT 0` means.
    let fragment = Fragment {
        output: Output::Rows { limit: Some(0) },
        ..Fragment::scan(table(), vec![0])
    };
    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(result.output, FragmentOutput::Rows(Vec::new()));
}

/// A grouping with no key is one group, and it exists even when nothing matched.
#[test]
fn an_empty_result_still_has_its_one_group() {
    let (_fs, reader) = open();
    let mut fragment = Fragment::aggregate(
        table(),
        vec![0],
        Vec::new(),
        vec![Aggregate::CountStar, Aggregate::Sum(0), Aggregate::Min(0)],
    );
    fragment.filter = Some(Expr::compare(0, CompareOp::Gt, Value::Int8(100_000)));

    let result = evaluate(&reader, &fragment).unwrap();
    assert_eq!(
        result.output,
        FragmentOutput::Groups(vec![esker_columnar::Group {
            key: Vec::new(),
            aggregates: vec![Partial::Count(0), Partial::Sum(None), Partial::Min(None)],
        }]),
        "count(*) over nothing is 0 and sum over nothing is NULL"
    );

    // A grouping *with* a key produces no groups at all when nothing matched.
    let mut fragment = Fragment::aggregate(table(), vec![0], vec![0], vec![Aggregate::CountStar]);
    fragment.filter = Some(Expr::compare(0, CompareOp::Gt, Value::Int8(100_000)));
    assert_eq!(
        evaluate(&reader, &fragment).unwrap().output,
        FragmentOutput::Groups(Vec::new())
    );
}

/// Groups come back in `pg_cmp` order of their keys, whatever order the rows arrived in.
#[test]
fn groups_are_ordered_and_complete() {
    let (_fs, reader) = open();
    let fragment = Fragment::aggregate(
        table(),
        vec![1, 0],
        vec![0],
        vec![Aggregate::CountStar, Aggregate::Max(1)],
    );
    let FragmentOutput::Groups(groups) = evaluate(&reader, &fragment).unwrap().output else {
        panic!("not groups");
    };

    assert_eq!(groups.len(), 4);
    let keys: Vec<&Value> = groups.iter().map(|group| &group.key[0]).collect();
    assert_eq!(
        keys,
        vec![
            &Value::Text("kind-0".into()),
            &Value::Text("kind-1".into()),
            &Value::Text("kind-2".into()),
            &Value::Text("kind-3".into()),
        ]
    );
    for group in &groups {
        assert_eq!(group.aggregates[0], Partial::Count(ROWS as u64 / 4));
    }
    // `kind-3` holds ids 3, 7, ... 499, so its largest id is 499.
    assert_eq!(
        groups[3].aggregates[1],
        Partial::Max(Some(Value::Int8(499)))
    );
}

/// A fragment this build cannot evaluate is refused before anything is read.
#[test]
fn a_refused_fragment_reads_nothing() {
    let (_fs, reader) = open();
    reader.reset_counters();

    let mut fragment = Fragment::scan(table(), vec![0]);
    fragment.range = esker_columnar::KeyRange {
        start: b"a".to_vec(),
        end: Vec::new(),
    };
    let error = evaluate(&reader, &fragment).unwrap_err();
    assert!(error.is_refused(), "{error}");
    assert_eq!(reader.counters().columns_decoded, 0);
    assert_eq!(reader.counters().bytes_read, 0);
}

/// A sum that does not fit is an error, not a wrapped number — and it stops the scan.
#[test]
fn an_overflowing_sum_is_an_error() {
    let fs = MemFileSystem::new();
    let path = Path::new("/s/big.col");
    fs.create_dir_all(path.parent().unwrap()).unwrap();
    let schema = Schema::new(vec![ColumnDef::new("n", ColumnType::Int8)]).unwrap();
    let mut writer = Writer::create(&fs, path, schema, WriterOptions::default()).unwrap();
    for _ in 0..3 {
        writer.append_row(&[Value::Int8(i64::MAX / 2)]).unwrap();
    }
    writer.finish().unwrap();

    let reader = Reader::open(&fs, path).unwrap();
    let fragment = Fragment::aggregate(table(), vec![0], Vec::new(), vec![Aggregate::Sum(0)]);
    let error = evaluate(&reader, &fragment).unwrap_err();
    assert!(error.is_overflow(), "{error}");
    assert!(error.to_string().contains("bigint out of range"), "{error}");
}
