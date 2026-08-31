//! Driving the object-storage tier from the engine.
//!
//! The tier itself ([`crate::fs::tier`]) knows how to upload, fetch and evict. What it cannot
//! know is *when*, and what it must never do is any of it on the write path — so this module
//! is the thread that drives it and the edit that records what it did.
//!
//! # Why a thread of its own
//!
//! [ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 1: an upload is
//! never awaited by a flush, a compaction, or a foreground write. It is not enough to run
//! uploads *after* a flush's edit on the flush thread — the next memtable's flush would then
//! wait behind them, L0 would climb, and a slow bucket would become a write stall by a longer
//! route. So the uploader gets its own thread and its own signal, the same shape the flusher
//! and the compaction pool already have.
//!
//! Tests use [`Db::tier_maintenance`] directly and set `compaction_threads` aside entirely:
//! a background thread makes "did the upload happen yet" a race, and a test that has to sleep
//! to find out is a test that will be flaky on a loaded machine.

use std::sync::atomic::Ordering;

use crate::error::Result;
use crate::version::{FileLocation, VersionEdit};

use super::{Db, DbInner, lock};

impl Db {
    /// Does one pass of tier work and records what it achieved in the manifest.
    ///
    /// Returns how many uploads completed. Safe to call when there is no tier — it does
    /// nothing and returns zero — so a caller need not ask first.
    ///
    /// The background thread calls this; so may anyone who wants the work done *now*, which is
    /// what every test in this crate does.
    pub fn tier_maintenance(&self) -> Result<usize> {
        self.inner.tier_maintenance()
    }

    /// Where every file the current version names is recorded as living.
    ///
    /// The manifest's own answer, not the filesystem's — which is the point: it is what a
    /// reopen will believe, and therefore what a test about promotion has to check.
    pub fn file_locations(&self) -> std::collections::BTreeMap<u64, FileLocation> {
        let Ok(versions) = lock(&self.inner.versions) else {
            return std::collections::BTreeMap::new();
        };
        let current = versions.current();
        let mut out = std::collections::BTreeMap::new();
        for cf in current.column_families() {
            for level in 0..self.inner.options.num_levels {
                for file in current.files(cf, level) {
                    out.insert(file.number, file.location);
                }
            }
        }
        out
    }

    /// What the tier has done so far, or `None` when the filesystem has no tier.
    pub fn tier_stats(&self) -> Option<crate::fs::tier::TierStats> {
        self.inner.fs.tier().map(crate::fs::SstTier::stats)
    }
}

impl DbInner {
    /// One pass: let the tier work, then durably record any promotions it produced.
    pub(crate) fn tier_maintenance(&self) -> Result<usize> {
        let Some(tier) = self.fs.tier() else {
            return Ok(0);
        };
        let uploaded = tier.maintain();

        let promoted = tier.drain_promotions();
        if promoted.is_empty() {
            return Ok(uploaded);
        }

        let mut edit = VersionEdit::new();
        {
            let versions = lock(&self.versions)?;
            let current = versions.current();
            for number in promoted {
                // A file that is in no version was compacted away between the upload and now.
                // There is nothing to promote and nothing wrong: `retain` will delete its
                // object on the next sweep.
                if let Some((cf, level)) = current.locate(number) {
                    edit.set_location(cf, level, number, FileLocation::Tiered);
                }
            }
        }
        if !edit.is_empty() {
            self.log_and_apply(&mut edit)?;
        }
        Ok(uploaded)
    }

    /// Tells the tier that `number` is durable locally and may be uploaded.
    ///
    /// Called **after** the edit that names the file, never before it. Doing nothing when
    /// there is no tier keeps the call sites free of `if let Some`.
    pub(crate) fn note_durable_sst(&self, number: u64) {
        if let Some(tier) = self.fs.tier() {
            tier.note_durable_sst(number);
            self.signal_tier();
        }
    }

    /// Wakes the uploader.
    pub(crate) fn signal_tier(&self) {
        if let Ok(mut state) = self.tier.lock() {
            state.wanted = true;
        }
        self.tier_wanted.notify_one();
    }

    /// The uploader thread's body.
    ///
    /// Wakes on a signal or on a timeout. The timeout is what retries a failed upload and what
    /// lets an idle database finish tiering the last flush it did — which is exactly the
    /// scale-to-zero case, where nothing else will ever wake this thread again.
    pub(crate) fn tier_loop(&self) {
        // Long enough that an idle database is genuinely idle, short enough that a bucket
        // coming back after an outage is noticed within a heartbeat rather than a coffee break.
        const IDLE: std::time::Duration = std::time::Duration::from_secs(5);

        loop {
            {
                let Ok(mut state) = self.tier.lock() else {
                    return;
                };
                while !state.wanted && !self.shutdown.load(Ordering::Acquire) {
                    let Ok((next, timeout)) = self.tier_wanted.wait_timeout(state, IDLE) else {
                        return;
                    };
                    state = next;
                    if timeout.timed_out() {
                        break;
                    }
                }
                if self.shutdown.load(Ordering::Acquire) {
                    return;
                }
                state.wanted = false;
            }

            if let Err(err) = self.tier_maintenance() {
                // An upload failure is not an error here — the tier swallows those and retries.
                // Reaching this means the *manifest* write failed, which the next edit will
                // report to whoever is writing.
                tracing::warn!(error = %err, "recording tier promotions failed");
            }
        }
    }
}
