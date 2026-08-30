//! The obsolete-file sweep, against a flush that publishes underneath it.
//!
//! A sweep decides what to delete from two things: the directory listing, which says what
//! exists, and the register of files being written, which says which of those are outputs that
//! no version has named *yet*. Both are true when read. Read one after the other, they describe
//! two different moments — and a flush that installs its edit in the gap falls through it: it
//! was in no version when the directory was read, and is no longer pending by the time the
//! register is. Its live SST is deleted, and the next read of it fails with `NotFound`.
//!
//! That is a two-thread ordering bug, not a failing operation, so [`FaultFileSystem`] cannot
//! reach it. It is reproduced here by construction, through the pause points in
//! [`esker_engine::testing::pause`].
//!
//! # Why the sweep's wait is a deadline and not a rendezvous
//!
//! The interleaving needs the flush to publish *while the sweep sits between its two samples*.
//! The fix is that the sweep holds the version lock across both — which is exactly the lock the
//! flush needs to publish. So in a correct engine the flush **cannot** get there, and a hook
//! that waited for it would deadlock precisely when the code is right. The wait is therefore a
//! deadline, and the deadline expiring is the passing outcome: it means the flush could not
//! publish, which is the property under test. A broken engine lets the flush through in
//! microseconds, far inside the window.
//!
//! Note which side the waiting is on. The test never depends on the flush being *fast* — a slow
//! machine only gives a broken engine more room to lose the file, never less. There is no way
//! for this test to pass for the wrong reason by running slowly.
//!
//! [`FaultFileSystem`]: esker_engine::testing::FaultFileSystem

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use esker_engine::fs::FileSystem;
use esker_engine::memfs::MemFileSystem;
use esker_engine::options::{CfOptions, Options, ReadOptions};
use esker_engine::testing::{PauseHook, PausePoint};
use esker_engine::{Db, cf};

const DIR: &str = "/db";

/// How long the sweep is held between its two samples. A broken engine publishes inside a
/// millisecond; a correct one cannot publish at all, so this is what the test costs.
const HOLD: Duration = Duration::from_millis(500);

/// An upper bound on the flush's wait for the sweep to reach the directory listing. Only a
/// guard against hanging the suite if the sweep never arrives — the assertion, not this, is
/// what reports a failure.
const HANDOFF: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct State {
    /// Set once the setup flush is done, so only the flush the test cares about is held.
    armed: bool,
    /// The flush has written its table and is parked before logging the edit that names it.
    table_built: bool,
    /// The sweep has listed the directory and is parked before reading the register.
    swept: bool,
}

/// Puts the flush and the sweep into the one order that loses a file.
#[derive(Debug, Default)]
struct Rendezvous {
    state: Mutex<State>,
    signal: Condvar,
}

impl Rendezvous {
    fn arm(&self) {
        self.state.lock().unwrap().armed = true;
    }

    fn wait_for_table(&self) {
        let mut state = self.state.lock().unwrap();
        while !state.table_built {
            let (next, timeout) = self.signal.wait_timeout(state, HANDOFF).unwrap();
            state = next;
            assert!(!timeout.timed_out(), "the flush never built its table");
        }
    }
}

impl PauseHook for Rendezvous {
    fn pause(&self, point: PausePoint) {
        match point {
            // The table is on disk and belongs to no version. Announce that, then wait for the
            // sweep to have listed the directory — so the listing is guaranteed to contain it.
            PausePoint::FlushedTableBeforeEdit => {
                let mut state = self.state.lock().unwrap();
                if !state.armed || state.table_built {
                    return;
                }
                state.table_built = true;
                self.signal.notify_all();
                while !state.swept {
                    let (next, timeout) = self.signal.wait_timeout(state, HANDOFF).unwrap();
                    state = next;
                    if timeout.timed_out() {
                        return;
                    }
                }
            }
            // The directory has been listed and the register has not been read. Release the
            // flush and hold here, which is the gap the bug lives in.
            PausePoint::SweptDirectoryBeforePending => {
                {
                    let mut state = self.state.lock().unwrap();
                    if !state.armed || state.swept || !state.table_built {
                        return;
                    }
                    state.swept = true;
                    self.signal.notify_all();
                }
                std::thread::sleep(HOLD);
            }
            _ => {}
        }
    }
}

/// **Regression.** The sweep must not delete a file a flush published while it was deciding.
#[test]
fn the_sweep_never_deletes_a_file_a_flush_just_published() {
    let rendezvous = Arc::new(Rendezvous::default());
    let fs: Arc<dyn FileSystem> = Arc::new(MemFileSystem::new());
    let options = Options {
        create_if_missing: true,
        pause_hook: Some(Arc::clone(&rendezvous) as Arc<dyn PauseHook>),
        cf_options: CfOptions {
            // Out of reach, so the only sweep is the one this test asks for and the background
            // pool cannot get in front of it.
            level0_file_num_compaction_trigger: 100,
            ..CfOptions::default()
        },
        ..Options::default()
    };
    let db =
        Arc::new(Db::open_with(DIR, options, Arc::clone(&fs), &[cf::DEFAULT, cf::LOCK]).unwrap());

    // Something for the compaction below to move, which is what gets it as far as a sweep.
    db.put(cf::DEFAULT, b"a", b"1").unwrap();
    db.flush(cf::DEFAULT).unwrap();

    rendezvous.arm();

    // The flush whose output is at risk. It parks with the table written and unnamed.
    let flusher = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            db.put(cf::LOCK, b"b", b"2").unwrap();
            db.flush(cf::LOCK).unwrap();
        })
    };

    // Only once the table exists can a listing include it, so the sweep starts after that.
    rendezvous.wait_for_table();
    db.compact_range(cf::DEFAULT, None, None).unwrap();
    flusher.join().unwrap();

    let found = db
        .get(cf::LOCK, b"b", &ReadOptions::default())
        .expect("the sweep deleted a file the current version still named");
    assert_eq!(
        found.as_deref(),
        Some(&b"2"[..]),
        "the flushed value has to survive a sweep that ran across its publish"
    );

    // The two samples disagreeing is the mechanism, so check the file itself is still there and
    // not merely readable from a cache that has not noticed yet.
    let survivors: Vec<_> = fs
        .list(std::path::Path::new(DIR))
        .unwrap()
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "sst"))
        .collect();
    assert!(
        !survivors.is_empty(),
        "every SST was swept away: {survivors:?}"
    );
}
