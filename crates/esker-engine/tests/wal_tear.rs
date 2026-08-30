//! What a half-written log record costs, and what it must not.
//!
//! A `write` to a file can fail after writing some of its bytes — a full disk and a partial
//! write look identical from inside `append`. The record left behind is torn, and the write
//! is correctly not acknowledged. The danger is everything **after** it: if the log keeps
//! taking records, the next one is laid over the tail of the half-written one, which then has
//! a valid header, plausible bytes and a failing checksum. Recovery meets corruption in the
//! middle of a log rather than a torn record at its end, and every acknowledged write past the
//! tear is gone.
//!
//! So a failed append ends the segment: every later write on it fails, nothing more is
//! acknowledged, and the tear stays where recovery already knows how to handle it — at the
//! tail (`docs/DESIGN.md` §4.3).
//!
//! These are the regression tests for that, built on the crash lane's fault injector.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_engine::batch::WriteBatch;
use esker_engine::filename;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions, WriteOptions};
use esker_engine::testing::{Fault, FaultFileSystem, FaultPlan};
use esker_engine::{Db, cf};

const DIR: &str = "/db";

fn options() -> Options {
    Options {
        create_if_missing: true,
        ..Options::default()
    }
}

fn key_for(op: u32) -> Vec<u8> {
    format!("key-{op:04}").into_bytes()
}

fn value_for(op: u32) -> Vec<u8> {
    format!("value-{op:04}").into_bytes()
}

/// One run under a fault plan, and what it managed to acknowledge.
struct Outcome {
    inner: Arc<MemFileSystem>,
    faulty: FaultFileSystem,
    /// Operations that returned `Ok` from `write`.
    acked: Vec<u32>,
    /// How many writes were attempted.
    attempted: u32,
}

impl Outcome {
    /// The operation index of the first torn append, if there was one.
    fn first_tear(&self) -> Option<u64> {
        self.faulty
            .faults()
            .iter()
            .filter(|record| matches!(record.fault, Fault::ShortAppend { .. }))
            .map(|record| record.op)
            .min()
    }

    /// Whether the database was ever created: `CURRENT` is the last step of creating one.
    fn database_exists(&self) -> bool {
        self.inner
            .exists(&filename::current(std::path::Path::new(DIR)))
            .unwrap_or(false)
    }
}

fn run(plan: FaultPlan, ops: u32) -> Outcome {
    let inner = Arc::new(MemFileSystem::new());
    let backing: Arc<dyn FileSystem> = inner.clone();
    let faulty = FaultFileSystem::new(backing, plan);
    let fs: Arc<dyn FileSystem> = Arc::new(faulty.clone());

    let mut acked = Vec::new();
    if let Ok(db) = Db::open_with(DIR, options(), fs, &[cf::DEFAULT]) {
        let id = db.cf_id(cf::DEFAULT).unwrap();
        for op in 0..ops {
            let mut batch = WriteBatch::new();
            batch.put(id, &key_for(op), &value_for(op));
            if db.write(batch, &WriteOptions::synced()).is_ok() {
                acked.push(op);
            }
        }
        drop(db);
    }
    Outcome {
        inner,
        faulty,
        acked,
        attempted: ops,
    }
}

/// How many filesystem operations a clean run performs, so a cut can be aimed at one.
fn operation_count(seed: u64, ops: u32) -> u64 {
    run(FaultPlan::none(seed), ops).faulty.operations()
}

fn reopen(outcome: &Outcome) -> esker_engine::error::Result<Db> {
    let fs: Arc<dyn FileSystem> = outcome.inner.clone();
    Db::open_with(
        DIR,
        Options {
            create_if_missing: false,
            ..Options::default()
        },
        fs,
        &[cf::DEFAULT],
    )
}

/// Every acknowledged write is readable, and nothing readable is wrong.
fn check_acked_are_readable(db: &Db, outcome: &Outcome, context: &str) {
    for op in 0..outcome.attempted {
        let found = db
            .get(cf::DEFAULT, &key_for(op), &ReadOptions::default())
            .unwrap_or_else(|error| panic!("{context}: reading op {op} failed: {error}"));
        if outcome.acked.contains(&op) {
            assert_eq!(
                found.as_deref(),
                Some(&value_for(op)[..]),
                "{context}: op {op} was acknowledged and is missing"
            );
        } else if let Some(found) = found {
            assert_eq!(
                &found[..],
                &value_for(op)[..],
                "{context}: op {op} came back as something nobody wrote"
            );
        }
    }
}

/// The repro the crash lane reduced this bug to: seed 5, six writes, one torn append.
///
/// Before the fix, write 1's append wrote a prefix and failed — correctly unacknowledged —
/// and writes 2 to 5 were then laid over the tear and acknowledged, only to be unreadable
/// afterwards. Now the tear ends the segment and nothing after it is acknowledged.
#[test]
fn a_torn_append_stops_the_log_taking_writes() {
    let seed = 5;
    let plan = FaultPlan::none(seed).with_short_appends(0.15);
    let outcome = run(plan, 6);

    // The shape this repro depends on, asserted rather than assumed: if the injector's
    // schedule ever changes, this says so instead of quietly testing nothing.
    let tear = outcome
        .first_tear()
        .expect("this schedule is supposed to tear exactly one append");
    let tears = outcome
        .faulty
        .faults()
        .iter()
        .filter(|record| matches!(record.fault, Fault::ShortAppend { .. }))
        .count();
    assert_eq!(tears, 1, "expected exactly one torn append");

    // Before the fix this was `[0, 2, 3, 4, 5]`: five of six acknowledged, four of them over
    // the tear and unreadable afterwards. Only the write that finished before the tear may be
    // acknowledged now.
    assert_eq!(
        outcome.acked,
        vec![0],
        "a write was acknowledged after the tear at operation {tear}"
    );

    let db = reopen(&outcome).expect("a torn tail is what recovery is for");
    check_acked_are_readable(&db, &outcome, "seed 5");
}

/// The sweep: sixty seeds, three cut points each, short appends throughout.
///
/// Wherever the database reopens, every acknowledged write is there. Where it does not, the
/// power cut landed before the database existed — and then nothing was acknowledged, so
/// nothing was lost. Both halves are asserted, because "it did not open" is only acceptable
/// for the second reason.
#[test]
fn a_torn_append_never_costs_an_acknowledged_write() {
    let (mut torn, mut never_created) = (0usize, 0usize);
    for seed in 0..60u64 {
        let total = operation_count(seed, 24);
        for cut_at in [total / 3, total / 2, total - 1] {
            let plan = FaultPlan::power_cut(seed, cut_at).with_short_appends(0.35);
            let outcome = run(plan, 24);
            if outcome.first_tear().is_none() {
                continue;
            }
            torn += 1;
            let context = format!("seed {seed}, cut at {cut_at}");
            match reopen(&outcome) {
                Ok(db) => check_acked_are_readable(&db, &outcome, &context),
                Err(error) => {
                    never_created += 1;
                    assert!(
                        outcome.acked.is_empty(),
                        "{context}: reopen failed with {} acknowledged writes: {error}",
                        outcome.acked.len()
                    );
                    assert!(
                        !outcome.database_exists(),
                        "{context}: CURRENT exists but the database would not reopen: {error}"
                    );
                }
            }
        }
    }
    assert!(
        torn > 50,
        "only {torn} schedules tore a record; this sweep measured almost nothing"
    );
    println!(
        "torn appends: {torn} schedules tore a record, {never_created} were cut before the \
         database existed, 0 lost an acknowledged write"
    );
}

/// The other half of the sweep's count, named rather than left as a number.
///
/// A power cut a third of the way through a short run usually lands inside `Db::open`, which
/// performs most of a run's filesystem operations. `CURRENT` is written last, so the directory
/// holds a manifest and no pointer to it — which is not a database, and says so.
#[test]
fn a_cut_before_current_leaves_no_database_rather_than_a_broken_one() {
    let mut cut_before_current = 0usize;
    for seed in 0..20u64 {
        let total = operation_count(seed, 4);
        for cut_at in 0..total {
            let outcome = run(FaultPlan::power_cut(seed, cut_at), 4);
            if outcome.database_exists() {
                let db = reopen(&outcome)
                    .unwrap_or_else(|error| panic!("seed {seed} cut {cut_at}: {error}"));
                check_acked_are_readable(&db, &outcome, &format!("seed {seed} cut {cut_at}"));
            } else {
                cut_before_current += 1;
                assert!(
                    outcome.acked.is_empty(),
                    "seed {seed} cut {cut_at}: writes were acknowledged into a database that \
                     does not exist"
                );
                assert!(reopen(&outcome).is_err());
            }
        }
    }
    assert!(
        cut_before_current > 0,
        "no schedule cut before CURRENT; this test measured nothing"
    );
}
