//! Membership, and the rule that makes single-server changes safe.
//!
//! **A configuration takes effect when its entry is appended, not when it commits**
//! (dissertation §4.1). That is counter-intuitive — everything else in Raft waits for a commit —
//! and it is not an optimisation. A leader that waited for the commit would have to count the
//! quorum for that very entry under the *old* configuration while the new one is what the entry
//! establishes; the two overlap by design for a single-server change, and using the new
//! configuration immediately is what keeps the overlap from mattering.
//!
//! The price is that an uncommitted configuration can be **truncated away**, and the node must
//! then revert to what it had before. So this type is a stack, not a value: appending a change
//! pushes, truncating pops, and committing folds the settled prefix into the base
//! (`docs/plans/phase-3.md` §6 race 3).

// TODO(step-6): step-6 (membership) is the first caller of the mutators here.
#![allow(dead_code)]

use crate::types::{ConfChange, ConfState, Index};

/// The configuration, plus enough history to undo the part that is not committed yet.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConfTracker {
    /// The configuration as of `base_index`; committed, or from a snapshot, so never undone.
    base: ConfState,
    /// The index `base` is the configuration as of.
    base_index: Index,
    /// Changes appended above `base_index`, ascending by index. Each entry records the *resulting*
    /// configuration, so reverting is a pop rather than an inverse operation — inverting
    /// "add voter 4" requires knowing whether 4 was previously a learner, and the stack knows.
    appended: Vec<(Index, ConfState)>,
}

impl ConfTracker {
    /// A tracker starting from a known configuration at `index`.
    pub(crate) fn new(base: ConfState, index: Index) -> Self {
        Self {
            base,
            base_index: index,
            appended: Vec::new(),
        }
    }

    /// The configuration in force right now — the latest in the log, committed or not.
    pub(crate) fn current(&self) -> &ConfState {
        self.appended.last().map_or(&self.base, |(_, conf)| conf)
    }

    /// Applies `change` as of the entry at `index`, returning the new configuration.
    pub(crate) fn append(&mut self, index: Index, change: &ConfChange) -> &ConfState {
        let mut next = self.current().clone();
        change.apply_to(&mut next);
        // A repeated index means the entry at that position was replaced; drop the old one first.
        self.appended.retain(|(at, _)| *at < index);
        self.appended.push((index, next));
        self.current()
    }

    /// Reverts every change appended at or above `index` — the truncation path.
    ///
    /// Returns whether the configuration actually changed, which is what the leader needs to know:
    /// a reverted membership means the progress map has to be rebuilt.
    pub(crate) fn truncate_from(&mut self, index: Index) -> bool {
        let before = self.current().clone();
        self.appended.retain(|(at, _)| *at < index);
        *self.current() != before
    }

    /// Folds every change at or below `index` into the base, so it can no longer be undone.
    pub(crate) fn commit_to(&mut self, index: Index) {
        while let Some((at, conf)) = self.appended.first() {
            if *at > index {
                break;
            }
            self.base = conf.clone();
            self.base_index = *at;
            self.appended.remove(0);
        }
    }

    /// The index of the first configuration change that is appended but not committed, if any.
    ///
    /// A second single-server change may not be proposed while this is `Some`: overlapping changes
    /// can produce two disjoint majorities, which is exactly the split the one-at-a-time rule
    /// exists to prevent.
    pub(crate) fn pending(&self) -> Option<Index> {
        self.appended.first().map(|(at, _)| *at)
    }

    /// Adopts a configuration wholesale — a snapshot restore, where the log below `index` is gone
    /// and with it any history worth keeping.
    pub(crate) fn reset(&mut self, conf: ConfState, index: Index) {
        self.base = conf;
        self.base_index = index;
        self.appended.clear();
    }
}
