//! MVCC visibility through a real file: the newest version of each key a read at `ts` may see.
//!
//! The resolver's own rules are unit-tested beside it. What only a file can show is the wiring —
//! that the scan decodes the version columns whether or not the fragment projects them, that a
//! key's versions resolve correctly across a **stripe boundary**, and that pruning is off.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_columnar::scan::visible::Visibility;
use esker_columnar::{
    ColumnDef, ColumnType, Expr, Fragment, FragmentOutput, Reader, ScanOptions, Schema, TableRef,
    Value, Writer, WriterOptions, evaluate_with, fragment::expr::CompareOp,
};
use esker_engine::memfs::MemFileSystem;

/// `id, name, __commit_ts, __deleted` — the apply target's run shape.
fn schema() -> Schema {
    Schema::new(vec![
        ColumnDef::new("id", ColumnType::Int8),
        ColumnDef::new("name", ColumnType::Text),
        ColumnDef::new("__commit_ts", ColumnType::Int8),
        ColumnDef::new("__deleted", ColumnType::Bool),
    ])
    .unwrap()
}

fn visibility(ts: i64) -> Visibility {
    Visibility {
        key_columns: vec![0],
        ts_column: 2,
        deleted_column: 3,
        ts,
    }
}

/// Writes `versions` in the run's own order: `(id, commit_ts DESC)`.
fn run(fs: &MemFileSystem, rows: &[(i64, &str, i64, bool)], stripe_rows: usize) -> Reader {
    let path = std::path::Path::new("/run.col");
    let mut sorted = rows.to_vec();
    sorted.sort_by(|left, right| left.0.cmp(&right.0).then(right.2.cmp(&left.2)));
    let mut writer = Writer::create(
        fs,
        path,
        schema(),
        WriterOptions {
            stripe_rows,
            ..WriterOptions::default()
        },
    )
    .unwrap();
    for (id, name, ts, deleted) in sorted {
        writer
            .append_row(&[
                Value::Int8(id),
                Value::Text(name.to_string()),
                Value::Int8(ts),
                Value::Bool(deleted),
            ])
            .unwrap();
    }
    writer.finish().unwrap();
    Reader::open(fs, path).unwrap()
}

fn visible_at(reader: &Reader, ts: i64, filter: Option<Expr>) -> Vec<(i64, String)> {
    let mut fragment = Fragment::scan(
        TableRef {
            tenant: 1,
            table_id: 1,
        },
        vec![0, 1],
    );
    fragment.filter = filter;
    let result = evaluate_with(
        reader,
        &fragment,
        &ScanOptions {
            prune: true,
            visibility: Some(visibility(ts)),
        },
    )
    .unwrap();
    match result.output {
        FragmentOutput::Rows(rows) => rows
            .iter()
            .map(|row| match (&row[0], &row[1]) {
                (Value::Int8(id), Value::Text(name)) => (*id, name.clone()),
                other => panic!("unexpected row: {other:?}"),
            })
            .collect(),
        FragmentOutput::Groups(groups) => panic!("not rows: {groups:?}"),
    }
}

/// The version columns are read even though the fragment projects only `id` and `name`.
#[test]
fn a_read_sees_the_newest_version_at_or_below_its_timestamp() {
    let fs = MemFileSystem::new();
    let reader = run(
        &fs,
        &[
            (1, "one@10", 10, false),
            (1, "one@20", 20, false),
            (1, "one@30", 30, false),
            (2, "two@15", 15, false),
        ],
        64,
    );

    assert_eq!(
        visible_at(&reader, 5, None),
        Vec::new(),
        "nothing yet exists"
    );
    assert_eq!(visible_at(&reader, 10, None), [(1, "one@10".into())]);
    assert_eq!(
        visible_at(&reader, 25, None),
        [(1, "one@20".into()), (2, "two@15".into())]
    );
    assert_eq!(
        visible_at(&reader, 99, None),
        [(1, "one@30".into()), (2, "two@15".into())]
    );
}

/// A delete hides what is under it, and only from the timestamp it was committed at.
#[test]
fn a_tombstone_hides_its_key_from_the_moment_it_commits() {
    let fs = MemFileSystem::new();
    let reader = run(
        &fs,
        &[
            (1, "alive", 10, false),
            (1, "", 20, true),
            (2, "other", 10, false),
        ],
        64,
    );
    assert_eq!(
        visible_at(&reader, 15, None),
        [(1, "alive".into()), (2, "other".into())],
        "a read before the delete must still see the row"
    );
    assert_eq!(
        visible_at(&reader, 25, None),
        [(2, "other".into())],
        "the deleted key is gone, and so is the row underneath it"
    );
}

/// A key's versions may straddle a stripe, and "already settled" is a fact about the scan rather
/// than about one stripe. A one-row stripe puts every version in its own.
#[test]
fn a_key_resolves_across_a_stripe_boundary() {
    let fs = MemFileSystem::new();
    let reader = run(
        &fs,
        &[
            (1, "one@10", 10, false),
            (1, "one@20", 20, false),
            (1, "one@30", 30, false),
        ],
        1,
    );
    assert!(reader.stripes().len() >= 3, "the stripes did not split");
    assert_eq!(visible_at(&reader, 25, None), [(1, "one@20".into())]);
}

/// **The pruning trap, pinned.**
///
/// Key 1 is `name='x'` at ts 20 and `name='y'` at ts 10, in separate stripes. Filter `name='y'`,
/// read at ts 30. The right answer is *nothing*: key 1 resolves to the ts-20 version, which fails
/// the filter. A scan that pruned the ts-20 stripe — its statistics say it holds no `'y'` — would
/// resolve key 1 to the ts-10 version instead and return a row that was overwritten.
///
/// `prune: true` is passed deliberately: `Visibility` must override it rather than trust the
/// caller, which is the whole point of the override being in the evaluator and not in the docs.
#[test]
fn visibility_overrides_pruning_rather_than_trusting_the_caller() {
    let fs = MemFileSystem::new();
    let reader = run(
        &fs,
        &[(1, "x", 20, false), (1, "y", 10, false)],
        1, // one row per stripe, so the two versions can be pruned apart
    );
    let filter = Expr::And(
        Box::new(Expr::compare(0, CompareOp::Eq, Value::Int8(1))),
        Box::new(Expr::compare(1, CompareOp::Eq, Value::Text("y".into()))),
    );

    assert_eq!(
        visible_at(&reader, 30, Some(filter)),
        Vec::new(),
        "an overwritten version was returned because its stripe survived pruning"
    );
}
