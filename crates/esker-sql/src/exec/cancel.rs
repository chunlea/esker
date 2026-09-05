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

use std::cell::Cell;
use std::time::Instant;

use crate::error::{Result, SqlError};

thread_local! {
    /// When the statement on this thread must stop, or `None` for "as long as it takes".
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
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

/// `57014` once this statement's deadline has passed, and `Ok` every other time.
///
/// Call it at the top of any loop that can run long. It is a thread-local read and a clock
/// comparison, so a per-page or per-10ms call costs nothing worth measuring; calling it per *row*
/// would be a different question and is not what any caller does.
pub(super) fn check() -> Result<()> {
    match DEADLINE.with(Cell::get) {
        Some(deadline) if Instant::now() >= deadline => Err(SqlError::StatementTimeout),
        _ => Ok(()),
    }
}
