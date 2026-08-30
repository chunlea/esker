//! The vocabulary of fault injection: what can be done to an operation, and how often.
//!
//! Split out from [`fault_fs`](super::fault_fs) because these four types are the part a test
//! *writes against* — a plan going in, a log of records coming out — while the filesystem
//! itself is the machinery that applies them.

use std::fmt;
use std::path::PathBuf;

/// Which operation a [`FaultRecord`] is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    /// Creating or truncating a file.
    Create(PathBuf),
    /// Appending `bytes` to a file.
    Append(PathBuf, usize),
    /// Making a file's bytes durable.
    SyncData(PathBuf),
    /// Replacing one path with another.
    Rename(PathBuf, PathBuf),
    /// Removing a file.
    Delete(PathBuf),
    /// Making a directory's entries durable.
    FsyncDir(PathBuf),
    /// Creating a directory and its parents.
    CreateDirAll(PathBuf),
    /// Linking a second name to a file.
    HardLink(PathBuf, PathBuf),
}

impl Operation {
    /// The probability this kind of operation is faulted, under `plan`.
    pub(super) fn probability(&self, plan: &FaultPlan) -> f64 {
        match self {
            Self::Append(..) => plan.short_append,
            Self::SyncData(_) => plan.failed_sync,
            Self::Rename(..) => plan.failed_rename + plan.delayed_rename,
            Self::FsyncDir(_) => plan.failed_fsync_dir,
            // Not faulted at random; still stopped by a power cut.
            Self::Create(_) | Self::Delete(_) | Self::CreateDirAll(_) | Self::HardLink(..) => 0.0,
        }
    }
}

/// What was done to an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Only `wrote` of `requested` bytes reached the file, and then the append failed.
    ShortAppend {
        /// Bytes that did reach the file.
        wrote: usize,
        /// Bytes the caller asked to write.
        requested: usize,
    },
    /// The operation returned an error and did nothing.
    Failed,
    /// The rename reported success but was held pending: see the module docs.
    DelayedRename,
    /// The power cut has happened. This operation, and every later one, fails.
    PowerCut,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShortAppend { wrote, requested } => {
                write!(f, "short append: {wrote} of {requested} bytes")
            }
            Self::Failed => f.write_str("injected failure"),
            Self::DelayedRename => f.write_str("rename held pending a directory sync"),
            Self::PowerCut => f.write_str("power cut"),
        }
    }
}

/// One entry of the injector's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultRecord {
    /// The operation's index in the schedule.
    pub op: u64,
    /// What was being attempted.
    pub operation: Operation,
    /// What happened to it.
    pub fault: Fault,
}

/// Which faults to inject, and how often.
///
/// Probabilities are per operation *of that kind*, so `short_append: 0.1` means one append in
/// ten. They are independent of each other; `failed_rename` and `delayed_rename` share the
/// rename draw, with a failure taking precedence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FaultPlan {
    /// Seed of the schedule. Print this on failure; it is the whole reproduction.
    pub seed: u64,
    /// Probability an append writes a prefix and then fails.
    pub short_append: f64,
    /// Probability `sync_data` fails.
    pub failed_sync: f64,
    /// Probability a rename fails outright.
    pub failed_rename: f64,
    /// Probability `fsync_dir` fails.
    pub failed_fsync_dir: f64,
    /// Probability a rename is held pending a directory sync instead of applied.
    pub delayed_rename: f64,
    /// Operation index at which the power goes out, if it does.
    pub power_cut_at: Option<u64>,
}

impl FaultPlan {
    /// A plan that injects nothing — the baseline a test compares against.
    #[must_use]
    pub fn none(seed: u64) -> Self {
        Self {
            seed,
            short_append: 0.0,
            failed_sync: 0.0,
            failed_rename: 0.0,
            failed_fsync_dir: 0.0,
            delayed_rename: 0.0,
            power_cut_at: None,
        }
    }

    /// The power goes out at operation `op`: that operation and every later one fails.
    #[must_use]
    pub fn power_cut(seed: u64, op: u64) -> Self {
        Self {
            power_cut_at: Some(op),
            ..Self::none(seed)
        }
    }

    /// Every fault at the same probability — the plan a fuzzing crash loop wants.
    #[must_use]
    pub fn chaos(seed: u64, probability: f64) -> Self {
        Self {
            short_append: probability,
            failed_sync: probability,
            failed_rename: probability,
            failed_fsync_dir: probability,
            delayed_rename: probability,
            ..Self::none(seed)
        }
    }

    /// Sets the short-append probability.
    #[must_use]
    pub fn with_short_appends(mut self, probability: f64) -> Self {
        self.short_append = probability;
        self
    }

    /// Sets the failed-`sync_data` probability.
    #[must_use]
    pub fn with_failed_syncs(mut self, probability: f64) -> Self {
        self.failed_sync = probability;
        self
    }

    /// Sets the failed-`rename` probability.
    #[must_use]
    pub fn with_failed_renames(mut self, probability: f64) -> Self {
        self.failed_rename = probability;
        self
    }

    /// Sets the failed-`fsync_dir` probability.
    #[must_use]
    pub fn with_failed_dir_syncs(mut self, probability: f64) -> Self {
        self.failed_fsync_dir = probability;
        self
    }

    /// Sets the probability a rename is held pending a directory sync.
    #[must_use]
    pub fn with_delayed_renames(mut self, probability: f64) -> Self {
        self.delayed_rename = probability;
        self
    }

    /// Cuts the power at operation `op`.
    #[must_use]
    pub fn with_power_cut_at(mut self, op: u64) -> Self {
        self.power_cut_at = Some(op);
        self
    }
}
