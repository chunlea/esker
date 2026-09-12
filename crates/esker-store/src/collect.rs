//! Going and collecting when the safepoint says there is more to collect.
//!
//! [ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md) publishes the
//! number and #62 made a compaction drop what is below it. Neither of them makes a compaction
//! *happen*, and `Store::raise_safepoint`'s own neighbour says why that matters: *a safepoint
//! changes nothing until a compaction reads the entries it applies to.* This is the wire between
//! the two.
//!
//! # What r1 measured with the wire missing
//!
//! At the default 64 MiB write buffer, twenty files over forty-six minutes produced **no
//! compaction at all** (run 127e) — the memtable never even filled, so there was nothing for the
//! level scores to have an opinion about. One forced pass then threw away **73.9%** of the stored
//! entries with the live relation count unmoved. The versions were collectable the whole time and
//! nothing went to collect them.
//!
//! # Why a rise, and not a timer
//!
//! A safepoint moving up **is** the event "a batch of history just became collectable", and it is
//! the only event in the system that means that. A timer would sweep when nothing had changed and
//! miss the moment when everything had. So the sweep is asked for by
//! [`Sweeper::wanted`], which the store calls from the one funnel every safepoint goes through,
//! and a safepoint that does not move asks for nothing.
//!
//! # Why it is debounced, and why from the end
//!
//! A store hears a new safepoint on every heartbeat — ten seconds — and a sweep rewrites whole
//! column families. Undebounced this would be a compaction loop with a safepoint attached.
//!
//! The gap is measured from the **end** of the last sweep rather than its start, which is what
//! makes it self-limiting: a sweep that takes five minutes is followed by the debounce's worth of
//! quiet, not by another sweep that was already due before the first one finished. A gap measured
//! from the start has no such floor, and the degenerate case is a store that only ever compacts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use esker_engine::{Db, cf};

/// The column families a sweep walks.
///
/// **Not `raft`.** The collector is `MvccCollector` and it understands `write` records; the log,
/// hard state and region metadata are not history that a safepoint makes collectable, and
/// rewriting them on every rise would be pure cost. `lock` and `default` are here because what
/// they hold is reachable only through a `write` record: once that record goes, so may they, and
/// a sweep that took `write` alone would leave the two families that a dropped table's rows
/// actually sit in.
/// **`write` first and `default` last**, because the pass that removes orphaned spilled values
/// runs between them: it reads what `write`'s compaction decided and is read by `default`'s.
const COLLECTABLE: [&str; 3] = [cf::WRITE, cf::LOCK, cf::DEFAULT];

/// What the worker and its owner share.
struct Shared {
    db: Arc<Db>,
    /// The shortest gap between the end of one sweep and the start of the next.
    debounce: Duration,
    state: Mutex<State>,
    /// Woken when a sweep is asked for, and when the owner is going away.
    wanted: Condvar,
    /// Woken when a sweep finishes, so a test can wait for one exactly rather than sleeping.
    finished: Condvar,
    /// Sweeps that have finished. Read without the lock, for a property.
    swept: AtomicU64,
}

#[derive(Debug)]
struct State {
    /// The highest safepoint a sweep has been asked for.
    asked: u64,
    /// The highest safepoint a sweep has run at.
    ///
    /// `asked > done` is the whole of "a sweep is pending", which is why a safepoint that repeats
    /// costs nothing: PD republishes the same number every heartbeat when nothing has moved.
    done: u64,
    /// When the last sweep finished.
    last: Option<Instant>,
    stopping: bool,
}

/// Collects when the safepoint rises, no more often than the debounce allows.
///
/// Dropping it stops the worker and waits for it: the thread holds the engine, and a store that
/// closed its database while a sweep was still rewriting it would be the crash test's own
/// scenario arriving by accident.
#[derive(Debug)]
pub struct Sweeper {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Sweeper {
    /// Starts a worker over `db`, collecting at most once per `debounce`.
    #[must_use]
    pub fn start(db: Arc<Db>, debounce: Duration) -> Self {
        let shared = Arc::new(Shared {
            db,
            debounce,
            state: Mutex::new(State {
                asked: 0,
                done: 0,
                last: None,
                stopping: false,
            }),
            wanted: Condvar::new(),
            finished: Condvar::new(),
            swept: AtomicU64::new(0),
        });
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("esker-collect".to_owned())
            .spawn(move || sweep_forever(&worker))
            .ok();
        Self { shared, thread }
    }

    /// Asks for a sweep, because the safepoint is now `safepoint`.
    ///
    /// Cheap and non-blocking on purpose: it is called from the heartbeat's path and from every
    /// `GcSafepoint`, and a store that stalled its heartbeat behind a compaction would be told it
    /// was down by the very driver that asked it to collect.
    pub fn wanted(&self, safepoint: u64) {
        let Ok(mut state) = self.shared.state.lock() else {
            return;
        };
        if safepoint <= state.asked {
            return;
        }
        state.asked = safepoint;
        drop(state);
        self.shared.wanted.notify_all();
    }

    /// Sweeps that have finished.
    ///
    /// The denominator for "collecting kept up": a flat cost curve with this at zero is a store
    /// that never collected and merely had nothing to do.
    #[must_use]
    pub fn swept(&self) -> u64 {
        self.shared.swept.load(Ordering::Relaxed)
    }

    /// Waits until at least `sweeps` have finished, or `timeout` passes. Says which happened.
    ///
    /// Exact rather than a sleep: a test that slept would be asserting on a race, and the
    /// condition it wants is one the worker already signals.
    pub fn wait_for_sweeps(&self, sweeps: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let Ok(mut state) = self.shared.state.lock() else {
            return false;
        };
        while self.swept() < sweeps {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return false;
            };
            let Ok((next, _)) = self.shared.finished.wait_timeout(state, left) else {
                return false;
            };
            state = next;
        }
        true
    }
}

impl std::fmt::Debug for Shared {
    /// By hand because a `Condvar` has no `Debug`, and naming them rather than skipping them
    /// because a field left out of a hand-written `Debug` is how one goes stale.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sweeper")
            .field("db", &self.db)
            .field("debounce", &self.debounce)
            .field("swept", &self.swept.load(Ordering::Relaxed))
            .field("state", &self.state.lock().ok())
            .field("wanted", &"Condvar")
            .field("finished", &"Condvar")
            .finish()
    }
}

impl Drop for Sweeper {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.stopping = true;
        }
        self.shared.wanted.notify_all();
        if let Some(thread) = self.thread.take() {
            // A sweep in flight is waited out rather than abandoned: it is rewriting files in the
            // database this store is about to close.
            let _ = thread.join();
        }
    }
}

/// Entries one column family's SSTs hold, or `0` when the engine will not say.
///
/// A count and not a `Result`: this is a log line's denominator, and a sweep that collected must
/// not fail because the number it wanted to report about itself was unavailable.
fn entries(db: &Db, cf: &str) -> u64 {
    db.sst_entries(cf)
        .map_or(0, |ssts| ssts.iter().map(|(_, _, entries)| entries).sum())
}

/// The worker's body.
fn sweep_forever(shared: &Arc<Shared>) {
    loop {
        let target = {
            let Ok(mut state) = shared.state.lock() else {
                return;
            };
            loop {
                if state.stopping {
                    return;
                }
                if state.asked <= state.done {
                    let Ok(next) = shared.wanted.wait(state) else {
                        return;
                    };
                    state = next;
                    continue;
                }
                // Something is wanted. The debounce decides whether it is wanted *yet*, and this
                // waits on the same condvar so that a stop arriving during the gap is not sat
                // through.
                let waited = state.last.map(|last| last.elapsed());
                if let Some(waited) = waited
                    && waited < shared.debounce
                {
                    // Saturating: `elapsed` is monotonic in principle and this is a wait in
                    // practice, and the two disagreeing should cost a sweep that runs early, not
                    // one that waits for the rest of the day.
                    let gap = shared.debounce.saturating_sub(waited);
                    let Ok((next, _)) = shared.wanted.wait_timeout(state, gap) else {
                        return;
                    };
                    state = next;
                    continue;
                }
                break state.asked;
            }
        };

        let started = Instant::now();
        let mut before = 0u64;
        let mut after = 0u64;
        let mut orphans = 0;
        for name in COLLECTABLE {
            // **Flushed before the count, because the sweep flushes anyway.** `compact_range`
            // starts with a flush, so a "before" taken without one counts the SSTs and not the
            // memtable the sweep is about to add to them — and the sweep would read as having
            // *gained* entries. This is the same flush, moved one line earlier so that the two
            // numbers are comparable.
            if let Err(error) = shared.db.flush(name) {
                tracing::warn!(cf = name, error = %error, "flushing before a collection failed");
                continue;
            }
            let held = entries(&shared.db, name);
            if let Err(error) = shared.db.compact_range(name, None, None) {
                // Loud, and not fatal. The safepoint is unchanged, so the next rise asks again,
                // and a store that refused to serve because a collection failed would be trading
                // a space problem for an availability one.
                tracing::warn!(cf = name, error = %error, "collecting the column family failed");
                continue;
            }
            let left = entries(&shared.db, name);
            tracing::debug!(
                cf = name,
                safepoint = target,
                before = held,
                after = left,
                dropped = held.saturating_sub(left),
                "collected a column family"
            );
            before += held;
            after += left;

            // **Between the two compactions, and that is the whole of the ordering.** The pass
            // reads what `write`'s compaction decided — ADR 0111 drops a deleted key's records
            // there, and only then does the value they named look like an orphan — and what it
            // writes is read by `default`'s, which is the next family in `COLLECTABLE` and applies
            // the tombstones this leaves rather than carrying them to the next sweep. Run before
            // the first, it sees every record still in place and finds nothing; run after the
            // second, its deletions wait a whole debounce to take effect.
            if name == cf::WRITE {
                match crate::gc::collect_spilled_values(&shared.db, target) {
                    Ok(count) => orphans = count,
                    Err(error) => {
                        tracing::warn!(error = %error, "collecting orphaned spilled values failed");
                    }
                }
            }
        }
        // **One line per sweep at `info`**, because a sweep is rare by construction — at most one
        // per debounce — and it is the answer to "did publishing that safepoint reclaim anything".
        // Until this existed the only way to tell a collection that dropped three quarters of the
        // store from one that dropped nothing was to measure the directory (run 127i).
        tracing::info!(
            safepoint = target,
            before,
            after,
            dropped = before.saturating_sub(after),
            orphans,
            took_ms = started.elapsed().as_millis(),
            "collected"
        );

        let Ok(mut state) = shared.state.lock() else {
            return;
        };
        state.done = target;
        state.last = Some(Instant::now());
        shared.swept.fetch_add(1, Ordering::Relaxed);
        drop(state);
        shared.finished.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::Sweeper;
    use esker_engine::{Db, Options, cf};
    use std::sync::Arc;
    use std::time::Duration;

    /// Long enough that a machine under load does not report a hang as a defect, short enough that
    /// a real hang still ends the test.
    const PATIENCE: Duration = Duration::from_secs(10);

    fn db() -> (tempfile::TempDir, Arc<Db>) {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let db = Db::open_with(
            dir.path(),
            Options {
                create_if_missing: true,
                ..Options::default()
            },
            Arc::new(esker_engine::fs::LocalFileSystem::new()),
            &cf::BUILTIN,
        )
        .expect("the database opens");
        (dir, Arc::new(db))
    }

    /// The whole of #70 in one assertion: the number moves, and something goes and acts on it.
    #[test]
    fn a_rising_safepoint_is_collected() {
        let (_dir, db) = db();
        let sweeper = Sweeper::start(db, Duration::ZERO);
        sweeper.wanted(10);
        assert!(
            sweeper.wait_for_sweeps(1, PATIENCE),
            "the safepoint rose and nothing collected"
        );
    }

    /// **A safepoint that repeats is most heartbeats.** PD republishes the same number every ten
    /// seconds when nothing has moved, and a store that swept on each of those would be compacting
    /// continuously over a window in which nothing became collectable.
    #[test]
    fn a_safepoint_that_does_not_move_asks_for_nothing() {
        let (_dir, db) = db();
        let sweeper = Sweeper::start(db, Duration::ZERO);
        sweeper.wanted(10);
        assert!(
            sweeper.wait_for_sweeps(1, PATIENCE),
            "the first rise sweeps"
        );
        for _ in 0..5 {
            sweeper.wanted(10);
        }
        assert!(
            !sweeper.wait_for_sweeps(2, Duration::from_millis(200)),
            "the same safepoint five more times asked for {} sweeps",
            sweeper.swept()
        );
    }

    /// The debounce holds the second sweep off, and then lets it through.
    ///
    /// **Both halves.** A debounce that never released would pass the first assertion and be a
    /// store that collects once and never again.
    #[test]
    fn the_debounce_holds_a_second_sweep_off_and_then_releases_it() {
        let (_dir, db) = db();
        let gap = Duration::from_millis(400);
        let sweeper = Sweeper::start(db, gap);
        sweeper.wanted(10);
        assert!(
            sweeper.wait_for_sweeps(1, PATIENCE),
            "the first rise sweeps"
        );

        sweeper.wanted(20);
        assert!(
            !sweeper.wait_for_sweeps(2, gap / 4),
            "a second rise a quarter of the debounce later swept anyway"
        );
        assert!(
            sweeper.wait_for_sweeps(2, PATIENCE),
            "the debounce passed and the sweep it was holding never ran — which is a store that \
             collects once and then never again"
        );
    }
}
