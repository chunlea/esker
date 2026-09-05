//! Which session is which, so that a cancellation can name one.
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
pub(crate) struct Backend {
    /// What `BackendKeyData` announced and `pg_stat_activity` will report.
    ///
    /// `u32` because that is what the protocol carries; `pg_stat_activity`'s column is `int4` and
    /// converts at that boundary, which is the only place the sign means anything.
    pub(crate) pid: u32,
    /// The secret that makes a `CancelRequest` for this pid this client's to send.
    pub(crate) key: u32,
    /// Set by a cancellation, read by `crate::exec::cancel` between units of work.
    pub(crate) cancel: Arc<AtomicBool>,
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
pub(crate) fn register() -> Backend {
    // Small positive integers, as a real server's pids are. Uniqueness within this process is all
    // a `CancelRequest` needs, since the key is what proves the right to use one.
    static NEXT_PID: AtomicU32 = AtomicU32::new(1);
    let backend = Backend {
        pid: NEXT_PID.fetch_add(1, Ordering::Relaxed),
        key: fresh_key(),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    if let Ok(mut live) = backends().lock() {
        live.insert(backend.pid, backend.clone());
    }
    backend
}

/// Forgets a session that has gone.
pub(crate) fn deregister(pid: u32) {
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
pub(crate) fn cancel(pid: u32, key: u32) -> bool {
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
