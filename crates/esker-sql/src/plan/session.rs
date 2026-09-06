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
    /// `SET [LOCAL|SESSION] SESSION AUTHORIZATION DEFAULT`: the session's own user, which is what
    /// it already is.
    ///
    /// A **no-op that is not a lie**: this node has no roles, so there is nothing to switch away
    /// from and `DEFAULT` asks for exactly that. A named role is `22023` in the lowering and never
    /// becomes one of these.
    /// `SET SESSION AUTHORIZATION <name>`, or `DEFAULT`/`RESET` as `None`.
    ///
    /// **The name travels to execution rather than being judged here.** Lowering refused every
    /// name outright — correct while the node had no roles, and a lie the moment it did: the
    /// session that had just created a role was told it does not exist, because the refusal is
    /// upstream of the catalog and never asks it.
    SetSessionAuthorization {
        /// The role named, or `None` for `DEFAULT` — which `RESET SESSION AUTHORIZATION` is
        /// rewritten to, PostgreSQL documenting the two as one statement.
        name: Option<String>,
        /// `SET LOCAL`: undone when the transaction ends, and by a rollback to a savepoint taken
        /// before it.
        ///
        /// **Never a `SET LOCAL` that outlives its transaction.** That is a wrong answer rather
        /// than a missing feature, and ADR 0031 ranks a refusal above one — so this is carried on
        /// the same transaction-scoped restore `esker.read_as_of` already uses rather than
        /// accepted and forgotten.
        local: bool,
    },
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
    /// `RESET ALL`: every parameter back to its boot value in one statement.
    ///
    /// Its own variant rather than a loop of `SetParameter { value: None }`, because the set it
    /// resets is *what the session has set* and only the executor knows that — and because a real
    /// server's `RESET ALL` leaves the ones it cannot change alone rather than failing on them.
    ResetAll,
    /// `DISCARD ALL` and its three narrower spellings.
    ///
    /// **What a pooled connection is reset with**, and not a statement any test writes:
    /// `postgresql_adapter.rb:392` sends it when the adapter returns a connection to the pool, so
    /// it lands on every file that does. It cannot run inside a transaction block (`25001`), which
    /// is checked where `BEGIN` is rather than here — the session knows whether one is open and a
    /// plan does not.
    Discard(DiscardTarget),
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
    /// `SHOW SESSION AUTHORIZATION`, or the one-word GUC spelling of it.
    ///
    /// **Not a [`SessionStatement::ShowParameter`], deliberately.** The value lives on the
    /// executor (`SET SESSION AUTHORIZATION` writes it) and not in the parameter map, and adding
    /// it to that map would open a second door — a generic `SET session_authorization = 'bob'`
    /// writing the map while the field it is supposed to be kept nothing. One state, one door.
    ShowSessionAuthorization,
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
            | SessionStatement::SetSessionAuthorization { .. }
            | SessionStatement::SetParameter { .. } => "SET",
            // **`RESET`, not `SET`.** A `RESET ALL` reports its own verb, which is what a client
            // reading the command tag expects; `RESET <name>` is a `SetParameter` with no value
            // and reports `SET`, exactly as a real server does. Measured, both.
            SessionStatement::ResetAll => "RESET",
            // **The tag names the target**, and `TEMPORARY` reports as `TEMP` — measured, all
            // four. A tag of a bare `DISCARD` or a constant `DISCARD ALL` would be a client told
            // it reset more than it asked for.
            SessionStatement::Discard(target) => match target {
                DiscardTarget::All => "DISCARD ALL",
                DiscardTarget::Plans => "DISCARD PLANS",
                DiscardTarget::Sequences => "DISCARD SEQUENCES",
                DiscardTarget::Temp => "DISCARD TEMP",
            },
            SessionStatement::ShowReadAsOf
            | SessionStatement::ShowParameter(_)
            | SessionStatement::ShowSessionAuthorization => "SHOW",
        }
    }
}

/// Which session state a `DISCARD` throws away.
///
/// Four targets, and the capture measured each one against what it must *not* touch —
/// `DISCARD PLANS` leaves the advisory locks and the temp table, `DISCARD SEQUENCES` leaves
/// everything but `currval`, and only `ALL` releases a lock. An implementation that treated them
/// as one word would pass the `ALL` lines and quietly break a pooled connection's other resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardTarget {
    /// `DISCARD ALL`: every other target at once, plus prepared statements and every `SET`.
    All,
    /// `DISCARD PLANS`: cached plans. This node caches none, so it is a no-op — and the capture
    /// says a real server's is nearly one too, since it touches nothing else.
    Plans,
    /// `DISCARD SEQUENCES`: this session's `currval` values, which become *undefined* again rather
    /// than stale — `55000` on the next `currval`, the same answer as a fresh connection.
    Sequences,
    /// `DISCARD TEMP` / `DISCARD TEMPORARY`: this session's temporary tables, dropped —
    /// records, rows and the schema they live in ([ADR 0054]). The session carries on with no
    /// temp schema, so the next `CREATE TEMP TABLE` allocates a fresh one.
    ///
    /// [ADR 0054]: ../../../docs/adr/0054-a-temporary-table-is-a-relation-in-a-schema-that-belongs-to-one-session.md
    Temp,
}

impl DiscardTarget {
    /// The word, for the command tag and for a message.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            DiscardTarget::All => "ALL",
            DiscardTarget::Plans => "PLANS",
            DiscardTarget::Sequences => "SEQUENCES",
            DiscardTarget::Temp => "TEMP",
        }
    }
}
