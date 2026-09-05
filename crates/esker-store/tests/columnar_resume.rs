//! Reopening a columnar copy **resumes** it from the index its manifest names.
//!
//! `columnar_region.rs` defends what a copy answers; this defends what an open has to read to be
//! able to answer it. Before ADR 0038 every open re-walked the region's whole committed state —
//! correct, and linear in the region at a moment when nothing was wrong. The manifest now names
//! the apply index its runs are complete to, so an open replays the log from there.
//!
//! The three things asserted here are the three that can go wrong:
//!
//! 1. a resume reads **only** what arrived after the manifest's index, and the copy it produces is
//!    the same copy the full walk produced;
//! 2. a manifest older than the log's truncation point falls back to the full walk, loudly, rather
//!    than replaying entries that are not there;
//! 3. the manifest never claims more than the runs durably hold — asserted against a copy whose
//!    buffer was dropped exactly as a crash would drop it.

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
use esker_raft::{ConfState, Entry};
use esker_store::columnar::ColumnarOptions;
use esker_store::columnar::region::ColumnarSlot;
use esker_store::{Command, RaftLogStorage};

const TENANT: u64 = 1;
const TABLE: u64 = 7;
const REGION: u64 = 3;

fn open_db(dir: &std::path::Path) -> Arc<Db> {
    Arc::new(
        Db::open_with(
            dir,
            Options {
                create_if_missing: true,
                wal_sync_mode: WalSyncMode::Never,
                ..Options::default()
            },
            Arc::new(LocalFileSystem::new()),
            &cf::BUILTIN,
        )
        .unwrap(),
    )
}

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

fn put(id: i64, name: &str) -> TxnMutation {
    TxnMutation::Put {
        key: row_key(id),
        value: row(id, name),
        read_ts: None,
    }
}

/// The region's log, so a resume has entries to replay. The whole point is that this is the real
/// writer: a resume that only worked against entries a test wrote by hand would prove nothing.
struct Log {
    storage: RaftLogStorage,
    index: u64,
}

impl Log {
    fn open(db: &Arc<Db>) -> Self {
        Self {
            storage: RaftLogStorage::open(Arc::clone(db), REGION, ConfState::default()).unwrap(),
            index: 0,
        }
    }

    /// Appends the entry that commits `keys` at `commit_ts` and marks it applied, exactly as the
    /// driver does: the payload the log carries, then the apply index written with the data.
    fn commit(&mut self, db: &Db, start_ts: u64, commit_ts: u64, mutations: &[TxnMutation]) -> u64 {
        let keys: Vec<Bytes> = mutations.iter().map(|m| m.key().clone()).collect();
        let primary = keys[0].clone();

        let mut batch = WriteBatch::new();
        esker_store::txnkv::prewrite(db, &mut batch, start_ts, &primary, 3_000, mutations).unwrap();
        db.write(batch, &WriteOptions::unsynced()).unwrap();

        self.index += 1;
        let payload = Command::Txn(esker_store::txn_command::TxnCommand::Commit {
            start_ts,
            commit_ts,
            keys: keys.clone(),
        })
        .encode();
        let mut batch = WriteBatch::new();
        self.storage
            .stage_ready(&mut batch, None, &[Entry::normal(1, self.index, payload)]);
        esker_store::txnkv::commit(db, &mut batch, start_ts, commit_ts, &keys).unwrap();
        self.storage.stage_applied(&mut batch, self.index);
        db.write(batch, &WriteOptions::unsynced()).unwrap();
        self.index
    }
}

fn slot(dir: &std::path::Path) -> ColumnarSlot {
    ColumnarSlot::new(
        Arc::new(LocalFileSystem::new()) as Arc<dyn FileSystem>,
        dir.join("columnar"),
        REGION,
        ColumnarOptions::default(),
    )
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
            range: None,
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

/// Writes the catalog record that asks for a copy, through the log like everything else.
fn ask_for_a_copy(db: &Db, log: &mut Log) {
    log.commit(
        db,
        10,
        11,
        &[TxnMutation::Put {
            key: Bytes::from(esker_keys::columnar::key(TENANT, TABLE)),
            value: Bytes::from(published(1)),
            read_ts: None,
        }],
    );
}

/// **The unit.** A reopen replays the log from the manifest's index and reads nothing older.
#[test]
fn a_reopen_replays_only_what_arrived_after_the_manifest_says() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_db(dir.path());
    let mut log = Log::open(&db);
    ask_for_a_copy(&db, &mut log);

    // Twelve versions the region committed before anything asked for a copy of them.
    for id in 0_i64..12 {
        log.commit(
            &db,
            20 + id.unsigned_abs() * 2,
            21 + id.unsigned_abs() * 2,
            &[put(id, &format!("row-{id}"))],
        );
    }

    let first = slot(dir.path());
    // The first open has no manifest, so it walks the region — twelve versions, from nothing.
    assert_eq!(read(&first, &db, 1_000).len(), 12);
    let build = first.last_build(TENANT, TABLE).expect("a build");
    assert!(!build.resumed, "the first open resumed from somewhere");
    assert_eq!(build.from_index, 0);
    assert_eq!(build.versions, 12);
    let sealed_at = build.to_index;
    drop(first);

    // One more commit, teed to nothing — the copy on disk is complete only to `sealed_at`.
    let index = log.commit(&db, 90, 91, &[put(99, "arrived-after")]);
    assert!(index > sealed_at);

    let second = slot(dir.path());
    assert_eq!(
        read(&second, &db, 1_000).len(),
        13,
        "the resumed copy is missing a version the full walk would have found",
    );
    let build = second.last_build(TENANT, TABLE).expect("a build");
    assert!(
        build.resumed,
        "the reopen re-walked the region instead of resuming from its manifest",
    );
    assert_eq!(
        build.from_index, sealed_at,
        "the resume started somewhere other than the index the manifest names",
    );
    assert_eq!(
        build.versions, 1,
        "the resume read {} versions where one arrived after the manifest's index",
        build.versions,
    );
    assert_eq!(build.to_index, index);

    // And the copy it produced is the copy a full walk produces, contents and all.
    assert_eq!(
        read(&second, &db, 1_000),
        {
            let fresh = tempfile::tempdir().unwrap();
            let walked = slot(fresh.path());
            read(&walked, &db, 1_000)
        },
        "a resumed copy and a re-walked one disagree",
    );
}

/// A manifest older than the log's truncation point cannot be replayed from, so the open re-walks
/// the region — the answer that was always correct.
#[test]
fn a_manifest_older_than_the_log_falls_back_to_the_full_walk() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_db(dir.path());
    let mut log = Log::open(&db);
    ask_for_a_copy(&db, &mut log);
    for id in 0_i64..4 {
        log.commit(
            &db,
            20 + id.unsigned_abs() * 2,
            21 + id.unsigned_abs() * 2,
            &[put(id, "x")],
        );
    }

    let first = slot(dir.path());
    assert_eq!(read(&first, &db, 1_000).len(), 4);
    let sealed_at = first.last_build(TENANT, TABLE).unwrap().to_index;
    drop(first);

    // The log is compacted past what the copy names, which is exactly the entries a resume would
    // have replayed.
    let index = log.commit(&db, 90, 91, &[put(9, "after")]);
    let mut batch = WriteBatch::new();
    log.storage
        .stage_compact(&mut batch, index, 1, ConfState::default())
        .unwrap();
    db.write(batch, &WriteOptions::synced()).unwrap();
    assert!(log.storage.truncated_index() > sealed_at);

    let second = slot(dir.path());
    assert_eq!(
        read(&second, &db, 1_000).len(),
        5,
        "the fallback lost a version",
    );
    let build = second.last_build(TENANT, TABLE).expect("a build");
    assert!(
        !build.resumed,
        "a copy older than the log's truncation point was resumed from anyway",
    );
    assert_eq!(build.versions, 5, "the fallback did not re-walk the region");
}

/// **The crash ordering.** The manifest may never claim an index whose versions are only in a
/// buffer, because a buffer is what a crash takes.
///
/// The copy here is fed and then dropped without a seal, which is the crash: the runs on disk hold
/// what the last seal took and nothing more. A reopen that trusted an index past that would answer
/// without the versions in between and never say so.
#[test]
fn the_manifest_never_claims_more_than_the_runs_hold() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_db(dir.path());
    let mut log = Log::open(&db);
    ask_for_a_copy(&db, &mut log);
    log.commit(&db, 20, 21, &[put(1, "ada")]);

    let first = slot(dir.path());
    assert_eq!(read(&first, &db, 1_000).len(), 1);
    let sealed_at = first.last_build(TENANT, TABLE).unwrap().to_index;

    // Three more entries, teed and left in the buffer: nothing seals them.
    let mut last = sealed_at;
    for id in 2_i64..5 {
        last = log.commit(
            &db,
            20 + id.unsigned_abs() * 10,
            21 + id.unsigned_abs() * 10,
            &[put(id, "buffered")],
        );
        first
            .commit(&db, last, 21 + id.unsigned_abs() * 10, &[row_key(id)])
            .unwrap();
    }
    assert!(last > sealed_at);
    // The crash: the slot goes without a seal, so the buffer never reaches a run.
    drop(first);

    let second = slot(dir.path());
    assert_eq!(
        read(&second, &db, 1_000).len(),
        4,
        "the reopen answered without the versions the crash took from the buffer",
    );
    let build = second.last_build(TENANT, TABLE).expect("a build");
    assert!(
        build.from_index <= sealed_at,
        "the manifest claimed index {} where the runs were sealed at {sealed_at}",
        build.from_index,
    );
    assert_eq!(
        build.versions, 3,
        "the resume replayed {} entries where three were left in the buffer",
        build.versions,
    );
}

/// **A copy that was not told of every entry re-walks instead of answering.**
///
/// [`a_manifest_older_than_the_log_falls_back_to_the_full_walk`] proves the refusal works on the
/// way *in*: a slot opened against a manifest older than the log's truncation point re-walks,
/// because the entries a resume would replay are gone. A copy that is **already open** never goes
/// through that door again — `ensure` short-circuits on the table being open — and this is the
/// case that puts one there.
///
/// A **snapshot install** writes committed versions straight into the column families: no entry
/// applies, so `RaftPeer::tee_columnar` never runs, and the log it lands on begins after the
/// snapshot's index. A copy that was behind is missing everything in between with no path back to
/// it — the log cannot replay those entries and nothing re-walks. `Store::fetch_snapshot` replaces
/// the region through `Store::retire_region_now`, which stops the peer and leaves the slot where it
/// was, so the copy that comes back is the one that was there before.
///
/// **The entry teed afterwards is the point of this test**, and the reason the check cannot live on
/// the read path. The peer carries on applying, and one entry ingested after the gap leaves every
/// index the copy holds looking continuous again; asked later, nothing distinguishes it from a copy
/// that saw everything. `ColumnarSlot::saw` is told about every applied entry, so the gap is caught
/// at the only moment it is visible.
///
/// This is the wrong answer `esker-sql`'s `joint_gate` differential caught under load: the fragment
/// answered without the rows of one entry, and the only row whose visible state depended on that
/// entry disappeared (`docs/plans/phase-16-mpp.md` §J13).
#[test]
fn a_copy_not_told_of_every_entry_re_walks_instead_of_answering() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_db(dir.path());
    let mut log = Log::open(&db);
    ask_for_a_copy(&db, &mut log);
    for id in 0_i64..4 {
        log.commit(
            &db,
            20 + id.unsigned_abs() * 2,
            21 + id.unsigned_abs() * 2,
            &[put(id, "x")],
        );
    }

    // Opened here and **kept**, which is the whole difference from the test above.
    let open = slot(dir.path());
    assert_eq!(read(&open, &db, 1_000).len(), 4);
    let sealed_at = open.last_build(TENANT, TABLE).unwrap().to_index;

    // The transfer: a version this copy is never told about, on a log compacted past everything it
    // holds. Both halves of a snapshot install, and neither reaches the copy.
    let unseen = log.commit(&db, 90, 91, &[put(9, "brought-by-the-snapshot")]);
    let mut batch = WriteBatch::new();
    log.storage
        .stage_compact(&mut batch, unseen, 1, ConfState::default())
        .unwrap();
    db.write(batch, &WriteOptions::synced()).unwrap();
    assert!(log.storage.truncated_index() > sealed_at);

    // And the peer carries on. This entry is applied and teed normally, and until `saw` existed it
    // was enough to make the copy's indices continuous again.
    let next = log.commit(&db, 92, 93, &[put(10, "arrived-after")]);
    open.saw(next);
    open.commit(&db, next, 93, &[row_key(10)]).unwrap();

    assert_eq!(
        read(&open, &db, 1_000).len(),
        6,
        "the copy answered from what it held when the log left it behind, plus what it was teed \
         after; the versions in between are in the region and in no log, so a copy that does not \
         re-walk here never holds them",
    );
}
