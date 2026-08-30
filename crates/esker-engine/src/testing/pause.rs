//! Pause points: holding one thread still, at a named place, while another runs.
//!
//! [`FaultFileSystem`](super::FaultFileSystem) can make an operation fail. It cannot make one
//! thread wait for another, and some of the bugs worth regression-testing are not "an operation
//! failed" but "two threads each did the right thing in the wrong order". Reproducing one of
//! those by sleeping gives a test that passes on a slow machine for the wrong reason, so the
//! engine names the handful of instants where a test may take hold and [`PauseHook`] is how it
//! takes hold of them.
//!
//! A hook is installed per database through
//! [`Options::pause_hook`](crate::options::Options::pause_hook). Both the hook and every call
//! to it are behind `#[cfg(any(test, feature = "testing"))]`, so a normal build contains
//! neither the field nor the call.
//!
//! # Do not build a rendezvous the correct code cannot complete
//!
//! These points sit inside locks, and that is the whole reason they are interesting. A hook
//! that parks at one of them until some *other* thread reaches a point needing the same lock
//! deadlocks exactly when the engine is right — which is the opposite of what a regression test
//! is for. So wait with a deadline, and let the deadline expiring **be** the passing outcome
//! where that is what correctness means: the other thread could not get past the lock, which is
//! precisely the property under test. `the_sweep_never_deletes_a_file_a_flush_just_published`
//! in `tests/concurrency.rs` is written that way and reads as the worked example.

/// A named instant inside the engine at which a test may hold the running thread.
///
/// Non-exhaustive: adding a point is not a breaking change, and no caller should be matching
/// on the whole set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PausePoint {
    /// Inside the obsolete-file sweep, after the directory has been listed and before the
    /// register of files being written is read.
    ///
    /// The two have to describe one instant. A flush that installs its edit between them is in
    /// no version when the directory is read and no longer pending when the register is, so it
    /// looks like garbage twice over and its live file is deleted. The sweep holds the version
    /// lock across both, so a hook parked here blocks every thread trying to install an edit —
    /// which is what makes the deadline, rather than the other thread, the thing that ends the
    /// wait.
    SweptDirectoryBeforePending,

    /// Inside a flush, after the table has been written and before the edit naming it is
    /// logged.
    ///
    /// For this window the file exists on disk and belongs to no version, which is the window
    /// the register above exists to cover.
    FlushedTableBeforeEdit,
}

/// Something a test installs to be called when the engine reaches a [`PausePoint`].
///
/// Called on whichever thread reached the point, which for a flush is the background flush
/// thread and for a sweep is whoever asked for the flush or compaction that ended in one. An
/// implementation must therefore be prepared to be called from several threads, and to be
/// called at a point it does not care about — matching on the point and returning is the
/// normal shape.
pub trait PauseHook: Send + Sync + std::fmt::Debug {
    /// Runs at `point`. Returning resumes the engine.
    fn pause(&self, point: PausePoint);
}
