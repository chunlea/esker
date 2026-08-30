//! The in-process half of the phase-1 crash loop: kill the filesystem, not the process.
//!
//! `prompts/01-engine.md` requires a crash loop that kills the engine at a random moment and
//! checks that every acknowledged write survives. `tests/crash_kill.rs` does that with a real
//! `SIGKILL`; this one does it with [`FaultFileSystem`], and the two find different things.
//!
//! A subprocess kill is realistic but coarse: it lands wherever the scheduler puts it, so
//! covering every interesting instant takes thousands of iterations and luck. The fault
//! injector is the opposite — it kills at operation *N* for every *N* the run performs, so a
//! single sweep covers **every** point at which the engine could have died, exactly once, in
//! milliseconds. Between them: one says "this survives a real crash", the other says "this
//! survives a crash *anywhere*".
//!
//! # The two questions
//!
//! * **acked ⊆ readable.** A write whose `write()` returned `Ok` was acknowledged, and
//!   `CLAUDE.md` invariant 1 says its bytes were durable before that happened. Losing one is
//!   the bug this whole test exists for.
//! * **readable ⊆ attempted, and correct.** Nothing may come back that was never written, and
//!   nothing may come back wrong. A write that was *not* acknowledged may or may not be there
//!   — that is what "crashed midway" means — but if it is there it must be byte-for-byte what
//!   was written.
//!
//! # Two durability models, because they are not the same crash
//!
//! `src/fs.rs` is explicit that v1 targets process-crash durability. So each schedule is
//! checked twice: once reopening the filesystem exactly as the crash left it (`kill -9`, where
//! unsynced bytes are still in the page cache and survive), and once after
//! [`MemFileSystem::lose_unsynced`] (a power loss, where they do not). Every write here is
//! `sync = true`, so both must hold — and if only the first does, the engine is acknowledging
//! before it is durable.
//!
//! The schedule is a pure function of `(seed, operation index)`, so the run is performed twice
//! rather than snapshotted, and the second run is identical to the first by construction.
//!
//! # What is blocked
//!
//! The clean-truncation half of this file passes at full coverage. The *torn record* half does
//! not: at the commit this was written against, a failed log append left a prefix on disk and
//! the engine kept appending after it, so recovery met a broken record that was not at the
//! tail and refused the database — losing writes it had acknowledged. See
//! [`writes_after_a_torn_record_are_acknowledged_and_then_lost`], which is ignored until the
//! spine gives the log an error state, and
//! [`a_torn_append_never_produces_wrong_data`], which asserts only what must hold either way.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_base::rng::Pcg32;
use esker_engine::batch::WriteBatch;
use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{Options, ReadOptions, WriteOptions};
use esker_engine::testing::{Fault, FaultFileSystem, FaultPlan};
use esker_engine::{Db, cf};

const DIR: &str = "/db";

/// Writes per schedule. Small on purpose: the sweep's coverage comes from the number of
/// schedules, not from the length of any one of them.
const OPS: u32 = 24;

/// The key of operation `op`. Fixed width so the order is the operation order.
fn key_for(op: u32) -> Vec<u8> {
    format!("key-{op:06}").into_bytes()
}

/// The value of operation `op` under `seed`.
///
/// Length and contents both vary, so a record recovered from the wrong place in the log looks
/// wrong rather than plausible, and the first four bytes carry the operation index so that a
/// value found under the wrong key names the write it really came from.
fn value_for(seed: u64, op: u32) -> Vec<u8> {
    let mut rng = Pcg32::new(seed, u64::from(op));
    let len = 8 + usize::try_from(rng.below(120)).unwrap_or(0);
    let mut value = vec![0u8; len];
    rng.fill_bytes(&mut value);
    value[..4].copy_from_slice(&op.to_le_bytes());
    value
}

fn options() -> Options {
    Options {
        create_if_missing: true,
        ..Options::default()
    }
}

/// One write, and where it sat in the injector's schedule.
struct Attempt {
    acked: bool,
    /// The injector's operation index when this write began, so a fault's recorded index can
    /// be placed before or after it.
    started_at: u64,
}

/// What one schedule did.
struct Run {
    inner: Arc<MemFileSystem>,
    faulty: FaultFileSystem,
    /// Operations whose `write()` returned `Ok`.
    acked: Vec<u32>,
    /// Every write tried, in order.
    attempts: Vec<Attempt>,
    /// Operations that were tried at all.
    attempted: u32,
    /// The database could not even be opened, so nothing was written.
    open_failed: bool,
}

impl Run {
    /// The injector's operation index of the first torn append, if one happened.
    fn first_tear(&self) -> Option<u64> {
        self.faulty
            .faults()
            .iter()
            .find(|record| matches!(record.fault, Fault::ShortAppend { .. }))
            .map(|record| record.op)
    }

    /// Whether any write was acknowledged *after* a record was left half-written.
    ///
    /// This is the shape that `writes_after_a_torn_record_are_acknowledged_and_then_lost`
    /// documents: those acknowledgements went into a log that was already broken behind them.
    fn acknowledged_after_a_tear(&self) -> bool {
        let Some(tear) = self.first_tear() else {
            return false;
        };
        self.attempts
            .iter()
            .any(|attempt| attempt.acked && attempt.started_at > tear)
    }
}

impl Run {
    /// A one-line description for a failure message: everything needed to replay it.
    fn describe(&self, seed: u64, plan: &FaultPlan) -> String {
        format!(
            "seed {seed}, cut at {:?}, short_append {}, {} acked of {} attempted, \
             {} faults injected, powered_off {}",
            plan.power_cut_at,
            plan.short_append,
            self.acked.len(),
            self.attempted,
            self.faulty.faults().len(),
            self.faulty.powered_off()
        )
    }
}

/// Opens a database on a faulty filesystem, writes the pattern, and drops it.
fn run(seed: u64, plan: FaultPlan) -> Run {
    let inner = Arc::new(MemFileSystem::new());
    let backing: Arc<dyn FileSystem> = inner.clone();
    let faulty = FaultFileSystem::new(backing, plan);
    let fs: Arc<dyn FileSystem> = Arc::new(faulty.clone());

    let mut acked = Vec::new();
    let mut attempts = Vec::new();
    let mut attempted = 0;
    let mut open_failed = false;

    match Db::open_with(DIR, options(), fs, &[cf::DEFAULT]) {
        Err(_) => open_failed = true,
        Ok(db) => {
            let id = db.cf_id(cf::DEFAULT).expect("the default family exists");
            for op in 0..OPS {
                attempted = op + 1;
                let started_at = faulty.operations();
                let mut batch = WriteBatch::new();
                batch.put(id, &key_for(op), &value_for(seed, op));
                let ok = db.write(batch, &WriteOptions::synced()).is_ok();
                attempts.push(Attempt {
                    acked: ok,
                    started_at,
                });
                if ok {
                    acked.push(op);
                }
            }
            // Dropping under a cut filesystem is itself part of the test: whatever the engine
            // does on the way out, it cannot make things worse than the crash already did.
            drop(db);
        }
    }

    Run {
        inner,
        faulty,
        acked,
        attempts,
        attempted,
        open_failed,
    }
}

/// Reopens the database on the *real* filesystem state the crash left behind.
///
/// Separate from [`check`] because opening is not free of side effects — it starts a new log
/// segment and writes a manifest edit — so a caller that wants to know *whether* recovery
/// works must not open once to find out and again to look.
fn reopen(run: &Run) -> esker_engine::error::Result<Db> {
    let fs: Arc<dyn FileSystem> = run.inner.clone();
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

/// Checks both directions of the contract against a recovered database.
fn check(db: &Db, run: &Run, seed: u64, plan: &FaultPlan, model: &str) {
    let context = run.describe(seed, plan);

    for op in 0..run.attempted {
        let found = db
            .get(cf::DEFAULT, &key_for(op), &ReadOptions::default())
            .unwrap_or_else(|error| panic!("{model}: reading op {op} failed ({context}): {error}"));
        match found {
            Some(value) => assert_eq!(
                value.as_ref(),
                &value_for(seed, op)[..],
                "{model}: op {op} read back bytes that were never written ({context})"
            ),
            None => assert!(
                !run.acked.contains(&op),
                "{model}: acknowledged write {op} was lost ({context})"
            ),
        }
    }

    // Nothing may exist beyond what was attempted.
    for op in run.attempted..run.attempted + 4 {
        let found = db
            .get(cf::DEFAULT, &key_for(op), &ReadOptions::default())
            .unwrap();
        assert!(
            found.is_none(),
            "{model}: op {op} is readable but was never written ({context})"
        );
    }
}

/// Reopens and checks, requiring the reopen to succeed whenever anything was acknowledged.
fn verify(run: &Run, seed: u64, plan: &FaultPlan, model: &str) {
    let context = run.describe(seed, plan);
    match reopen(run) {
        Ok(db) => check(&db, run, seed, plan, model),
        // A crash before the database existed leaves nothing to reopen. That is only
        // acceptable if nothing was ever acknowledged.
        Err(error) if run.acked.is_empty() => assert!(
            run.open_failed || run.faulty.powered_off(),
            "{model}: reopen failed on a database that opened cleanly ({context}): {error}"
        ),
        Err(error) => panic!(
            "{model}: reopen failed with {} acknowledged writes ({context}): {error}",
            run.acked.len()
        ),
    }
}

/// Runs one schedule and checks it under both durability models.
///
/// The run is performed twice rather than snapshotted: the injector's schedule is a pure
/// function of `(seed, operation index)`, so the second run reaches the same state as the
/// first, and that this is true is itself asserted.
fn check_schedule(seed: u64, plan: FaultPlan) {
    let first = run(seed, plan);
    verify(&first, seed, &plan, "kill -9");

    let second = run(seed, plan);
    assert_eq!(
        first.acked,
        second.acked,
        "the same seed acknowledged different writes on a second run ({})",
        first.describe(seed, &plan)
    );
    assert_eq!(first.faulty.faults(), second.faulty.faults());
    second.inner.lose_unsynced().unwrap();
    verify(&second, seed, &plan, "power loss");
}

/// How many filesystem operations a clean run performs, so the sweep can cover all of them.
fn operation_count(seed: u64) -> u64 {
    run(seed, FaultPlan::none(seed)).faulty.operations()
}

/// A clean run must not be disturbed by the injector being in the path at all, and must
/// acknowledge everything.
#[test]
fn a_run_with_no_faults_keeps_everything() {
    let seed = 1;
    let outcome = run(seed, FaultPlan::none(seed));
    assert!(!outcome.open_failed);
    assert_eq!(
        outcome.acked.len(),
        OPS as usize,
        "a clean run lost a write"
    );
    assert!(
        outcome.faulty.faults().is_empty(),
        "the injector injected something into a plan with nothing in it"
    );
    verify(&outcome, seed, &FaultPlan::none(seed), "no faults");

    // And after a power loss, since every write was synced.
    let second = run(seed, FaultPlan::none(seed));
    second.inner.lose_unsynced().unwrap();
    verify(
        &second,
        seed,
        &FaultPlan::none(seed),
        "no faults, power loss",
    );
}

/// **The sweep.** Cut the power at every single operation the run performs, for several
/// seeds, and check both durability models each time.
///
/// This is the test the whole file exists for. A subprocess kill loop would need thousands of
/// iterations and good luck to land on the instant between a log append and its `fsync`; this
/// lands on it, and on every other instant, by construction.
#[test]
fn cutting_the_power_at_every_operation() {
    let mut schedules = 0usize;
    for seed in [1u64, 2, 3, 0xDEAD_BEEF] {
        let total = operation_count(seed);
        assert!(
            total > 20,
            "a {OPS}-write run performed only {total} operations"
        );
        for cut_at in 0..total {
            check_schedule(seed, FaultPlan::power_cut(seed, cut_at));
            schedules += 1;
        }
    }
    assert!(
        schedules >= 200,
        "only {schedules} schedules were swept; the coverage this test claims is gone"
    );
}

/// A torn append may cost the database, but it may never produce wrong data.
///
/// A short append leaves a record half on disk. Whether the engine can recover from that is
/// the subject of [`writes_after_a_torn_record_are_acknowledged_and_then_lost`] and is not
/// asserted here — recovery either works or it does not, and a test that *required* it to
/// fail would go red the day it starts working. What is asserted is the part that must hold
/// in every world: **wherever the database does reopen, the contract holds.** Nothing
/// acknowledged is missing, and nothing readable is wrong.
///
/// The two counts are printed rather than asserted, because they are the size of a bug and
/// bugs are supposed to shrink. Run with `--nocapture` to see them.
#[test]
fn a_torn_append_never_produces_wrong_data() {
    let (mut torn, mut unrecoverable, mut acked_after_tear) = (0usize, 0usize, 0usize);
    for seed in 0..60u64 {
        let total = operation_count(seed);
        for cut_at in [total / 3, total / 2, total - 1] {
            let plan = FaultPlan::power_cut(seed, cut_at).with_short_appends(0.35);
            let outcome = run(seed, plan);
            if outcome.first_tear().is_none() {
                continue;
            }
            torn += 1;
            if outcome.acknowledged_after_a_tear() {
                acked_after_tear += 1;
            }
            match reopen(&outcome) {
                Err(_) => unrecoverable += 1,
                Ok(db) => check(&db, &outcome, seed, &plan, "torn append"),
            }
        }
    }

    assert!(
        torn > 50,
        "only {torn} schedules tore a record; this sweep measured almost nothing"
    );
    println!(
        "torn appends: {torn} schedules tore a record, {unrecoverable} left the database \
         unopenable, {acked_after_tear} acknowledged a write after the tear"
    );
}

/// **Known engine bug — the log keeps taking writes after an append fails.**
///
/// Seed 5, six writes, one torn append. Write 1's append writes a prefix and then fails, so
/// `write()` returns an error and it is correctly *not* acknowledged. Writes 2 to 5 then
/// append after the half-written bytes and are acknowledged. Reopening the database fails
/// outright — `corruption in 000002.wal: checksum mismatch` — and with `paranoid_checks` off
/// it opens but only write 0 is readable: **five acknowledged writes, silently gone.**
///
/// That is `CLAUDE.md` invariant 1: a write acknowledged as durable was not. The engine has no
/// state that says "this log is broken now"; `DbInner::commit_group` returns the error to one
/// caller and the next group calls `add_record` again on the same writer, at the offset after
/// the partial bytes. `LevelDB` and `RocksDB` both answer this with a permanent background
/// error that fails every later write.
///
/// Ignored until the spine fixes it. It is a repro, not a regression test yet: un-ignore it
/// with the fix.
#[test]
#[ignore = "engine bug: a failed WAL append does not stop later writes being acknowledged"]
fn writes_after_a_torn_record_are_acknowledged_and_then_lost() {
    let seed = 5;
    let plan = FaultPlan::none(seed).with_short_appends(0.15);
    let outcome = run(seed, plan);

    // The shape this repro depends on, asserted rather than assumed — if the schedule ever
    // changes, this says so instead of quietly testing nothing.
    let tears: Vec<u64> = outcome
        .faulty
        .faults()
        .iter()
        .filter(|record| matches!(record.fault, Fault::ShortAppend { .. }))
        .map(|record| record.op)
        .collect();
    assert_eq!(
        tears.len(),
        1,
        "expected exactly one torn append, got {tears:?}"
    );
    assert!(
        outcome.acknowledged_after_a_tear(),
        "no write was acknowledged after the tear, so there is nothing to lose"
    );
    assert!(
        outcome.acked.len() >= 4,
        "only {} writes were acknowledged",
        outcome.acked.len()
    );

    // What must be true, and is not.
    verify(&outcome, seed, &plan, "after a torn record");
}

/// A write that the filesystem refused must not be acknowledged, whatever else happens. If a
/// failed `fsync` could still return `Ok` from `write()`, invariant 1 would be a comment.
#[test]
fn a_failed_sync_is_never_acknowledged() {
    let mut refused = 0usize;
    for seed in 0..24u64 {
        let plan = FaultPlan::none(seed).with_failed_syncs(0.4);
        let outcome = run(seed, plan);

        let failed_syncs = outcome
            .faulty
            .faults()
            .iter()
            .filter(|record| {
                matches!(record.fault, Fault::Failed)
                    && matches!(
                        record.operation,
                        esker_engine::testing::Operation::SyncData(_)
                    )
            })
            .count();
        refused += failed_syncs;

        // Every synced write that was acknowledged had a successful `sync_data` behind it, so
        // the count of acknowledgements plus the count of refused syncs cannot exceed what was
        // attempted.
        assert!(
            outcome.acked.len() + failed_syncs >= outcome.attempted.try_into().unwrap_or(0),
            "{} acked and {failed_syncs} syncs refused does not account for {} attempts ({})",
            outcome.acked.len(),
            outcome.attempted,
            outcome.describe(seed, &plan)
        );
        verify(&outcome, seed, &plan, "failed syncs");
    }
    assert!(
        refused > 30,
        "only {refused} syncs were refused across the sweep"
    );
}
