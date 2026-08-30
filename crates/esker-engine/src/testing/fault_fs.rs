//! A [`FileSystem`] that fails on purpose, deterministically.
//!
//! `docs/DESIGN.md` §11 asks the engine's crash tests to run "with fault injection into
//! `FileSystem` (partial writes, failed fsync, failed rename)". This is that injector. It
//! wraps any other filesystem — [`MemFileSystem`](crate::memfs::MemFileSystem) for speed,
//! [`LocalFileSystem`](crate::fs::LocalFileSystem) when a test wants a real disk — and decides,
//! per operation, whether to let it through.
//!
//! # The schedule is a pure function of `(seed, operation index)`
//!
//! Not of the wall clock, not of a hash map's iteration order, and not of one long random
//! stream that a second thread can shift. Operation *n* derives its own generator from the
//! seed and from *n*, so the answer to "does operation 47 fail?" is the same in every run, in
//! every process, and whatever else the schedule was asked before it. That is what makes a
//! failing seed worth printing: re-running it replays the same failures.
//!
//! Which operation *gets* index 47 is the caller's business. A single-threaded run — which is
//! what a crash test is — replays exactly. Concurrent callers race for indices, so a
//! concurrent failure reproduces the *schedule* but not necessarily the interleaving; the
//! recorded log says what actually happened either way.
//!
//! # What counts as an operation
//!
//! Only the ones that change something: `create`, `append`, `sync_data`, `rename`, `delete`,
//! `fsync_dir`, `create_dir_all`, `hard_link`. Reads are never counted and never faulted, so
//! that a test can cut the power and then reopen the database to see what survived — which is
//! the whole point of the exercise. A read that has to fail is a job for a corrupt file, and
//! `MemFileSystem::install` plants one of those.
//!
//! # The faults
//!
//! | Fault | What it models |
//! |---|---|
//! | [`Fault::ShortAppend`] | a write that reached the kernel in part and then failed |
//! | [`Fault::Failed`] | `fdatasync`, `rename` or a directory sync returning an error |
//! | [`Fault::DelayedRename`] | a rename whose directory entry is not yet durable |
//! | [`Fault::PowerCut`] | the machine is gone: this operation and every later one fails |
//!
//! [`Fault::DelayedRename`] is the subtle one, and the reason it is here. POSIX makes a rename
//! atomic but not durable: until the containing directory is synced, a crash can still show
//! the old name (`CLAUDE.md` invariant 3, `docs/DESIGN.md` §4.6). So a delayed rename is held
//! *pending* — the inner filesystem is not touched, and `list` therefore keeps showing the old
//! name on its own — and a successful [`FileSystem::fsync_dir`] on the containing directory is
//! what applies it. An engine that syncs the directory after renaming `CURRENT` is unaffected;
//! one that forgets loses the rename at [`FaultFileSystem::restart`]. That is the bug this
//! exists to catch, and a chaos generator that failed renames at random would not catch it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use esker_base::hash::mix64;
use esker_base::rng::Pcg32;

use crate::fs::{FileSystem, RandomAccessFile, WritableFile};

use super::plan::{Fault, FaultPlan, FaultRecord, Operation};

/// The generator for one operation index.
///
/// A fresh stream per index rather than one advancing stream, so that the schedule cannot
/// depend on how many operations came before or on which thread asked first. `mix64` on both
/// sides is what stops adjacent indices from producing correlated draws.
fn schedule_rng(seed: u64, op: u64) -> Pcg32 {
    Pcg32::from_seed(mix64(seed ^ mix64(op)))
}

#[derive(Debug, Default)]
struct State {
    next_op: u64,
    log: Vec<FaultRecord>,
    /// Renames reported as successful but not applied, oldest first.
    pending: Vec<(PathBuf, PathBuf)>,
    powered_off: bool,
}

#[derive(Debug)]
struct Shared {
    inner: Arc<dyn FileSystem>,
    plan: FaultPlan,
    state: Mutex<State>,
}

/// What [`Shared::begin`] decided about one operation.
enum Decision {
    /// Let it through.
    Proceed,
    /// Apply this fault instead.
    Inject(Fault),
}

impl Shared {
    /// Locks the state, treating a poisoned lock as a live one: a poisoned lock means a test
    /// thread panicked, and a second panic from in here would bury the first one's message.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the next operation index and decides what to do with `operation`, recording the
    /// decision if it is a fault.
    fn begin(&self, operation: &Operation) -> Decision {
        let mut state = self.state();
        let op = state.next_op;
        state.next_op += 1;

        let cut = self.plan.power_cut_at.is_some_and(|at| op >= at);
        if cut || state.powered_off {
            state.powered_off = true;
            state.log.push(FaultRecord {
                op,
                operation: operation.clone(),
                fault: Fault::PowerCut,
            });
            return Decision::Inject(Fault::PowerCut);
        }

        let probability = operation.probability(&self.plan);
        if probability <= 0.0 {
            return Decision::Proceed;
        }
        let mut rng = schedule_rng(self.plan.seed, op);
        if !rng.chance(probability) {
            return Decision::Proceed;
        }

        let fault = match operation {
            Operation::Append(_, 0) => return Decision::Proceed, // nothing to shorten
            Operation::Append(_, requested) => {
                // At least one byte short, and possibly none written at all.
                let bound = u32::try_from(*requested).unwrap_or(u32::MAX);
                let wrote = usize::try_from(rng.below(bound)).unwrap_or(0);
                Fault::ShortAppend {
                    wrote,
                    requested: *requested,
                }
            }
            Operation::Rename(..) => {
                // The two rename faults share this draw; an outright failure wins, because a
                // caller that saw an error must not also be surprised by a pending rename.
                let failed_share = self.plan.failed_rename / probability;
                if rng.chance(failed_share) {
                    Fault::Failed
                } else {
                    Fault::DelayedRename
                }
            }
            _ => Fault::Failed,
        };

        state.log.push(FaultRecord {
            op,
            operation: operation.clone(),
            fault,
        });
        Decision::Inject(fault)
    }
}

/// The error an injected fault surfaces as. One shape, naming both the fault and what it hit,
/// so a test can assert on the message and a failing engine log says where it came from.
fn injected(operation: &Operation, fault: Fault) -> io::Error {
    io::Error::other(format!(
        "esker fault injection: {fault} during {operation:?}"
    ))
}

/// A filesystem that fails on purpose. Clone it freely: every clone shares one schedule, one
/// log and one set of pending renames.
#[derive(Debug, Clone)]
pub struct FaultFileSystem {
    shared: Arc<Shared>,
}

impl FaultFileSystem {
    /// Wraps `inner`, injecting the faults `plan` describes.
    #[must_use]
    pub fn new(inner: Arc<dyn FileSystem>, plan: FaultPlan) -> Self {
        Self {
            shared: Arc::new(Shared {
                inner,
                plan,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// The plan in force.
    #[must_use]
    pub fn plan(&self) -> FaultPlan {
        self.shared.plan
    }

    /// Every fault injected so far, in the order it happened.
    ///
    /// This is what a test asserts against: that a short append happened at all, that exactly
    /// one rename was held pending, that the power cut landed where it was aimed.
    #[must_use]
    pub fn faults(&self) -> Vec<FaultRecord> {
        self.shared.state().log.clone()
    }

    /// How many operations the schedule has handed out.
    #[must_use]
    pub fn operations(&self) -> u64 {
        self.shared.state().next_op
    }

    /// Whether the power has gone out.
    #[must_use]
    pub fn powered_off(&self) -> bool {
        self.shared.state().powered_off
    }

    /// Renames that were reported as successful but have not been applied.
    #[must_use]
    pub fn pending_renames(&self) -> Vec<(PathBuf, PathBuf)> {
        self.shared.state().pending.clone()
    }

    /// Applies every pending rename, as a directory sync of each one's parent would.
    pub fn reveal_renames(&self) -> io::Result<()> {
        let pending = std::mem::take(&mut self.shared.state().pending);
        for (from, to) in pending {
            self.shared.inner.rename(&from, &to)?;
        }
        Ok(())
    }

    /// Discards every pending rename: the crash they were waiting for.
    pub fn lose_pending_renames(&self) {
        self.shared.state().pending.clear();
    }

    /// Reboots: the power comes back, the schedule restarts, and every rename that had not
    /// been made durable is gone.
    ///
    /// The log survives, because it is the record of what the run did.
    pub fn restart(&self) {
        let mut state = self.shared.state();
        state.powered_off = false;
        state.next_op = 0;
        state.pending.clear();
    }

    /// Applies any pending renames whose destination is directly inside `dir`.
    fn sync_dir_applies_renames(&self, dir: &Path) -> io::Result<()> {
        let ready: Vec<(PathBuf, PathBuf)> = {
            let mut state = self.shared.state();
            let (ready, keep) = state
                .pending
                .iter()
                .cloned()
                .partition(|(_, to)| to.parent() == Some(dir));
            state.pending = keep;
            ready
        };
        for (from, to) in ready {
            self.shared.inner.rename(&from, &to)?;
        }
        Ok(())
    }
}

impl FileSystem for FaultFileSystem {
    fn create(&self, path: &Path) -> io::Result<Box<dyn WritableFile>> {
        let operation = Operation::Create(path.to_path_buf());
        if let Decision::Inject(fault) = self.shared.begin(&operation) {
            return Err(injected(&operation, fault));
        }
        Ok(Box::new(FaultWritableFile {
            inner: self.shared.inner.create(path)?,
            path: path.to_path_buf(),
            shared: Arc::clone(&self.shared),
        }))
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn RandomAccessFile>> {
        // Reads are never counted and never faulted: see the module docs.
        self.shared.inner.open(path)
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        // A pending rename has not touched the inner filesystem, so the old name is still
        // there and the new one is not. No special case is needed here, which is exactly why
        // renames are modelled as pending rather than as a shadow name.
        self.shared.inner.list(dir)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let operation = Operation::Rename(from.to_path_buf(), to.to_path_buf());
        match self.shared.begin(&operation) {
            Decision::Proceed => self.shared.inner.rename(from, to),
            Decision::Inject(Fault::DelayedRename) => {
                self.shared
                    .state()
                    .pending
                    .push((from.to_path_buf(), to.to_path_buf()));
                Ok(())
            }
            Decision::Inject(fault) => Err(injected(&operation, fault)),
        }
    }

    fn delete(&self, path: &Path) -> io::Result<()> {
        let operation = Operation::Delete(path.to_path_buf());
        if let Decision::Inject(fault) = self.shared.begin(&operation) {
            return Err(injected(&operation, fault));
        }
        self.shared.inner.delete(path)
    }

    fn fsync_dir(&self, dir: &Path) -> io::Result<()> {
        let operation = Operation::FsyncDir(dir.to_path_buf());
        if let Decision::Inject(fault) = self.shared.begin(&operation) {
            return Err(injected(&operation, fault));
        }
        self.shared.inner.fsync_dir(dir)?;
        // A successful directory sync is what makes a rename durable (invariant 3).
        self.sync_dir_applies_renames(dir)
    }

    fn size(&self, path: &Path) -> io::Result<u64> {
        self.shared.inner.size(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        self.shared.inner.exists(path)
    }

    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        let operation = Operation::CreateDirAll(dir.to_path_buf());
        if let Decision::Inject(fault) = self.shared.begin(&operation) {
            return Err(injected(&operation, fault));
        }
        self.shared.inner.create_dir_all(dir)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        let operation = Operation::HardLink(from.to_path_buf(), to.to_path_buf());
        if let Decision::Inject(fault) = self.shared.begin(&operation) {
            return Err(injected(&operation, fault));
        }
        self.shared.inner.hard_link(from, to)
    }
}

/// A writable file that can write less than it was given.
struct FaultWritableFile {
    inner: Box<dyn WritableFile>,
    path: PathBuf,
    shared: Arc<Shared>,
}

impl WritableFile for FaultWritableFile {
    fn append(&mut self, data: &[u8]) -> io::Result<()> {
        let operation = Operation::Append(self.path.clone(), data.len());
        match self.shared.begin(&operation) {
            Decision::Proceed => self.inner.append(data),
            Decision::Inject(Fault::ShortAppend { wrote, requested }) => {
                // The prefix really does reach the file, which is the point: recovery has to
                // cope with a half-written record, not merely with a failed call.
                self.inner.append(&data[..wrote])?;
                Err(injected(
                    &operation,
                    Fault::ShortAppend { wrote, requested },
                ))
            }
            Decision::Inject(fault) => Err(injected(&operation, fault)),
        }
    }

    fn sync_data(&mut self) -> io::Result<()> {
        let operation = Operation::SyncData(self.path.clone());
        if let Decision::Inject(fault) = self.shared.begin(&operation) {
            return Err(injected(&operation, fault));
        }
        self.inner.sync_data()
    }
}

#[cfg(test)]
mod tests {
    use super::{Fault, FaultFileSystem, FaultPlan, Operation, schedule_rng};
    use crate::fs::FileSystem;
    use crate::memfs::MemFileSystem;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn wrap(plan: FaultPlan) -> (MemFileSystem, FaultFileSystem) {
        let inner = MemFileSystem::new();
        let faulty = FaultFileSystem::new(Arc::new(inner.clone()), plan);
        (inner, faulty)
    }

    /// Writes `count` records, returning how many appends were reported as successful.
    fn write_records(fs: &dyn FileSystem, path: &str, count: usize) -> (usize, usize) {
        let Ok(mut file) = fs.create(Path::new(path)) else {
            return (0, 0);
        };
        let (mut ok, mut failed) = (0, 0);
        for i in 0..count {
            let record = format!("record-{i:04}|");
            if file.append(record.as_bytes()).is_ok() {
                ok += 1;
            } else {
                failed += 1;
            }
        }
        (ok, failed)
    }

    /// The property everything else rests on: the schedule depends on the seed and the
    /// operation index, and on nothing else.
    #[test]
    fn the_schedule_is_a_function_of_seed_and_index() {
        for seed in [0u64, 1, 0xDEAD_BEEF, u64::MAX] {
            for op in [0u64, 1, 2, 47, 1_000_000] {
                let mut a = schedule_rng(seed, op);
                let mut b = schedule_rng(seed, op);
                assert_eq!(a.next_u64(), b.next_u64(), "seed {seed} op {op}");
            }
        }
        // Adjacent indices must not produce the same draw, or a plan would fault in runs.
        let draws: Vec<u64> = (0..64u64)
            .map(|op| schedule_rng(7, op).next_u64())
            .collect();
        let unique: std::collections::BTreeSet<u64> = draws.iter().copied().collect();
        assert_eq!(unique.len(), draws.len(), "adjacent indices collided");

        // And a different seed is a different schedule.
        assert_ne!(schedule_rng(7, 3).next_u64(), schedule_rng(8, 3).next_u64());
    }

    /// Same seed, same failures — twice over, from two independent filesystems.
    #[test]
    fn the_same_seed_replays_exactly() {
        let plan = FaultPlan::chaos(0x5EED, 0.25);
        let run = || {
            let (inner, faulty) = wrap(plan);
            let (ok, failed) = write_records(&faulty, "/log", 200);
            let _ = faulty.rename(Path::new("/log"), Path::new("/log.old"));
            let _ = faulty.fsync_dir(Path::new("/"));
            (ok, failed, faulty.faults(), inner.contents("/log").ok())
        };
        assert_eq!(run(), run());

        // A different seed schedules different failures.
        let other = {
            let (_, faulty) = wrap(FaultPlan::chaos(0x5EED + 1, 0.25));
            write_records(&faulty, "/log", 200);
            faulty.faults()
        };
        let first = run().2;
        assert_ne!(first, other, "two seeds produced the same schedule");
    }

    /// A short append leaves a prefix on disk and reports an error, which is what a torn
    /// record is.
    #[test]
    fn a_short_append_writes_a_prefix_and_fails() {
        let (inner, faulty) = wrap(FaultPlan::none(3).with_short_appends(1.0));
        let mut file = faulty.create(Path::new("/log")).unwrap();
        let error = file.append(b"0123456789").unwrap_err();
        assert!(error.to_string().contains("short append"), "{error}");

        let bytes = inner.contents("/log").unwrap();
        assert!(bytes.len() < 10, "the whole record was written anyway");
        assert_eq!(bytes, b"0123456789"[..bytes.len()], "the prefix is wrong");

        let log = faulty.faults();
        assert_eq!(log.len(), 1);
        assert!(matches!(
            log[0].fault,
            Fault::ShortAppend {
                requested: 10,
                wrote
            } if wrote == bytes.len()
        ));
        assert_eq!(log[0].operation, Operation::Append("/log".into(), 10));
    }

    /// An empty append has no prefix to leave, so it is let through rather than turned into a
    /// nonsensical zero-byte failure.
    #[test]
    fn an_empty_append_is_never_shortened() {
        let (_, faulty) = wrap(FaultPlan::none(1).with_short_appends(1.0));
        let mut file = faulty.create(Path::new("/log")).unwrap();
        assert!(file.append(b"").is_ok());
        assert!(faulty.faults().is_empty());
    }

    /// `sync_data` and `fsync_dir` failures are reported and logged.
    #[test]
    fn syncs_can_fail() {
        let (_, faulty) = wrap(
            FaultPlan::none(9)
                .with_failed_syncs(1.0)
                .with_failed_dir_syncs(1.0),
        );
        let mut file = faulty.create(Path::new("/log")).unwrap();
        file.append(b"data").unwrap();
        assert!(file.sync_data().is_err());
        assert!(faulty.fsync_dir(Path::new("/")).is_err());

        let kinds: Vec<Operation> = faulty.faults().into_iter().map(|f| f.operation).collect();
        assert_eq!(
            kinds,
            vec![
                Operation::SyncData("/log".into()),
                Operation::FsyncDir("/".into())
            ]
        );
        assert!(faulty.faults().iter().all(|f| f.fault == Fault::Failed));
    }

    /// A failed rename changes nothing, and says so.
    #[test]
    fn a_failed_rename_does_nothing() {
        let (inner, faulty) = wrap(FaultPlan::none(4).with_failed_renames(1.0));
        inner.install("/a", b"contents".to_vec()).unwrap();

        assert!(faulty.rename(Path::new("/a"), Path::new("/b")).is_err());
        assert!(inner.exists(Path::new("/a")).unwrap());
        assert!(!inner.exists(Path::new("/b")).unwrap());
        assert!(faulty.pending_renames().is_empty());
        assert_eq!(faulty.faults()[0].fault, Fault::Failed);
    }

    /// The fault this module exists for: a rename that reports success, is not visible, and
    /// becomes visible only when the directory is synced.
    #[test]
    fn a_delayed_rename_needs_a_directory_sync() {
        let (inner, faulty) = wrap(FaultPlan::none(5).with_delayed_renames(1.0));
        inner.install("/dir/CURRENT.tmp", b"new".to_vec()).unwrap();
        inner.install("/dir/CURRENT", b"old".to_vec()).unwrap();

        // The engine renames and believes it worked.
        faulty
            .rename(Path::new("/dir/CURRENT.tmp"), Path::new("/dir/CURRENT"))
            .unwrap();
        assert_eq!(faulty.faults()[0].fault, Fault::DelayedRename);
        assert_eq!(inner.contents("/dir/CURRENT").unwrap(), b"old");
        assert_eq!(
            faulty.pending_renames(),
            vec![(
                PathBuf::from("/dir/CURRENT.tmp"),
                PathBuf::from("/dir/CURRENT")
            )]
        );
        // `list` shows the pre-rename directory, without the injector special-casing it.
        assert!(
            faulty
                .list(Path::new("/dir"))
                .unwrap()
                .contains(&PathBuf::from("/dir/CURRENT.tmp"))
        );

        // Crashing here loses it — the engine forgot to sync the directory.
        let lost = faulty.clone();
        lost.lose_pending_renames();
        assert_eq!(inner.contents("/dir/CURRENT").unwrap(), b"old");

        // Doing it properly: rename, then sync the directory.
        faulty
            .rename(Path::new("/dir/CURRENT.tmp"), Path::new("/dir/CURRENT"))
            .unwrap();
        faulty.fsync_dir(Path::new("/dir")).unwrap();
        assert_eq!(inner.contents("/dir/CURRENT").unwrap(), b"new");
        assert!(faulty.pending_renames().is_empty());
    }

    /// A directory sync only applies the renames that belong to that directory.
    #[test]
    fn a_directory_sync_applies_only_its_own_renames() {
        let (inner, faulty) = wrap(FaultPlan::none(6).with_delayed_renames(1.0));
        inner.install("/a/x.tmp", b"ax".to_vec()).unwrap();
        inner.install("/b/y.tmp", b"by".to_vec()).unwrap();
        faulty
            .rename(Path::new("/a/x.tmp"), Path::new("/a/x"))
            .unwrap();
        faulty
            .rename(Path::new("/b/y.tmp"), Path::new("/b/y"))
            .unwrap();
        assert_eq!(faulty.pending_renames().len(), 2);

        faulty.fsync_dir(Path::new("/a")).unwrap();
        assert!(inner.exists(Path::new("/a/x")).unwrap());
        assert!(!inner.exists(Path::new("/b/y")).unwrap());
        assert_eq!(faulty.pending_renames().len(), 1);
    }

    /// A power cut stops every mutation from its operation onwards, leaves reads working, and
    /// is survived by a restart.
    #[test]
    fn a_power_cut_stops_writes_but_not_reads() {
        let (inner, faulty) = wrap(FaultPlan::power_cut(2, 20));
        let (ok, failed) = write_records(&faulty, "/log", 40);
        // Operation 0 is the create, so 19 appends get through.
        assert_eq!((ok, failed), (19, 21));
        assert!(faulty.powered_off());

        // Reads still work: this is how a test inspects what survived.
        let survived = inner.contents("/log").unwrap();
        assert!(!survived.is_empty());
        assert!(faulty.open(Path::new("/log")).is_ok());
        assert_eq!(
            faulty.size(Path::new("/log")).unwrap(),
            survived.len() as u64
        );
        assert!(faulty.exists(Path::new("/log")).unwrap());

        // Everything after the cut is logged as a power cut, and nothing before it.
        let log = faulty.faults();
        assert_eq!(log.len(), 21);
        assert!(log.iter().all(|record| record.fault == Fault::PowerCut));
        assert_eq!(log[0].op, 20);

        // Rebooting brings the schedule back, and the bytes that were written are still there.
        faulty.restart();
        assert!(!faulty.powered_off());
        assert_eq!(faulty.operations(), 0);
        let mut file = faulty.create(Path::new("/log2")).unwrap();
        assert!(file.append(b"after the reboot").is_ok());
    }

    /// A power cut at operation 0 stops everything, including the first create.
    #[test]
    fn a_power_cut_at_zero_stops_everything() {
        let (inner, faulty) = wrap(FaultPlan::power_cut(1, 0));
        assert!(faulty.create(Path::new("/log")).is_err());
        assert!(faulty.fsync_dir(Path::new("/")).is_err());
        assert!(faulty.delete(Path::new("/log")).is_err());
        assert!(faulty.create_dir_all(Path::new("/d")).is_err());
        assert!(!inner.exists(Path::new("/log")).unwrap());
        assert_eq!(faulty.faults().len(), 4);
    }

    /// With no faults planned, the wrapper is invisible — which is what makes it usable as
    /// the control arm of a test.
    #[test]
    fn an_empty_plan_injects_nothing() {
        let (inner, faulty) = wrap(FaultPlan::none(0));
        let (ok, failed) = write_records(&faulty, "/log", 50);
        assert_eq!((ok, failed), (50, 0));
        faulty
            .create_dir_all(Path::new("/dir"))
            .and_then(|()| faulty.rename(Path::new("/log"), Path::new("/dir/log")))
            .and_then(|()| faulty.fsync_dir(Path::new("/dir")))
            .unwrap();
        assert!(faulty.faults().is_empty());
        assert!(inner.exists(Path::new("/dir/log")).unwrap());
        assert_eq!(faulty.operations(), 54);
    }

    /// Probabilities in between actually land in between: a chaos plan fails some operations
    /// and not others, rather than all or none.
    #[test]
    fn a_partial_probability_faults_some_operations() {
        let (_, faulty) = wrap(FaultPlan::chaos(0x00C0_FFEE, 0.3));
        let (ok, failed) = write_records(&faulty, "/log", 500);
        assert!(ok > 0 && failed > 0, "{ok} ok, {failed} failed");
        // Around 30%, with wide latitude: this asserts the dial is connected, not its exact
        // calibration.
        assert!(
            (60..=240).contains(&failed),
            "{failed} of 500 appends failed at p=0.3"
        );
    }
}
