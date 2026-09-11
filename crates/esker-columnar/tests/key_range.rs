//! A scan restricted to a **region's** key range.
//!
//! Not [`Fragment::range`](esker_columnar::Fragment), which a client sends and this build still
//! refuses: which rows a copy may answer for is a property of the *region* that holds it, not of
//! the query, so it arrives as a [`ScanOptions`] field from the store that owns the region record
//! and no caller can get it wrong.
//!
//! # What it is for
//!
//! A run can hold rows the region no longer owns — a parent's runs after a split, which nothing
//! prunes ([ADR 0040](../../../docs/adr/0040-the-engine-a-query-runs-on.md)). Scoping the *build*
//! stops a copy being written with another region's rows; this makes the answer right whatever a
//! run already contains, and `docs/plans/phase-8-learner.md` §store unit 4 records why neither
//! covers the other. The defect both halves close is the `mpp` lane's: a bare `count(*)` answering
//! 4× across regions, because every fragment answered for more than its shard.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_columnar::scan::visible::Visibility;
use esker_columnar::{
    ColumnDef, ColumnType, Fragment, FragmentOutput, KeyRange, Reader, ScanOptions, Schema,
    TableRef, Value, Writer, WriterOptions, evaluate_merged, evaluate_with,
};
use esker_engine::memfs::MemFileSystem;

/// `__key, name, __commit_ts, __deleted` — a store's copy, whose key is the raw row key.
fn schema() -> Schema {
    Schema::new(vec![
        ColumnDef::new("__key", ColumnType::Bytea),
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

fn run(fs: &MemFileSystem, path: &str, keys: &[&str]) -> Reader {
    let path = std::path::Path::new(path);
    let mut writer = Writer::create(
        fs,
        path,
        schema(),
        WriterOptions {
            stripe_rows: 2,
            ..WriterOptions::default()
        },
    )
    .unwrap();
    for key in keys {
        writer
            .append_row(&[
                Value::Bytea(key.as_bytes().to_vec()),
                Value::Text((*key).to_string()),
                Value::Int8(10),
                Value::Bool(false),
            ])
            .unwrap();
    }
    writer.finish().unwrap();
    Reader::open(fs, path).unwrap()
}

fn options(range: Option<KeyRange>) -> ScanOptions {
    ScanOptions {
        prune: true,
        widening: None,
        range,
        visibility: Some(visibility(100)),
    }
}

fn keys_of(output: FragmentOutput) -> Vec<String> {
    match output {
        FragmentOutput::Rows(rows) => rows
            .iter()
            .map(|row| match &row[0] {
                Value::Text(name) => name.clone(),
                other => panic!("unexpected: {other:?}"),
            })
            .collect(),
        FragmentOutput::Groups(groups) => panic!("not rows: {groups:?}"),
    }
}

fn fragment() -> Fragment {
    Fragment::scan(
        TableRef {
            tenant: 1,
            table_id: 1,
        },
        vec![1],
    )
}

/// **The single-reader path keeps only the keys in range.**
#[test]
fn one_run_answers_for_the_region_and_not_the_file() {
    let fs = MemFileSystem::new();
    let reader = run(&fs, "/one.col", &["a", "b", "c", "d", "e"]);

    assert_eq!(
        keys_of(
            evaluate_with(&reader, &fragment(), &options(None))
                .unwrap()
                .output
        ),
        vec!["a", "b", "c", "d", "e"],
        "no range is the whole file, which is every caller that is not a region"
    );
    assert_eq!(
        keys_of(
            evaluate_with(
                &reader,
                &fragment(),
                &options(Some(KeyRange {
                    start: b"b".to_vec(),
                    end: b"d".to_vec(),
                })),
            )
            .unwrap()
            .output
        ),
        vec!["b", "c"],
        "half-open: the start is in and the end is out"
    );
}

/// **An empty bound means opposite things on the two sides**, and reading either as a literal
/// empty string would answer nothing at all.
#[test]
fn an_empty_bound_is_unbounded_on_its_own_side() {
    let fs = MemFileSystem::new();
    let reader = run(&fs, "/two.col", &["a", "b", "c", "d", "e"]);

    for (range, expected, what) in [
        (
            KeyRange {
                start: Vec::new(),
                end: b"c".to_vec(),
            },
            vec!["a", "b"],
            "an empty start is the beginning of the key space",
        ),
        (
            KeyRange {
                start: b"c".to_vec(),
                end: Vec::new(),
            },
            vec!["c", "d", "e"],
            "an empty end is the end of it",
        ),
        (
            KeyRange {
                start: Vec::new(),
                end: Vec::new(),
            },
            vec!["a", "b", "c", "d", "e"],
            "both empty is the whole key space, the one region a single-region cluster has",
        ),
    ] {
        assert_eq!(
            keys_of(
                evaluate_with(&reader, &fragment(), &options(Some(range)))
                    .unwrap()
                    .output
            ),
            expected,
            "{what}"
        );
    }
}

/// **The merged path too**, which is the one a real copy of several runs takes.
///
/// Asserted separately because it is a separate loop: the single-reader scan and the merge each
/// resolve visibility themselves, so a range honoured in one and not the other would be right in
/// every unit test and wrong on every real learner.
#[test]
fn a_merged_read_over_several_runs_honours_the_range() {
    let fs = MemFileSystem::new();
    let readers = vec![
        run(&fs, "/low.col", &["a", "c", "e"]),
        run(&fs, "/high.col", &["b", "d", "f"]),
    ];

    assert_eq!(
        keys_of(
            evaluate_merged(&readers, &fragment(), &options(None))
                .unwrap()
                .output
        ),
        vec!["a", "b", "c", "d", "e", "f"],
        "the merge is in key order across the runs"
    );
    assert_eq!(
        keys_of(
            evaluate_merged(
                &readers,
                &fragment(),
                &options(Some(KeyRange {
                    start: b"c".to_vec(),
                    end: b"f".to_vec(),
                })),
            )
            .unwrap()
            .output
        ),
        vec!["c", "d", "e"],
        "and the range crosses both of them"
    );
}
