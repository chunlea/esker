//! The model test: a database is a `BTreeMap`, and this checks that it is.
//!
//! `prompts/01-engine.md` asks for "proptest of random `put/delete/get/scan/snapshot/flush/
//! compact` sequences against a `BTreeMap<(cf, key), value>` model with per-snapshot views".
//! The map is the specification. Every operation is applied to both, and every read is
//! required to agree — so a bug does not need to be imagined in advance to be caught, only
//! reachable from a sequence of ordinary operations.
//!
//! # What is modelled
//!
//! `put`, `delete`, `get`, bounded scans in both directions, multi-column-family write
//! batches, snapshot create/read/scan/drop, `flush`, and `reopen`. Three built-in column
//! families, twenty-four keys, and values of varying length: a key space small enough that
//! overwrites, deletes and re-inserts of the same key collide constantly, which is where the
//! interesting bugs are.
//!
//! # Snapshots are the interesting part
//!
//! A snapshot's model is a **clone of the map taken at the same moment**. Every read through
//! that snapshot must equal its clone no matter what has happened since, which is the property
//! that catches a read path that forgets to filter by sequence number, and it is checked while
//! later mutations are still arriving.
//!
//! # Snapshots do not survive a reopen
//!
//! That is this file's definition, the model drops its clones at every `Reopen`, and the
//! engine now enforces it: a handle from a closed database is refused rather than accepted as
//! a bare number — see [`a_snapshot_taken_before_a_reopen_is_refused`].
//!
//! # `flush` and `compact` must be invisible
//!
//! Moving data from a memtable to an L0 file changes where it lives, never what it is, so a
//! flush is checked as a no-op *for the model*: the whole database and every live snapshot are
//! re-verified across it. `compact` is not modelled because the engine has no compaction API
//! yet; [`compaction_still_has_no_public_api`] is the tripwire that says so out loud the day
//! it appears.
//!
//! # Shrinking
//!
//! Operations are a small enum over `u8` fields, mapped into range at execution time rather
//! than in the strategy. There are no paths, timestamps or seeds inside an operation, so a
//! failing sequence shrinks to something a person can read — which is the only reason a
//! failure from ten thousand cases is worth anything.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use esker_engine::batch::WriteBatch;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions, WriteOptions};
use esker_engine::{Db, Snapshot, cf};
use proptest::prelude::*;

const DIR: &str = "/db";

/// The families every sequence uses. Three, so that a batch can be atomic across more than
/// one and a scan of one cannot see another's keys.
const CFS: [&str; 3] = [cf::DEFAULT, cf::LOCK, cf::WRITE];

/// Distinct keys. Small on purpose: collisions are the point.
const KEYS: u8 = 24;

/// Sorts after every key [`key_of`] produces, so a scan bounded by it is unbounded.
const LAST_KEY: &[u8] = b"\xff\xff\xff";

const CASES: u32 = 1_000;
const CASES_IGNORED: u32 = 10_000;

/// The specification: every live key, in one ordered map, tagged with its column family.
type Model = BTreeMap<(usize, Vec<u8>), Vec<u8>>;

/// Live snapshots, each beside the map as it was when the snapshot was taken.
type Snapshots = Vec<(Snapshot, Model)>;

/// What a scan produces: whole entries, key and value, in key order.
type Entries = Vec<(Vec<u8>, Vec<u8>)>;

fn config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

/// The acceptance criterion is a number, so it is asserted rather than trusted.
#[test]
fn the_property_configuration_runs_a_thousand_cases_and_ten_on_demand() {
    assert_eq!(config(CASES).cases, 1_000);
    assert_eq!(CASES_IGNORED, 10_000);
}

// ---------------------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------------------

/// One change inside a write batch.
#[derive(Debug, Clone)]
enum Mutation {
    Put(u8, u8, u8),
    Delete(u8, u8),
}

/// One step of a sequence. Fields are raw `u8`s mapped into range when the step runs, so that
/// shrinking pulls them towards zero instead of towards an arbitrary strategy's idea of
/// simple.
#[derive(Debug, Clone)]
enum Op {
    Put(u8, u8, u8),
    Delete(u8, u8),
    Get(u8, u8),
    /// Several changes, possibly across column families, applied atomically.
    Batch(Vec<Mutation>),
    /// A bounded scan. `lo > hi` is an empty range and deliberately reachable.
    Scan {
        cf: u8,
        lo: u8,
        hi: u8,
        reverse: bool,
    },
    SnapshotCreate,
    SnapshotRead {
        slot: u8,
        cf: u8,
        key: u8,
    },
    SnapshotScan {
        slot: u8,
        cf: u8,
    },
    SnapshotDrop(u8),
    Flush(u8),
    Reopen,
    // TODO(spine step 7): a `Compact` variant, once `Db` has a compaction API. It belongs
    // beside `Flush` — same shape, same assertion, data identical through every live snapshot
    // and after a reopen. `compaction_still_has_no_public_api` fails when that day comes.
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        3 => (any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(cf, key, value)| Mutation::Put(cf, key, value)),
        1 => (any::<u8>(), any::<u8>()).prop_map(|(cf, key)| Mutation::Delete(cf, key)),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(cf, key, value)| Op::Put(cf, key, value)),
        3 => (any::<u8>(), any::<u8>()).prop_map(|(cf, key)| Op::Delete(cf, key)),
        3 => (any::<u8>(), any::<u8>()).prop_map(|(cf, key)| Op::Get(cf, key)),
        2 => prop::collection::vec(mutation(), 1..5).prop_map(Op::Batch),
        4 => (any::<u8>(), any::<u8>(), any::<u8>(), any::<bool>())
            .prop_map(|(cf, lo, hi, reverse)| Op::Scan { cf, lo, hi, reverse }),
        2 => Just(Op::SnapshotCreate),
        3 => (any::<u8>(), any::<u8>(), any::<u8>())
            .prop_map(|(slot, cf, key)| Op::SnapshotRead { slot, cf, key }),
        2 => (any::<u8>(), any::<u8>()).prop_map(|(slot, cf)| Op::SnapshotScan { slot, cf }),
        1 => any::<u8>().prop_map(Op::SnapshotDrop),
        2 => any::<u8>().prop_map(Op::Flush),
        1 => Just(Op::Reopen),
    ]
}

fn cf_index(raw: u8) -> usize {
    usize::from(raw) % CFS.len()
}

fn key_of(raw: u8) -> Vec<u8> {
    format!("k{:02}", raw % KEYS).into_bytes()
}

/// Values vary in length as well as content, so a read that returns the right *number* of
/// bytes from the wrong place is still visible.
fn value_of(raw: u8) -> Vec<u8> {
    let mut out = format!("v{raw:03}-").into_bytes();
    out.extend(std::iter::repeat_n(b'x', usize::from(raw) % 19));
    out
}

// ---------------------------------------------------------------------------------------
// The world: an engine, a model, and the snapshots of both
// ---------------------------------------------------------------------------------------

struct World {
    fs: Arc<dyn FileSystem>,
    /// `None` only for the instant inside [`World::reopen`] between dropping the old database
    /// and opening the new one. Two `Db`s on one directory would both claim the log.
    db: Option<Db>,
    model: Model,
    snapshots: Snapshots,
}

fn fail(context: &str, error: &impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(format!("{context}: {error}"))
}

impl World {
    fn new() -> Result<Self, TestCaseError> {
        let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
        let db = Db::open_with(
            DIR,
            Options {
                create_if_missing: true,
                ..Options::default()
            },
            Arc::clone(&fs),
            &CFS,
        )
        .map_err(|error| fail("opening the database", &error))?;
        Ok(Self {
            fs,
            db: Some(db),
            model: Model::new(),
            snapshots: Vec::new(),
        })
    }

    fn db(&self) -> &Db {
        self.db.as_ref().expect("a database is open")
    }

    /// The model's entries for one column family, in key order.
    fn expected(model: &Model, cf: usize) -> Entries {
        model
            .iter()
            .filter(|((family, _), _)| *family == cf)
            .map(|((_, key), value)| (key.clone(), value.clone()))
            .collect()
    }

    /// Walks one column family between `lo` and `hi` inclusive, in one direction.
    fn scan(
        &self,
        cf: usize,
        lo: &[u8],
        hi: &[u8],
        reverse: bool,
        options: &ReadOptions,
    ) -> Result<Entries, TestCaseError> {
        let mut iter = self
            .db()
            .iter(CFS[cf], options)
            .map_err(|error| fail("opening an iterator", &error))?;
        let mut got = Vec::new();

        if reverse {
            iter.seek_for_prev(hi);
            while iter.valid() && iter.key() >= lo {
                got.push((iter.key().to_vec(), iter.value().to_vec()));
                iter.prev();
            }
            got.reverse();
        } else {
            iter.seek(lo);
            while iter.valid() && iter.key() <= hi {
                got.push((iter.key().to_vec(), iter.value().to_vec()));
                iter.next();
            }
        }
        iter.status().map_err(|error| fail("scanning", &error))?;
        Ok(got)
    }

    /// Applies one operation to the engine and to the model, checking anything it can read.
    fn apply(&mut self, op: &Op) -> Result<(), TestCaseError> {
        match op {
            Op::Put(cf, key, value) => self.put(*cf, *key, *value),
            Op::Delete(cf, key) => self.delete(*cf, *key),
            Op::Get(cf, key) => self.get(*cf, *key),
            Op::Batch(mutations) => self.batch(mutations),
            Op::Scan {
                cf,
                lo,
                hi,
                reverse,
            } => self.bounded_scan(*cf, *lo, *hi, *reverse),
            Op::SnapshotCreate => {
                let snapshot = self.db().snapshot();
                self.snapshots.push((snapshot, self.model.clone()));
                Ok(())
            }
            Op::SnapshotRead { slot, cf, key } => self.snapshot_read(*slot, *cf, *key),
            Op::SnapshotScan { slot, cf } => self.snapshot_scan(*slot, *cf),
            Op::SnapshotDrop(slot) => {
                if !self.snapshots.is_empty() {
                    let slot = usize::from(*slot) % self.snapshots.len();
                    self.snapshots.remove(slot);
                }
                Ok(())
            }
            Op::Flush(cf) => self.flush(*cf),
            Op::Reopen => self.reopen(),
        }
    }

    fn put(&mut self, cf: u8, key: u8, value: u8) -> Result<(), TestCaseError> {
        let (cf, key, value) = (cf_index(cf), key_of(key), value_of(value));
        self.db()
            .put(CFS[cf], &key, &value)
            .map_err(|error| fail("put", &error))?;
        self.model.insert((cf, key), value);
        Ok(())
    }

    fn delete(&mut self, cf: u8, key: u8) -> Result<(), TestCaseError> {
        let (cf, key) = (cf_index(cf), key_of(key));
        self.db()
            .delete(CFS[cf], &key)
            .map_err(|error| fail("delete", &error))?;
        self.model.remove(&(cf, key));
        Ok(())
    }

    fn get(&self, cf: u8, key: u8) -> Result<(), TestCaseError> {
        let (cf, key) = (cf_index(cf), key_of(key));
        let found = self
            .db()
            .get(CFS[cf], &key, &ReadOptions::default())
            .map_err(|error| fail("get", &error))?;
        prop_assert_eq!(
            found.as_deref(),
            self.model.get(&(cf, key.clone())).map(Vec::as_slice),
            "get({}, {:?})",
            CFS[cf],
            String::from_utf8_lossy(&key)
        );
        Ok(())
    }

    /// One batch, several families, all or nothing.
    ///
    /// The model is only updated once the write has been acknowledged, so a batch the engine
    /// rejects leaves it untouched — which is what makes atomicity checkable rather than
    /// assumed.
    fn batch(&mut self, mutations: &[Mutation]) -> Result<(), TestCaseError> {
        let mut batch = WriteBatch::new();
        let mut staged = Vec::new();
        for mutation in mutations {
            match mutation {
                Mutation::Put(cf, key, value) => {
                    let (cf, key, value) = (cf_index(*cf), key_of(*key), value_of(*value));
                    let id = self.db().cf_id(CFS[cf]).expect("a built-in family");
                    batch.put(id, &key, &value);
                    staged.push((cf, key, Some(value)));
                }
                Mutation::Delete(cf, key) => {
                    let (cf, key) = (cf_index(*cf), key_of(*key));
                    let id = self.db().cf_id(CFS[cf]).expect("a built-in family");
                    batch.delete(id, &key);
                    staged.push((cf, key, None));
                }
            }
        }
        self.db()
            .write(batch, &WriteOptions::default())
            .map_err(|error| fail("write batch", &error))?;
        for (cf, key, value) in staged {
            match value {
                Some(value) => self.model.insert((cf, key), value),
                None => self.model.remove(&(cf, key)),
            };
        }
        Ok(())
    }

    fn bounded_scan(&self, cf: u8, lo: u8, hi: u8, reverse: bool) -> Result<(), TestCaseError> {
        let (cf, lo, hi) = (cf_index(cf), key_of(lo), key_of(hi));
        let got = self.scan(cf, &lo, &hi, reverse, &ReadOptions::default())?;
        let expected: Entries = Self::expected(&self.model, cf)
            .into_iter()
            .filter(|(key, _)| key[..] >= lo[..] && key[..] <= hi[..])
            .collect();
        prop_assert_eq!(
            got,
            expected,
            "scan({}, {:?}..={:?}, reverse={})",
            CFS[cf],
            String::from_utf8_lossy(&lo),
            String::from_utf8_lossy(&hi),
            reverse
        );
        Ok(())
    }

    fn snapshot_read(&self, slot: u8, cf: u8, key: u8) -> Result<(), TestCaseError> {
        if self.snapshots.is_empty() {
            return Ok(());
        }
        let slot = usize::from(slot) % self.snapshots.len();
        let (cf, key) = (cf_index(cf), key_of(key));
        let (snapshot, model) = &self.snapshots[slot];
        let options = ReadOptions {
            snapshot: Some(snapshot.clone()),
            ..ReadOptions::default()
        };
        let found = self
            .db()
            .get(CFS[cf], &key, &options)
            .map_err(|error| fail("snapshot get", &error))?;
        prop_assert_eq!(
            found.as_deref(),
            model.get(&(cf, key.clone())).map(Vec::as_slice),
            "snapshot {} get({}, {:?})",
            slot,
            CFS[cf],
            String::from_utf8_lossy(&key)
        );
        Ok(())
    }

    fn snapshot_scan(&self, slot: u8, cf: u8) -> Result<(), TestCaseError> {
        if self.snapshots.is_empty() {
            return Ok(());
        }
        let slot = usize::from(slot) % self.snapshots.len();
        let cf = cf_index(cf);
        let options = ReadOptions {
            snapshot: Some(self.snapshots[slot].0.clone()),
            ..ReadOptions::default()
        };
        let got = self.scan(cf, b"", LAST_KEY, false, &options)?;
        let expected = Self::expected(&self.snapshots[slot].1, cf);
        prop_assert_eq!(got, expected, "snapshot {} scan({})", slot, CFS[cf]);
        Ok(())
    }

    /// A flush moves data, it does not change it — including through snapshots taken before
    /// it, whose versions it must not have collected.
    fn flush(&self, cf: u8) -> Result<(), TestCaseError> {
        let cf = cf_index(cf);
        self.db()
            .flush(CFS[cf])
            .map_err(|error| fail("flush", &error))?;
        self.verify()
    }

    /// Closes the database and opens it again on the same filesystem.
    ///
    /// Snapshots do not survive: the model drops its clones, and the engine's handles go with
    /// the database that issued them. See the module docs.
    fn reopen(&mut self) -> Result<(), TestCaseError> {
        self.snapshots.clear();
        drop(self.db.take());

        let db = Db::open_with(DIR, Options::default(), Arc::clone(&self.fs), &CFS)
            .map_err(|error| fail("reopening", &error))?;
        self.db = Some(db);
        // Everything acknowledged before the close has to be here, which is the recovery
        // property the crash tests check under damage and this one checks under none.
        self.verify()
    }

    /// Checks the whole database against the whole model, and every snapshot against its clone.
    fn verify(&self) -> Result<(), TestCaseError> {
        for (cf, name) in CFS.iter().enumerate() {
            let expected = Self::expected(&self.model, cf);

            let forward = self.scan(cf, b"", LAST_KEY, false, &ReadOptions::default())?;
            prop_assert_eq!(&forward, &expected, "full forward scan of {}", name);

            let backward = self.scan(cf, b"", LAST_KEY, true, &ReadOptions::default())?;
            prop_assert_eq!(&backward, &expected, "full reverse scan of {}", name);

            // Point lookups as well as scans: they take different paths through the read side.
            for raw in 0..KEYS {
                let key = key_of(raw);
                let found = self
                    .db()
                    .get(name, &key, &ReadOptions::default())
                    .map_err(|error| fail("get during verification", &error))?;
                prop_assert_eq!(
                    found.as_deref(),
                    self.model.get(&(cf, key.clone())).map(Vec::as_slice),
                    "get({}, {:?}) during verification",
                    name,
                    String::from_utf8_lossy(&key)
                );
            }
        }

        for (index, (snapshot, model)) in self.snapshots.iter().enumerate() {
            let options = ReadOptions {
                snapshot: Some(snapshot.clone()),
                ..ReadOptions::default()
            };
            for (cf, name) in CFS.iter().enumerate() {
                let got = self.scan(cf, b"", LAST_KEY, false, &options)?;
                prop_assert_eq!(
                    got,
                    Self::expected(model, cf),
                    "snapshot {} still sees {} as it was",
                    index,
                    name
                );
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// The properties
// ---------------------------------------------------------------------------------------

fn run_sequence(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut world = World::new()?;
    for op in ops {
        world.apply(op)?;
    }
    world.verify()
}

proptest! {
    #![proptest_config(config(CASES))]

    /// Random sequences of every operation the engine has, against the map that defines them.
    #[test]
    fn a_database_is_a_btreemap(ops in prop::collection::vec(op(), 1..40)) {
        run_sequence(&ops)?;
    }
}

/// The acceptance run: ten thousand cases, as `prompts/01-engine.md` asks for.
#[test]
#[ignore = "the 10,000-case acceptance run; minutes, not seconds"]
fn a_database_is_a_btreemap_acceptance_run() {
    let mut runner = proptest::test_runner::TestRunner::new(config(CASES_IGNORED));
    runner
        .run(&prop::collection::vec(op(), 1..40), |ops| {
            run_sequence(&ops)
        })
        .expect("the model held over ten thousand sequences");
}

// ---------------------------------------------------------------------------------------
// Two semantics this file depends on, pinned
// ---------------------------------------------------------------------------------------

/// **A snapshot from a closed database is refused, not silently believed.**
///
/// A [`Snapshot`] is a sequence number plus a handle on the issuing database's snapshot list,
/// and sequence numbers survive a reopen — so a stale handle names a number that still means
/// something in the reopened database. Believing it is the most misleading of the available
/// behaviours: the reopened list has never heard of that handle, so the floor it hands
/// compaction can sit above the number the handle claims, and the versions it pinned are
/// collected while it still holds them.
///
/// The engine therefore stamps every list with an instance id, every snapshot remembers its
/// own, and the read paths compare them. A snapshot's whole promise is that what it saw stays
/// readable, and only the database that issued it can keep that promise.
#[test]
fn a_snapshot_taken_before_a_reopen_is_refused() {
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let options = || Options {
        create_if_missing: true,
        ..Options::default()
    };

    let stale = {
        let db = Db::open_with(DIR, options(), Arc::clone(&fs), &CFS).unwrap();
        db.put(cf::DEFAULT, b"k", b"first").unwrap();
        let snapshot = db.snapshot();
        db.put(cf::DEFAULT, b"k", b"second").unwrap();
        snapshot
    };

    let db = Db::open_with(DIR, Options::default(), Arc::clone(&fs), &CFS).unwrap();
    let read_options = ReadOptions {
        snapshot: Some(stale.clone()),
        ..ReadOptions::default()
    };

    let error = db
        .get(cf::DEFAULT, b"k", &read_options)
        .expect_err("a stale snapshot must be refused, not used as a bare sequence number");
    let message = error.to_string();
    assert!(message.contains("database instance"), "{message}");
    assert!(message.contains("do not survive a reopen"), "{message}");

    assert!(
        db.iter(cf::DEFAULT, &read_options).is_err(),
        "an iterator must refuse it too, or a scan becomes the way around the check"
    );

    // A snapshot from *this* database still works, and the database itself is unaffected.
    let live = db.snapshot();
    assert_eq!(
        db.get(
            cf::DEFAULT,
            b"k",
            &ReadOptions {
                snapshot: Some(live),
                ..ReadOptions::default()
            }
        )
        .unwrap()
        .as_deref(),
        Some(&b"second"[..])
    );
    assert_eq!(
        db.get(cf::DEFAULT, b"k", &ReadOptions::default())
            .unwrap()
            .as_deref(),
        Some(&b"second"[..]),
        "the reopened database lost the newer write"
    );

    // The stale handle still holds a sequence number in *its* list, which is now nobody's:
    // dropping it is all that is left to do with it.
    assert_eq!(stale.seqno(), 1);
    assert_ne!(
        stale.instance(),
        db.property("esker.instance").unwrap().parse().unwrap()
    );
}

/// The tripwire for the one operation `prompts/01-engine.md` lists that is not modelled.
///
/// `Op` has no `Compact` variant because `Db` has no compaction API. This reads the crate's own
/// source rather than the type system, because the absence of a method is not something a test
/// can otherwise notice — and the point is to fail loudly the day it appears, so that wiring
/// compaction into the model is not left to memory.
#[test]
fn compaction_still_has_no_public_api() {
    let db_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/db");
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&db_dir).expect("src/db is readable") {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a source file");
        if text.contains("pub fn compact") {
            found.push(path.file_name().map(std::ffi::OsStr::to_os_string));
        }
    }
    assert!(
        found.is_empty(),
        "compaction has a public API now ({found:?}). Add a `Compact` op to tests/model.rs \
         beside `Flush` — same shape, same assertion: the data must be identical through every \
         live snapshot and after a reopen — then delete this test."
    );
}
