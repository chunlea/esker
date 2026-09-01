//! The columnar copy **on a region**: built from what the region committed, kept up by its apply.
//!
//! `columnar_differential.rs` defends the apply target against a reference written longhand, over
//! a workload handed straight to it. This defends the layer that decides there should be a target
//! at all (`crate::columnar::region`): reading the table's schema out of the catalog the region
//! carries, converting the versions it already holds, following the ones that arrive after, and
//! resolving MVCC at the timestamp a fragment asks for.
//!
//! # Why the data is written through Percolator here
//!
//! Because that is what a columnar learner sees. A committed row is a `write` record pointing at a
//! `default` value, written by two entries at different times, and a copy fed anything simpler
//! would be a copy of something no region ever holds. So this test prewrites and commits through
//! `esker_store::txnkv` — the same functions the apply path calls — and then asks the slot what the
//! region committed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use bytes::Bytes;
use esker_columnar::scan::visible::Visibility;
use esker_columnar::{Fragment, FragmentOutput, Reader, ScanOptions, TableRef, Value};
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_engine::{Db, Options, WalSyncMode, WriteBatch, WriteOptions, cf};
use esker_keys::columnar::Published;
use esker_keys::value::{ColumnType, Datum};
use esker_proto::TxnMutation;
use esker_store::columnar::ColumnarOptions;
use esker_store::columnar::region::ColumnarSlot;

const TENANT: u64 = 1;
const TABLE: u64 = 7;

fn open_db() -> (tempfile::TempDir, Arc<Db>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_with(
        dir.path(),
        Options {
            create_if_missing: true,
            wal_sync_mode: WalSyncMode::Never,
            ..Options::default()
        },
        Arc::new(LocalFileSystem::new()),
        &cf::BUILTIN,
    )
    .unwrap();
    (dir, Arc::new(db))
}

/// `id int8, name text`, which is what the rows below encode.
fn published(schema_version: u64) -> Vec<u8> {
    esker_keys::columnar::encode(
        1,
        Some(&Published {
            schema_version,
            columns: vec![(ColumnType::Int8, None), (ColumnType::Text, None)],
        }),
    )
    .unwrap()
}

fn row_key(id: i64) -> Bytes {
    Bytes::from(esker_keys::row::row_key(TENANT, TABLE, &[Datum::Int8(id)]).unwrap())
}

fn row(id: i64, name: &str) -> Bytes {
    Bytes::from(
        esker_keys::row::encode_row(
            &[ColumnType::Int8, ColumnType::Text],
            &[Datum::Int8(id), Datum::Text(name.into())],
        )
        .unwrap(),
    )
}

/// Commits one transaction, as the apply path would: a prewrite entry, then a commit entry.
fn commit(db: &Db, start_ts: u64, commit_ts: u64, mutations: &[TxnMutation]) -> Vec<Bytes> {
    let keys: Vec<Bytes> = mutations.iter().map(|m| m.key().clone()).collect();
    let primary = keys[0].clone();

    let mut batch = WriteBatch::new();
    esker_store::txnkv::prewrite(db, &mut batch, start_ts, &primary, 3_000, mutations).unwrap();
    db.write(batch, &WriteOptions { sync: false }).unwrap();

    let mut batch = WriteBatch::new();
    esker_store::txnkv::commit(db, &mut batch, start_ts, commit_ts, &keys).unwrap();
    db.write(batch, &WriteOptions { sync: false }).unwrap();
    keys
}

fn put(id: i64, name: &str) -> TxnMutation {
    TxnMutation::Put {
        key: row_key(id),
        value: row(id, name),
    }
}

/// The rows a fragment returns from the slot's runs, at one visibility timestamp.
fn read(slot: &ColumnarSlot, db: &Db, ts: u64) -> Vec<(i64, String)> {
    let runs = slot.table(db, TENANT, TABLE).unwrap().expect("a copy");
    if runs.paths.is_empty() {
        return Vec::new();
    }
    let fs = LocalFileSystem::new();
    let readers: Vec<Reader> = runs
        .paths
        .iter()
        .map(|path| Reader::open(&fs, path).unwrap())
        .collect();
    let (key_column, ts_column, deleted_column) = runs.visibility;
    let result = esker_columnar::evaluate_merged(
        &readers,
        &Fragment::scan(
            TableRef {
                tenant: TENANT,
                table_id: TABLE,
            },
            vec![0, 1],
        ),
        &ScanOptions {
            prune: true,
            widening: None,
            visibility: Some(Visibility {
                key_columns: vec![key_column],
                ts_column,
                deleted_column,
                ts: i64::try_from(ts).unwrap(),
            }),
        },
    )
    .unwrap();
    let FragmentOutput::Rows(rows) = result.output else {
        panic!("a scan fragment returned groups");
    };
    let mut out: Vec<(i64, String)> = rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Int8(id), Value::Text(name)) => (*id, name.clone()),
            other => panic!("unexpected row {other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn slot(dir: &std::path::Path) -> ColumnarSlot {
    ColumnarSlot::new(
        Arc::new(LocalFileSystem::new()) as Arc<dyn FileSystem>,
        dir.join("columnar"),
        ColumnarOptions::default(),
    )
}

/// The whole of it: a copy built from history, extended by the log, read at a timestamp.
#[test]
fn a_copy_is_built_from_what_the_region_holds_and_kept_up_by_what_arrives() {
    let (dir, db) = open_db();

    // The catalog first, as the `ALTER` that asks for a copy writes it.
    commit(
        &db,
        10,
        11,
        &[TxnMutation::Put {
            key: Bytes::from(esker_keys::columnar::key(TENANT, TABLE)),
            value: Bytes::from(published(1)),
        }],
    );
    // Then rows the region committed **before** anything asked for a copy of them.
    commit(&db, 20, 21, &[put(1, "ada"), put(2, "grace")]);
    commit(&db, 30, 31, &[put(3, "edsger")]);

    let slot = slot(dir.path());
    // The first read builds the copy: nothing has been teed to it, and it must still be complete.
    assert_eq!(
        read(&slot, &db, 100),
        vec![
            (1, "ada".to_owned()),
            (2, "grace".to_owned()),
            (3, "edsger".to_owned())
        ],
        "the conversion did not cover what the region already held",
    );

    // And then the apply path extends it: an update, a delete, and a new row.
    let keys = commit(&db, 40, 41, &[put(1, "ada lovelace")]);
    slot.commit(&db, 41, &keys).unwrap();
    let keys = commit(
        &db,
        50,
        51,
        &[TxnMutation::Delete { key: row_key(2) }, put(4, "barbara")],
    );
    slot.commit(&db, 51, &keys).unwrap();

    assert_eq!(
        read(&slot, &db, 100),
        vec![
            (1, "ada lovelace".to_owned()),
            (3, "edsger".to_owned()),
            (4, "barbara".to_owned())
        ],
        "the copy did not follow the versions that arrived after it was built",
    );

    // **The past is still the past.** A read before the update sees the row it replaced, and a
    // read before the delete still sees the row it removed — which is the whole reason a delete is
    // stored as a version rather than as an absence.
    assert_eq!(
        read(&slot, &db, 31),
        vec![
            (1, "ada".to_owned()),
            (2, "grace".to_owned()),
            (3, "edsger".to_owned())
        ],
        "a read at an earlier timestamp saw a later state",
    );
    assert_eq!(
        read(&slot, &db, 41),
        vec![
            (1, "ada lovelace".to_owned()),
            (2, "grace".to_owned()),
            (3, "edsger".to_owned())
        ],
    );
    assert_eq!(read(&slot, &db, 5), Vec::new(), "nothing existed yet");
}

/// A table nobody asked for a copy of does not get one, and says so.
#[test]
fn a_table_with_no_columnar_record_has_no_copy() {
    let (dir, db) = open_db();
    commit(&db, 20, 21, &[put(1, "ada")]);
    let slot = slot(dir.path());
    assert!(
        slot.table(&db, TENANT, TABLE).unwrap().is_none(),
        "a table with no record was given a copy",
    );
    // And the apply path leaves it alone rather than buffering rows it cannot decode.
    let keys = commit(&db, 30, 31, &[put(2, "grace")]);
    slot.commit(&db, 31, &keys).unwrap();
    assert!(!slot.is_open());
}

/// A copy is **rebuilt** when it is opened, because a partial one is indistinguishable from a
/// whole one.
///
/// The case this is written from: a crash loses the memtable a seal had not taken yet, and the
/// runs left behind look exactly like a complete copy. Reopening must not trust them.
#[test]
fn opening_a_copy_rebuilds_it_from_the_region() {
    let (dir, db) = open_db();
    commit(
        &db,
        10,
        11,
        &[TxnMutation::Put {
            key: Bytes::from(esker_keys::columnar::key(TENANT, TABLE)),
            value: Bytes::from(published(1)),
        }],
    );
    commit(&db, 20, 21, &[put(1, "ada")]);

    let first = slot(dir.path());
    assert_eq!(read(&first, &db, 100), vec![(1, "ada".to_owned())]);
    drop(first);

    // Versions this copy never saw, because nothing was teeing to it: exactly what a crash between
    // a seal and the next one leaves behind.
    commit(&db, 30, 31, &[put(2, "grace")]);

    let second = slot(dir.path());
    assert_eq!(
        read(&second, &db, 100),
        vec![(1, "ada".to_owned()), (2, "grace".to_owned())],
        "reopening trusted runs that were missing a version",
    );
}
