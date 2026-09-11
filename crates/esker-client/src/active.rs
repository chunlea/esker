//! Which snapshots this client still has open, so the cluster knows what it may not collect.
//!
//! [ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md) makes the
//! garbage-collection safepoint `min(now − retention window, the oldest active read)`. The second
//! half is this: the oldest `start_ts` any open transaction here is still reading at. It is
//! reported to the placement driver by whoever holds the connection to it — a SQL node does, a
//! bare client does not — and **only the minimum is reported**, because that is the whole of what
//! the safepoint needs and the rest is this process's business.
//!
//! # A multiset, not a set
//!
//! Two transactions may begin at the same timestamp — `begin_at`, a checkpoint replayed twice, or
//! simply two reads in the same millisecond on a counting oracle. A set would let the *first* one
//! to finish release a floor the second still needs, which collects history out from under a live
//! read. So the count is kept.
//!
//! # Leaving is not optional
//!
//! A registration that outlives its transaction pins the safepoint **for ever**: PD is told a read
//! is open, no TTL applies (this process is still reporting), and history accumulates until
//! somebody restarts the node. So the deregistration is a [`Held`] guard rather than a call at the
//! end of each path — commit, rollback, abort and panic all unwind through the same `Drop`, and
//! the one that would have been forgotten is the one nobody writes a test for.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The snapshots this client has open, by `start_ts`, with a count each.
#[derive(Debug, Default)]
pub(crate) struct Active {
    open: Mutex<BTreeMap<u64, usize>>,
}

impl Active {
    /// Registers `start_ts` until the returned guard is dropped.
    pub(crate) fn hold(self: &Arc<Self>, start_ts: u64) -> Held {
        if let Ok(mut open) = self.open.lock() {
            *open.entry(start_ts).or_insert(0) += 1;
        }
        Held {
            active: Arc::clone(self),
            start_ts,
        }
    }

    /// The oldest snapshot still open, or `None` when this client holds none.
    ///
    /// `None` is a fact and not an absence: it says "nothing open here", which is what lets the
    /// window half of the safepoint apply. A process that cannot answer at all should not report.
    pub(crate) fn oldest(&self) -> Option<u64> {
        self.open
            .lock()
            .ok()
            .and_then(|open| open.keys().next().copied())
    }

    /// How many snapshots are open, for the tests that check the guard released.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.open.lock().map_or(0, |open| open.values().sum())
    }

    fn release(&self, start_ts: u64) {
        let Ok(mut open) = self.open.lock() else {
            return;
        };
        if let std::collections::btree_map::Entry::Occupied(mut slot) = open.entry(start_ts) {
            *slot.get_mut() -= 1;
            if *slot.get() == 0 {
                slot.remove();
            }
        }
    }
}

/// One registration, released when this is dropped.
///
/// Held by the transaction itself, so every way out of one — including a panic — goes through
/// `Drop`. That is the point: a path that forgot to deregister would pin the safepoint for the
/// life of the process, and it would be the path nobody tests.
#[derive(Debug)]
pub(crate) struct Held {
    active: Arc<Active>,
    start_ts: u64,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.active.release(self.start_ts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_oldest_is_the_minimum_and_a_release_takes_only_its_own() {
        let active = Arc::new(Active::default());
        let old = active.hold(10);
        let new = active.hold(20);
        assert_eq!(active.oldest(), Some(10));

        drop(old);
        assert_eq!(active.oldest(), Some(20), "releasing 10 left 20 holding");
        drop(new);
        assert_eq!(active.oldest(), None, "nothing open, and it says so");
    }

    /// **Two at the same timestamp, and the first to leave must not release the floor.**
    ///
    /// A set would; the count is why this is a multiset. The failure it prevents is silent:
    /// history collected out from under a transaction that is still reading.
    #[test]
    fn two_at_one_timestamp_need_two_releases() {
        let active = Arc::new(Active::default());
        let first = active.hold(10);
        let second = active.hold(10);

        drop(first);
        assert_eq!(
            active.oldest(),
            Some(10),
            "one of two readers at 10 left and the floor went with it"
        );
        drop(second);
        assert_eq!(active.oldest(), None);
    }

    /// **A panic out of the scope releases it**, which is the path a call at the end of each
    /// branch would have missed.
    #[test]
    fn a_panic_releases_the_registration() {
        let active = Arc::new(Active::default());
        let caught = std::panic::catch_unwind({
            let active = Arc::clone(&active);
            move || {
                let _held = active.hold(10);
                assert_eq!(active.len(), 1);
                panic!("a transaction that did not get to commit");
            }
        });

        assert!(
            caught.is_err(),
            "the panic did not happen, so nothing unwound"
        );
        assert_eq!(
            active.len(),
            0,
            "a panicking transaction left its snapshot registered, pinning the safepoint for ever"
        );
        assert_eq!(active.oldest(), None);
    }
}
