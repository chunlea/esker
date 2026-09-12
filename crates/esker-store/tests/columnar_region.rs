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
    db.write(batch, &WriteOptions::unsynced()).unwrap();

    let mut batch = WriteBatch::new();
    esker_store::txnkv::commit(db, &mut batch, start_ts, commit_ts, &keys).unwrap();
    db.write(batch, &WriteOptions::unsynced()).unwrap();
    keys
}

fn put(id: i64, name: &str) -> TxnMutation {
    TxnMutation::Put {
        key: row_key(id),
        value: row(id, name),
        read_ts: None,
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
            range: None,
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
    // Region 1, with no log written for it: these tests tee by hand rather than through a
    // driver, so there is nothing for a resume to replay and every open re-walks the region —
    // which is what makes them still about the conversion (`columnar_resume.rs` is the resume).
    ColumnarSlot::new(
        Arc::new(LocalFileSystem::new()) as Arc<dyn FileSystem>,
        dir.join("columnar"),
        1,
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
            read_ts: None,
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
    slot.commit(&db, 0, 41, &keys).unwrap();
    let keys = commit(
        &db,
        50,
        51,
        &[
            TxnMutation::Delete {
                key: row_key(2),
                read_ts: None,
            },
            put(4, "barbara"),
        ],
    );
    slot.commit(&db, 0, 51, &keys).unwrap();

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

/// **A copy opened before the rest of the history arrived still answers for it.**
///
/// `ColumnarSlot::ensure` builds a table's copy **once** — it returns early for a table already in
/// `tables.open` — and the build converts what the `write` column family holds *at that instant*.
/// Everything after that is the tee's: `RaftPeer::tee_columnar` feeds each applied entry in.
///
/// So a row that reaches the engine **without passing through the tee, after the copy is open** is
/// invisible to both halves: too late for the conversion, never offered to the tee. That is not a
/// hypothetical arrival path — it is what a **Raft snapshot install onto an already-open copy**
/// leaves behind, which is the state of a columnar learner that fell behind far enough for the
/// leader to compact past it. (`snapshot.rs`'s
/// `a_columnar_learner_caught_up_by_a_snapshot_answers_for_what_it_brought` covers the other
/// order — a snapshot *before* any copy exists — and passes, because the conversion then sees
/// everything the snapshot brought.)
///
/// This constructs that state directly rather than through Raft, which is what makes it
/// deterministic: `esker-sql`'s `joint_gate` differential produces it about one run in five, and
/// only under load.
#[test]
fn a_copy_opened_before_the_rest_of_the_history_arrived_still_answers_for_it() {
    let (dir, db) = open_db();

    // The catalog record, as the `ALTER` that asks for a copy writes it.
    commit(
        &db,
        10,
        11,
        &[TxnMutation::Put {
            key: Bytes::from(esker_keys::columnar::key(TENANT, TABLE)),
            value: Bytes::from(published(1)),
            read_ts: None,
        }],
    );
    // Part of the history.
    commit(&db, 20, 21, &[put(1, "ada")]);

    let slot = slot(dir.path());
    // **The copy opens here**, converting what the region holds at this instant — one row. The
    // assertion is the denominator: if this were empty the test below would pass by measuring a
    // copy that never worked at all.
    assert_eq!(
        read(&slot, &db, 100),
        vec![(1, "ada".to_owned())],
        "the copy did not convert the history that was there when it opened"
    );

    // The rest of the history reaches the engine **without the tee**, which is what a snapshot
    // install does: the peer's applied index jumps and no entry passes through `tee_columnar`.
    commit(&db, 30, 31, &[put(2, "grace")]);

    // **Stale, and this is the defect stated.** The copy was built once and never looks again, so
    // the row is invisible to a fragment and present to a row scan — the disagreement `joint_gate`
    // reports.
    assert_eq!(
        read(&slot, &db, 100),
        vec![(1, "ada".to_owned())],
        "a copy that was already open somehow noticed a row that reached the engine without the \
         tee; if that is now true the repair below is no longer what makes this work"
    );

    // **And this is the repair, at the seam production now uses.** `Store::fetch_snapshot` closes
    // this region's slot the moment it adopts a snapshot, because that is the moment the staleness
    // is knowable: `ColumnarSlot::saw` catches the same gap from the *next entry applied*, and a
    // learner that takes a snapshot and then goes quiet has no next entry.
    slot.close();
    assert_eq!(
        read(&slot, &db, 100),
        vec![(1, "ada".to_owned()), (2, "grace".to_owned())],
        "closing the copy did not make the next ask re-walk the region, so a snapshot's rows still \
         never reach it"
    );
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
    slot.commit(&db, 0, 31, &keys).unwrap();
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
            read_ts: None,
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

/// A slot for `region_id`, so a test can hold two of them over one store.
fn slot_for(dir: &std::path::Path, region_id: u64) -> ColumnarSlot {
    ColumnarSlot::new(
        Arc::new(LocalFileSystem::new()) as Arc<dyn FileSystem>,
        dir.join("columnar").join(region_id.to_string()),
        region_id,
        ColumnarOptions::default(),
    )
}

/// Writes the region records the walk reads its own range out of.
fn record_regions(db: &Db, regions: &[(u64, Bytes, Bytes)]) {
    let mut batch = WriteBatch::new();
    let raft = db.cf_id(cf::RAFT).expect("the raft column family");
    for (id, start_key, end_key) in regions {
        esker_store::meta::stage_region(
            &mut batch,
            raft,
            &esker_proto::Region {
                id: *id,
                start_key: start_key.clone(),
                end_key: end_key.clone(),
                peers: vec![esker_proto::Peer::voter(*id, *id)],
                epoch: esker_proto::Epoch::INITIAL,
            },
        );
    }
    db.write(batch, &WriteOptions::default())
        .expect("the region records are written");
}

/// **A region's copy holds that region's rows and no others.**
///
/// The walk that builds a copy for a newly placed learner was scoped to
/// `table_row_range(tenant, table_id)` — the whole *table* — and a store's `write` column family
/// holds the rows of **every region of that table the store hosts**. So on a store with two of
/// them each copy was fed both, every fragment answered for more rows than its shard covered, and
/// the SQL node added the shards up: the `mpp` lane measured a bare `count(*)` at 4× with four
/// learners on two stores.
///
/// Everything else was already region-scoped — the slot is keyed by region, its directory is named
/// for the region, the live tee sees only its own peer's entries — which is what made a single
/// unscoped range hard to see. It needs **two regions of one table on one store** to show at all,
/// and that shape did not exist before a SQL table could split
/// ([ADR 0073](../../../docs/adr/0073-a-regions-size-is-the-data-families-it-spans.md)).
///
/// The assertion is the rows in each copy rather than a count, so a failure names which region's
/// rows leaked into which copy.
#[test]
fn a_region_copy_holds_only_the_rows_of_its_own_region() {
    let (dir, db) = open_db();
    commit(
        &db,
        10,
        11,
        &[TxnMutation::Put {
            key: Bytes::from(esker_keys::columnar::key(TENANT, TABLE)),
            value: Bytes::from(published(1)),
            read_ts: None,
        }],
    );
    // Four rows of one table, and a boundary between 2 and 3.
    commit(&db, 20, 21, &[put(1, "ada"), put(2, "grace")]);
    commit(&db, 22, 23, &[put(3, "alan"), put(4, "edsger")]);

    // Two regions of that table on this one store, split at row 3's key.
    let boundary = row_key(3);
    record_regions(
        &db,
        &[
            (1, Bytes::new(), boundary.clone()),
            (2, boundary, Bytes::new()),
        ],
    );

    let low = slot_for(dir.path(), 1);
    let high = slot_for(dir.path(), 2);
    assert_eq!(
        read(&low, &db, 100),
        vec![(1, "ada".to_owned()), (2, "grace".to_owned())],
        "region 1 owns rows below the boundary and must hold no others"
    );
    assert_eq!(
        read(&high, &db, 100),
        vec![(3, "alan".to_owned()), (4, "edsger".to_owned())],
        "region 2 owns rows from the boundary up"
    );
}

/// **A region with no record is the whole key space, which is what the walk always did.**
///
/// The conservative direction — too much rather than too little — and the one every other test in
/// this file relies on, because none of them writes a region record.
#[test]
fn a_slot_with_no_region_record_still_converts_the_whole_table() {
    let (dir, db) = open_db();
    commit(
        &db,
        10,
        11,
        &[TxnMutation::Put {
            key: Bytes::from(esker_keys::columnar::key(TENANT, TABLE)),
            value: Bytes::from(published(1)),
            read_ts: None,
        }],
    );
    commit(&db, 20, 21, &[put(1, "ada"), put(2, "grace")]);

    let only = slot_for(dir.path(), 7);
    assert_eq!(
        read(&only, &db, 100),
        vec![(1, "ada".to_owned()), (2, "grace".to_owned())]
    );
}
