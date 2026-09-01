//! `SET`, `SHOW` and `RESET`, lowered — the three statements that change what a session *is*
//! rather than what the store holds.
//!
//! Everything here is PostgreSQL's own grammar, which is the whole point (`docs/plans/phase-6d.md`
//! §1). A time machine wants a per-session read timestamp, PostgreSQL has two ways to carry one —
//! a namespaced custom GUC and `SET TRANSACTION SNAPSHOT` — and taking both means `crate::parse`
//! needs no grammar of its own and a real server accepts every statement a user writes here.
//!
//! A `SET` this node does not know stays `0A000 SET is not supported`, exactly as before. The set
//! that is executed grows one name at a time; the set that is mishandled stays empty — which is
//! what [`crate::parameter`] is for: it holds, per parameter, the values this node *means* rather
//! than the values it will swallow.

/// One session statement, lowered.
///
/// Not run inside a transaction, which is why it is its own variant of [`crate::plan::Statement`]
/// rather than another statement the executor wraps: `SET TRANSACTION SNAPSHOT` *replaces* the
/// transaction the session is in, and a `SET` outside a block must not open one at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatement {
    /// `SET esker.read_as_of = '...'`, or `= DEFAULT` / `RESET`, which carry `None`.
    SetReadAsOf {
        /// What the user wrote, unresolved. `None` clears the setting.
        ///
        /// The *text* is kept as well as the timestamp it resolves to, because `SHOW` hands back
        /// what was set and not what it became — PostgreSQL's behaviour, and the useful one: a
        /// user who wrote `'-1h'` is told `-1h`.
        value: Option<String>,
        /// `SET LOCAL`: undone when the transaction ends, whichever way it ends.
        local: bool,
    },
    /// `SHOW esker.read_as_of`.
    ShowReadAsOf,
    /// `SET TRANSACTION SNAPSHOT '<id>'`.
    SetSnapshot(String),
    /// `SET <parameter> = <value>`, for one of the parameters in [`crate::parameter`].
    ///
    /// The name is carried as written and looked up when the statement *runs*, not when it is
    /// lowered: a real server parses `SET client_min_messages TO 'bogus'` and fails it at execute
    /// time, so a `Parse` that refused it would answer a message earlier than PostgreSQL does.
    SetParameter {
        /// What the user wrote. Folded and looked up by the executor.
        name: String,
        /// The text assigned, or `None` for `TO DEFAULT` — which is `RESET` by another name and
        /// the same operation, measured.
        value: Option<String>,
    },
    /// `SHOW <parameter>`, for one of the same.
    ShowParameter(String),
}

impl SessionStatement {
    /// The command tag PostgreSQL reports.
    ///
    /// `SHOW` is the odd one: it returns a row, and its tag is still `SHOW` with no count, which
    /// is not the shape any other row-returning statement has here.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            SessionStatement::SetReadAsOf { .. }
            | SessionStatement::SetSnapshot(_)
            | SessionStatement::SetParameter { .. } => "SET",
            SessionStatement::ShowReadAsOf | SessionStatement::ShowParameter(_) => "SHOW",
        }
    }
}
