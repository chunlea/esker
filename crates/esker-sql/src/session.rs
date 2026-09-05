//! Which session is which, so that a cancellation can name one.
//!
//! **Top level rather than under `pgwire`, because a session is not a pgwire idea.** Every
//! `Executor` has one — a connection's, a `Pair`'s, a `Cluster`'s — and `pg_stat_activity` reads
//! them all from here. Putting the registry under the protocol module would have made the catalog
//! depend on the wire, and would have left the in-process harnesses without an identity, which is
//! how a view ends up with one code path for tests and another for clients.
//!
//! A `CancelRequest` arrives on its **own connection** — the client opens a second socket, sends
//! the pid and secret key it was handed at startup, and the server closes it without a reply. So
//! the session being cancelled cannot be found from the connection asking: there has to be a
//! process-wide registry, and this is it.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// One live session's cancellation handle.
#[derive(Clone, Debug)]
pub struct Backend {
    /// What `BackendKeyData` announced and `pg_stat_activity` will report.
    ///
    /// `u32` because that is what the protocol carries; `pg_stat_activity`'s column is `int4` and
    /// converts at that boundary, which is the only place the sign means anything.
    pub pid: u32,
    /// The secret that makes a `CancelRequest` for this pid this client's to send.
    pub key: u32,
    /// Set by a cancellation, read by `crate::exec::cancel` between units of work.
    pub cancel: Arc<AtomicBool>,
    /// What this session is doing, for `pg_stat_activity` to report.
    pub activity: Arc<Mutex<Activity>>,
}

/// What a session is doing right now.
#[derive(Debug, Default, Clone)]
pub struct Activity {
    /// The database it connected to, or empty before it has said.
    pub database: String,
    /// The **last** statement this session ran, running or not.
    ///
    /// **Retained when it finishes, not cleared.** PostgreSQL keeps the text on an idle session and
    /// this node used to empty it — measured side by side, and it is why a hunter looking for the
    /// session *holding* a row lock found nothing: the holder is idle between statements, so its
    /// `SELECT … FOR UPDATE` had been erased while the waiter's was still visible.
    /// `WHERE query LIKE '% FOR UPDATE%'` matched the holder and the waiter on PG19 and only the
    /// waiter here.
    pub query: Option<String>,
    /// Whether that statement is running *now*, which is `active` against `idle`.
    pub running: bool,
    /// Whether the session is inside an open block, which is what makes `idle` into
    /// `idle in transaction` — a distinction PostgreSQL draws and this node did not.
    pub in_transaction: bool,
}

impl Backend {
    /// Records whether this session is inside an open transaction block.
    pub fn in_transaction(&self, open: bool) {
        if let Ok(mut activity) = self.activity.lock() {
            activity.in_transaction = open;
        }
    }

    /// Records the database this session is on.
    pub fn on_database(&self, database: &str) {
        if let Ok(mut activity) = self.activity.lock() {
            database.clone_into(&mut activity.database);
        }
    }

    /// Marks this session as running `sql` until the guard is dropped.
    ///
    /// A guard rather than a pair of calls, because the statement can leave by an error, a panic
    /// or a return, and a session left reading `active` forever would be a lie that grows: the
    /// Rails test that wants this asks `WHERE query LIKE '% FOR UPDATE'`, and a stale row would
    /// make it cancel a statement that finished long ago.
    /// **Owns a handle rather than borrowing the backend**: the guard lives for the whole
    /// statement, and a borrow of `self.identity` would hold the executor immutably borrowed for
    /// exactly as long — which the statement, needing `&mut self`, cannot allow.
    #[must_use]
    pub fn running(&self, sql: &str) -> Running {
        if let Ok(mut activity) = self.activity.lock() {
            activity.query = Some(sql.to_owned());
            activity.running = true;
        }
        Running(Arc::clone(&self.activity))
    }
}

/// Clears the running statement when it ends, however it ends.
#[derive(Debug)]
pub struct Running(Arc<Mutex<Activity>>);

impl Drop for Running {
    /// **Stops running; does not forget.** The text stays for `pg_stat_activity` to report on an
    /// idle session, which is what a real server does.
    fn drop(&mut self) {
        if let Ok(mut activity) = self.0.lock() {
            activity.running = false;
        }
    }
}

fn backends() -> &'static Mutex<HashMap<u32, Backend>> {
    static BACKENDS: OnceLock<Mutex<HashMap<u32, Backend>>> = OnceLock::new();
    BACKENDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A cancel key an attacker cannot derive.
///
/// **Deliberately not `esker_base::Pcg32`.** That generator is seeded, and where the project needs
/// values that merely *differ* it is seeded from the wall clock and the process id
/// (`esker_client::retry::Jitter::from_entropy`). A cancel key needs more than difference: anyone
/// who could compute it could cancel any statement on any connection, which is a denial of service
/// against every session on the node rather than a curiosity. "Denial only" is not a reason to
/// accept a key derivable from time and pid.
///
/// `esker-base`'s rule against OS entropy exists so the simulator replays deterministically, and
/// **no cancel key is on a simulated path** — nothing the simulator drives speaks the pgwire
/// protocol — so nothing that rule protects is touched here.
///
/// The source is `RandomState`, whose two `SipHash` keys come from the OS RNG once per thread.
/// `k1` never leaves that state and `SipHash` is a PRF, so hashing a fixed input yields an output
/// that cannot be predicted without it. Chosen over a four-byte read of `/dev/urandom` because it
/// has **no failure path** — no file to open, so no fallback branch that would go untested — and no
/// assumption about the platform having that device.
fn fresh_key() -> u32 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u8(0);
    // Truncating on purpose: `BackendKeyData` carries four bytes on protocol 3.0, so this is not a
    // wire change — the low word of a 64-bit PRF output is still a PRF output.
    #[allow(clippy::cast_possible_truncation)]
    let key = hasher.finish() as u32;
    key
}

/// Registers a new session and gives it the pid and key its `BackendKeyData` will announce.
#[must_use]
pub fn register() -> Backend {
    // Small positive integers, as a real server's pids are. Uniqueness within this process is all
    // a `CancelRequest` needs, since the key is what proves the right to use one.
    static NEXT_PID: AtomicU32 = AtomicU32::new(1);
    let backend = Backend {
        pid: NEXT_PID.fetch_add(1, Ordering::Relaxed),
        key: fresh_key(),
        cancel: Arc::new(AtomicBool::new(false)),
        activity: Arc::new(Mutex::new(Activity::default())),
    };
    if let Ok(mut live) = backends().lock() {
        live.insert(backend.pid, backend.clone());
    }
    backend
}

/// Every live session, for `pg_stat_activity`.
///
/// **One code path for every session**, which is the property this module exists for: a connection,
/// a `Pair` and a `Cluster` all register here, so the view has no second answer for the ones that
/// did not arrive over a socket.
#[must_use]
pub fn snapshot() -> Vec<(u32, Activity)> {
    let Ok(live) = backends().lock() else {
        return Vec::new();
    };
    let mut rows: Vec<(u32, Activity)> = live
        .values()
        .map(|backend| {
            let activity = backend
                .activity
                .lock()
                .map(|guard| guard.clone())
                .unwrap_or_default();
            (backend.pid, activity)
        })
        .collect();
    // **Running sessions first, then by pid** — and the first half is a choice this node makes
    // where PostgreSQL specifies no order at all, not an attempt to copy one.
    //
    // Measured, because the obvious answer is wrong. `pg_stat_activity` on a real server is read
    // out of the `PGPROC` slot array, and slots are *reused*, so its order is neither pid order
    // nor connection order. Three sessions opened one second apart on PG19:
    //
    // ```text
    // connected  46822, then 46830, then 46838
    // returned   46830 | 46822 | 46838
    // ```
    //
    // So "return them in connection order, which is what PostgreSQL does" cannot be implemented,
    // because that is not what PostgreSQL does — the run-86 capture that showed a waiter ahead of
    // an older holder was slot reuse, not an order anything can reproduce.
    //
    // What that leaves is a *choice*, and this is the one that serves the only client known to
    // depend on it. `transaction_test.rb` hunts a cancellation target with
    // `SELECT pid FROM pg_stat_activity WHERE query LIKE '% FOR UPDATE'` and **no `ORDER BY`**,
    // then cancels the first row. Both the holder and the waiter match. The useful answer — the
    // one a person asking that question wants — is the session that is *running* the statement,
    // not the one idle in a transaction that ran it earlier; PostgreSQL gives it by accident and
    // this gives it on purpose. The pid tie-break keeps the view stable between calls, which is
    // what the sort was here for in the first place.
    rows.sort_by_key(|(pid, activity)| (!activity.running, *pid));
    rows
}

/// Cancels the session at `pid` without a key, for `pg_cancel_backend()`.
///
/// **No key, and that is not an oversight.** The protocol's `CancelRequest` arrives unauthenticated
/// on a fresh socket, so it must prove it was told the secret; `pg_cancel_backend` is a function
/// call inside an authenticated session, which is the proof. PostgreSQL draws the same line.
///
/// False for a pid nobody holds, which is what the function answers there.
pub fn cancel_pid(pid: u32) -> bool {
    let Ok(live) = backends().lock() else {
        return false;
    };
    match live.get(&pid) {
        Some(backend) => {
            backend.cancel.store(true, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

/// Forgets a session that has gone.
pub fn deregister(pid: u32) {
    if let Ok(mut live) = backends().lock() {
        live.remove(&pid);
    }
}

/// Asks the session at `pid` to stop what it is doing, if `key` is the one it was given.
///
/// **A wrong key is silently nothing**, which is what a real server does: the protocol has no reply
/// to a `CancelRequest`, so there is no channel to report a refusal on and no oracle for guessing.
/// The bool is for callers that have one — `pg_cancel_backend` returns false for a pid that is not
/// there.
pub fn cancel(pid: u32, key: u32) -> bool {
    let Ok(live) = backends().lock() else {
        return false;
    };
    match live.get(&pid) {
        Some(backend) if backend.key == key => {
            backend.cancel.store(true, Ordering::Relaxed);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{cancel, deregister, fresh_key, register};
    use std::sync::atomic::Ordering;

    /// The right key cancels; a wrong one does nothing and says so.
    #[test]
    fn a_cancellation_needs_the_key_the_session_was_given() {
        let backend = register();
        assert!(!backend.cancel.load(Ordering::Relaxed));

        assert!(
            !cancel(backend.pid, backend.key.wrapping_add(1)),
            "a wrong key must not cancel"
        );
        assert!(
            !backend.cancel.load(Ordering::Relaxed),
            "and must not have set the flag on the way to saying no"
        );

        assert!(cancel(backend.pid, backend.key));
        assert!(backend.cancel.load(Ordering::Relaxed));
        deregister(backend.pid);
    }

    /// A pid nobody holds is not an error and not a panic — the protocol has no reply for either.
    #[test]
    fn cancelling_a_session_that_has_gone_is_merely_false() {
        let backend = register();
        deregister(backend.pid);
        assert!(!cancel(backend.pid, backend.key));
    }

    /// **Two sessions get two pids and two keys**, which is the whole point of the registry: while
    /// `BackendKeyData` was `0, 0` every session was the same session.
    #[test]
    fn each_session_is_told_something_different() {
        let one = register();
        let two = register();
        assert_ne!(one.pid, two.pid);
        assert_ne!(
            one.key, two.key,
            "two keys from one process collided, which a PRF should not do"
        );
        // Cancelling one leaves the other alone.
        assert!(cancel(one.pid, one.key));
        assert!(!two.cancel.load(Ordering::Relaxed));
        deregister(one.pid);
        deregister(two.pid);
    }

    /// **The key does not come from the seeded generator.** A `Pcg32` seeded from the clock and the
    /// pid gives the same sequence to anyone who can guess both; this asserts only what can be
    /// asserted cheaply — that successive keys differ and are not a counter — but the property it
    /// stands for is in `fresh_key`'s doc comment.
    #[test]
    fn keys_are_not_a_sequence() {
        let keys: Vec<u32> = (0..8).map(|_| fresh_key()).collect();
        for pair in keys.windows(2) {
            assert_ne!(pair[0], pair[1], "two keys in a row were equal");
            assert_ne!(
                pair[1].wrapping_sub(pair[0]),
                1,
                "keys advanced by one, which is a counter and not a secret"
            );
        }
    }
}
