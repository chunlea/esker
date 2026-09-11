//! A crash in the middle of the rewrite that reaching the bottom level now costs.
//!
//! Arriving at the last level used to be a **trivial move**: one manifest edit, no bytes read
//! and none written. That is why a column family with no compaction filter never dropped a
//! tombstone (#62) — and rewriting there, which is the fix, is also a crash surface that path
//! did not have before. Outputs are written and synced, and only then does the pointer move.
//!
//! `docs/DESIGN.md` §11 requires the engine's crash test to be a kill at every instant with
//! recovery checked against a model. `crash_kill.rs` does that with a real `SIGKILL` and
//! `crash_faultfs.rs` with the injector, and **neither of them compacts** — the pattern both
//! write is a log, and a log never reaches a compaction. So this file is the same sweep aimed
//! at the one path the fix changed: cut the power at every operation the compaction performs,
//! and reopen on whatever the crash left behind.
//!
//! # What must hold after the cut, at every one of them
//!
//! Invariant 3 says the manifest pointer is the only mutable thing on disk, so the database
//! either still names the inputs or already names the output, never both and never neither.
//! **Whichever it names, the same keys read back**: a compaction that drops only what nothing
//! can reach is one whose result is indistinguishable from its input, and a crash cannot make
//! that untrue at any point in between. Which side of the pointer switch the crash landed on is
//! deliberately not asserted — that is the engine's business, and pinning it would make this
//! test a description of the current implementation rather than of the contract.
//!
//! # Two durability models, for the same reason `crash_faultfs.rs` has two
//!
//! Each schedule is checked twice: once on the filesystem exactly as the crash left it (a
//! process crash, where unsynced bytes survive in the page cache) and once after
//! [`MemFileSystem::lose_unsynced`] (a power loss, where they do not). The second is the one
//! that matters here, because a compaction's whole safety argument is the order of its syncs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use esker_engine::batch::WriteBatch;
use esker_engine::filename::{FileKind, classify_path};
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions, WriteOptions};
use esker_engine::testing::{FaultFileSystem, FaultPlan};
use esker_engine::{Db, cf};

const DIR: &str = "/db";
const SEED: u64 = 0x62;

/// Keys written. Enough that the compaction has real work — several blocks of it — and few
/// enough that the sweep over every operation stays a second.
const KEYS: u32 = 60;

/// Every third key is deleted, so the compaction has tombstones to drop and values to keep, and
/// the two are interleaved rather than in separate halves.
fn is_deleted(key: u32) -> bool {
    key.is_multiple_of(3)
}

fn key_for(key: u32) -> Vec<u8> {
    format!("k{key:04}").into_bytes()
}

fn value_for(key: u32) -> Vec<u8> {
    format!("value-{key:04}").repeat(8).into_bytes()
}

fn options() -> Options {
    Options {
        create_if_missing: true,
        // One flush means one L0 file, which is below the automatic trigger: the only
        // compaction in the run is the one this test asks for, so the operation indices it
        // spans mean what they say.
        compaction_threads: 1,
        ..Options::default()
    }
}

/// What one schedule left on disk.
struct Crash {
    /// The filesystem underneath the injector: the bytes a crash actually left.
    inner: Arc<MemFileSystem>,
    /// The injector's operation indices the compaction spanned. Meaningful only for the run
    /// that cut nothing — the others stop partway through it.
    compaction: std::ops::Range<u64>,
    /// The tables the version named before the compaction and after it, for the same run.
    tables: (BTreeSet<u64>, BTreeSet<u64>),
}

/// Writes the workload, flushes it, and compacts it to the bottom, cutting the power at `cut`.
///
/// Every write is `sync = true`, so both durability models below are entitled to all of them.
fn run(cut: Option<u64>) -> Crash {
    let inner = Arc::new(MemFileSystem::new());
    let backing: Arc<dyn FileSystem> = inner.clone();
    let plan = match cut {
        Some(op) => FaultPlan::power_cut(SEED, op),
        None => FaultPlan::none(SEED),
    };
    let faulty = FaultFileSystem::new(backing, plan);
    let fs: Arc<dyn FileSystem> = Arc::new(faulty.clone());

    let mut compaction = 0..0;
    let mut tables = (BTreeSet::new(), BTreeSet::new());
    if let Ok(db) = Db::open_with(DIR, options(), fs, &[cf::DEFAULT]) {
        let id = db.cf_id(cf::DEFAULT).expect("the default family exists");
        for key in 0..KEYS {
            let mut batch = WriteBatch::new();
            batch.put(id, &key_for(key), &value_for(key));
            let _ = db.write(batch, &WriteOptions::synced());
        }
        for key in (0..KEYS).filter(|key| is_deleted(*key)) {
            let mut batch = WriteBatch::new();
            batch.delete(id, &key_for(key));
            let _ = db.write(batch, &WriteOptions::synced());
        }
        let _ = db.flush(cf::DEFAULT);
        tables.0 = db.file_locations().into_keys().collect();
        let began = faulty.operations();
        let _ = db.compact_range(cf::DEFAULT, None, None);
        compaction = began..faulty.operations();
        tables.1 = db.file_locations().into_keys().collect();
        // Dropping under a cut filesystem is part of the test: whatever the engine does on the
        // way out, it cannot make things worse than the crash already did.
        drop(db);
    }
    Crash {
        inner,
        compaction,
        tables,
    }
}

/// Reopens on the state the crash left, and checks that the live keys are exactly the live keys.
fn verify(crash: &Crash, model: &str, cut: u64) {
    let fs: Arc<dyn FileSystem> = crash.inner.clone();
    let db = Db::open_with(
        DIR,
        Options {
            create_if_missing: false,
            ..Options::default()
        },
        fs,
        &[cf::DEFAULT],
    )
    .unwrap_or_else(|error| panic!("{model}: reopening after a cut at {cut} failed: {error}"));

    for key in 0..KEYS {
        let found = db
            .get(cf::DEFAULT, &key_for(key), &ReadOptions::default())
            .unwrap_or_else(|error| {
                panic!("{model}: reading k{key:04} after {cut} failed: {error}")
            });
        if is_deleted(key) {
            assert_eq!(
                found, None,
                "{model}: k{key:04} came back from the dead after a cut at {cut}"
            );
        } else {
            assert_eq!(
                found.as_deref(),
                Some(&value_for(key)[..]),
                "{model}: k{key:04} was lost or changed by a cut at {cut}"
            );
        }
    }

    // And the half-written output of the interrupted compaction is not still sitting there.
    // The sweep at open is what removes it, and it is the same sweep that reclaims the object
    // of a tiered file — which a directory listing can never reach.
    let named: BTreeSet<u64> = db.file_locations().into_keys().collect();
    let on_disk: BTreeSet<u64> = crash
        .inner
        .list(std::path::Path::new(DIR))
        .unwrap()
        .into_iter()
        .filter_map(|path| match classify_path(&path) {
            Some(FileKind::Sst(number)) => Some(number),
            _ => None,
        })
        .collect();
    let orphans: Vec<u64> = on_disk.difference(&named).copied().collect();
    assert!(
        orphans.is_empty(),
        "{model}: opening after a cut at {cut} left the tables {orphans:?} that no version \
         names; the version names {named:?}"
    );
}

/// **The sweep.** Cut the power at every operation the compaction performs, and recover.
#[test]
fn cutting_the_power_at_every_step_of_the_rewrite() {
    let survey = run(None);
    let compaction = survey.compaction.clone();
    // **The denominator.** This test is about the crash surface of a *rewrite*, and a trivial
    // move — which is what arriving at the bottom used to get — has none: it performs a manifest
    // edit and no output at all. Counting the operations does not separate the two, because
    // stepping a file down six levels costs six edits; the file *number* does. If the table the
    // database ends up naming is the table the flush produced, nothing was rewritten and every
    // cut below lands in a manifest edit this file is not the place to test.
    let (before, after) = &survey.tables;
    assert!(
        !before.is_empty() && after.is_disjoint(before),
        "the compaction produced no new table — it named {before:?} before and {after:?} after, \
         so it moved the file rather than rewriting it and there is no rewrite here to crash"
    );
    verify(&survey, "no fault", 0);

    for cut in compaction.clone() {
        let crash = run(Some(cut));
        verify(&crash, "process crash", cut);

        let power_loss = run(Some(cut));
        power_loss.inner.lose_unsynced().unwrap();
        verify(&power_loss, "power loss", cut);
    }
}
