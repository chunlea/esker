//! **A load arm that proves it is applying load.**
//!
//! The half of `a-load-arm-must-carry-its-own-timeout` that the memory did not have: an arm must
//! carry its own timeout *and* a caller must check it is running before believing anything it
//! measured. Twenty passing runs of a stalling test were collected here against shell background
//! jobs that the harness had already reaped — `ps` found none of them — and every "at load N" in
//! those notes was the box's ambient load. A negative result from an arm that was not running is
//! worth nothing, and it looks exactly like a result.
//!
//! So this is threads this process owns, a stop flag, a deadline each thread reads for itself, and
//! [`Load::applied`], which does not return true until the **one-minute average has actually
//! risen**. Nothing is counted before that.

#![allow(dead_code, reason = "a harness offers more than any one test uses")]
#![allow(
    unreachable_pub,
    reason = "a test-only module: `pub` is what makes it reachable from the binary that includes it"
)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Busy threads, and the proof that they are busy.
pub struct Load {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    baseline: f64,
}

impl Load {
    /// Spins `threads` threads until [`Load`] is dropped or `limit` passes, whichever is first.
    ///
    /// **The deadline is read by each thread**, not by whoever remembers to stop them: a thread
    /// that outlives its test is the failure this whole module is named after, and a flag alone
    /// relies on a `Drop` that a panic can skip.
    #[must_use]
    pub fn spin(threads: usize, limit: Duration) -> Self {
        let baseline = one_minute();
        let stop = Arc::new(AtomicBool::new(false));
        let handles = (0..threads)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let until = Instant::now() + limit;
                    // Work the optimiser cannot delete, and a clock check often enough that
                    // stopping is prompt and rare enough that it is not what is being measured.
                    let mut n: u64 = 0;
                    while !stop.load(Ordering::Relaxed) && Instant::now() < until {
                        for _ in 0..200_000 {
                            n = n.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                        }
                        std::hint::black_box(n);
                    }
                })
            })
            .collect();
        Load {
            stop,
            threads: handles,
            baseline,
        }
    }

    /// Whether the one-minute average has risen by at least `by` since this arm started.
    ///
    /// Polls until it has or `within` passes. The average is an exponentially weighted mean over a
    /// minute, so it climbs slowly and a caller has to be willing to wait for it — which is the
    /// point: an arm that cannot move it in `within` is an arm that is not applying load, and
    /// saying so is the whole reason this method exists.
    pub fn applied(&self, by: f64, within: Duration) -> bool {
        let until = Instant::now() + within;
        while Instant::now() < until {
            if one_minute() >= self.baseline + by {
                return true;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        false
    }

    /// The one-minute average now, for a caller that reports what a round ran at.
    ///
    /// Takes `&self` although it reads a global: an arm that has been dropped has stopped, and a
    /// reading taken through one that is still alive is a reading of what it is doing.
    #[must_use]
    pub fn now(&self) -> f64 {
        let _ = self;
        one_minute()
    }

    /// What it was before this arm started.
    #[must_use]
    pub fn baseline(&self) -> f64 {
        self.baseline
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// The one-minute load average, or `0.0` where the kernel does not publish one.
///
/// `/proc/loadavg` rather than a crate: this runs in the Linux container the gate uses, and a
/// dependency for one line of text is not one this project takes.
#[must_use]
pub fn one_minute() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .and_then(|first| first.parse().ok())
        .unwrap_or(0.0)
}
