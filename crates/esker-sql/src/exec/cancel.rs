//! Whether the statement running on this thread has been told to stop.
//!
//! **Why a thread-local and not a parameter.** A statement's deadline is needed in every loop long
//! enough to be worth interrupting — the scan walk, `pg_sleep`, and the row wait — and those sit
//! behind twenty-odd call sites that share no argument. Threading a deadline through them would put
//! the plumbing everywhere and the decision nowhere. The executor runs one statement to completion
//! on one blocking thread (`parameter.rs` says so, and it is why `statement_timeout` was refused
//! for so long), so "this thread's statement" is exactly the right scope, and the check sites stay
//! explicit even though the value is ambient.
//!
//! The deadline is installed by [`until`] for the length of one `Executor::execute` and **restores
//! whatever was there before**, so a statement that runs another statement — a function body — puts
//! the outer one back rather than leaving the inner one's clock running.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::error::{Result, SqlError};

thread_local! {
    /// When the statement on this thread must stop, or `None` for "as long as it takes".
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
    /// Who this thread's statement belongs to: its pid, and the flag another session sets to
    /// cancel it.
    ///
    /// **Installed per statement, not per connection.** Each statement is handed to
    /// `tokio::task::spawn_blocking`, so consecutive statements of one session run on *different*
    /// pool threads and anything installed once at connect time would be invisible to all of them.
    /// The pid rides along because `pg_backend_pid()` is evaluated deep in the expression
    /// evaluator, which is handed a transaction and a catalog and has no idea who is asking.
    static SESSION: RefCell<Option<(u32, Arc<AtomicBool>)>> = const { RefCell::new(None) };
}

/// Installs `deadline` until the returned guard is dropped.
pub(super) fn until(deadline: Option<Instant>) -> Guard {
    Guard(DEADLINE.with(|cell| cell.replace(deadline)))
}

/// Puts the previous deadline back. Holds the *previous* value, not the current one.
pub(super) struct Guard(Option<Instant>);

impl Drop for Guard {
    fn drop(&mut self) {
        DEADLINE.with(|cell| cell.set(self.0));
    }
}

/// Installs `flag` as this thread's cancellation flag until the guard is dropped, and **clears it
/// first**.
///
/// Clearing is the half that is easy to leave out: a `CancelRequest` that arrives while the session
/// is idle must not kill the *next* statement it runs. PostgreSQL cancels the query that is running
/// and nothing else, and a flag left set from a previous statement would cancel a statement nobody
/// asked about.
pub(crate) fn with_session(pid: u32, flag: Arc<AtomicBool>) -> FlagGuard {
    flag.store(false, Ordering::Relaxed);
    FlagGuard(SESSION.with_borrow_mut(|slot| slot.replace((pid, flag))))
}

/// The pid of the session running this thread's statement, for `pg_backend_pid()`.
pub(crate) fn current_pid() -> Option<u32> {
    SESSION.with_borrow(|slot| slot.as_ref().map(|(pid, _)| *pid))
}

/// Puts the previous flag back, for the same reason [`Guard`] does.
pub(crate) struct FlagGuard(Option<(u32, Arc<AtomicBool>)>);

impl Drop for FlagGuard {
    fn drop(&mut self) {
        SESSION.with_borrow_mut(|slot| *slot = self.0.take());
    }
}

/// `57014` once this statement's deadline has passed or somebody has cancelled it, and `Ok` every
/// other time.
///
/// Call it at the top of any loop that can run long. It is a thread-local read and a clock
/// comparison, so a per-page or per-10ms call costs nothing worth measuring; calling it per *row*
/// would be a different question and is not what any caller does.
pub(super) fn check() -> Result<()> {
    if SESSION.with_borrow(|slot| {
        slot.as_ref()
            .is_some_and(|(_, flag)| flag.load(Ordering::Relaxed))
    }) {
        return Err(SqlError::QueryCanceled);
    }
    match DEADLINE.with(Cell::get) {
        Some(deadline) if Instant::now() >= deadline => Err(SqlError::StatementTimeout),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{check, current_pid, until, with_session};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// A set flag is `57014`, and the sentence is the user-request one rather than the timeout's.
    #[test]
    fn a_set_flag_cancels_the_statement() {
        let flag = Arc::new(AtomicBool::new(false));
        let _guard = with_session(7, Arc::clone(&flag));
        assert!(check().is_ok(), "nothing has asked yet");

        flag.store(true, Ordering::Relaxed);
        let error = check().expect_err("the flag is set");
        assert_eq!(error.sqlstate(), "57014");
        assert_eq!(error.to_string(), "canceling statement due to user request");
    }

    /// **A cancellation that arrived while the session was idle does not kill the next statement.**
    ///
    /// The flag outlives one statement — it belongs to the session — so installing it for the next
    /// one has to clear it. Without that, a client that cancelled a query which had already
    /// finished would find its *following* query dead, which is a bug nobody would connect to the
    /// cancel they sent.
    #[test]
    fn a_stale_cancellation_does_not_carry_into_the_next_statement() {
        let flag = Arc::new(AtomicBool::new(true));
        let _guard = with_session(7, Arc::clone(&flag));
        assert!(
            check().is_ok(),
            "the flag was set before this statement began and must have been cleared"
        );
    }

    /// The deadline still answers `57014` with the timeout's own sentence, and the two do not
    /// shadow each other.
    #[test]
    fn a_deadline_and_a_flag_report_different_sentences() {
        // A deadline already in the past, without an unchecked subtraction: the clock near
        // process start is not guaranteed to be a millisecond old.
        let past = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap_or_else(Instant::now);
        let _clock = until(Some(past));
        let error = check().expect_err("the deadline is in the past");
        assert_eq!(
            error.to_string(),
            "canceling statement due to statement timeout"
        );
    }
}
