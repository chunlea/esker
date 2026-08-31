//! The time machine's verbs, lowered.
//!
//! Each is spelled as a **function call**, which is not a stylistic choice: it is the only spelling
//! a real PostgreSQL 19 parses. `docs/plans/phase-6d.md` §1 has the measurement — `CHECKPOINT
//! <name>` and `SELECT ... AS OF CHECKPOINT '<name>'` are `42601` on that server and unreadable to
//! `sqlparser`, so taking either would put this node outside contract C1's boundary in the one
//! direction the contract does not police, and would do it inside ADR 0014's containment.
//!
//! `pg_export_snapshot()` is PostgreSQL's own, and it already means this: take a snapshot, hand
//! back a token, let another transaction read at it. The named variant is the one divergence, and
//! it is a superset — PostgreSQL's exported snapshot lives only while the exporting transaction is
//! open, and a checkpoint outlives its session until retention passes it, which is what makes it a
//! checkpoint rather than a handle.

/// One time-machine verb.
///
/// Each runs in a transaction like any other statement, which is why these are `plan::Statement`
/// variants rather than session statements: naming a checkpoint writes a record, and a record is
/// written the way every record here is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeMachineVerb {
    /// `pg_export_snapshot()`, and `esker_checkpoint('<name>')` for the named variant.
    ///
    /// **Free**, and the reason is structural rather than an optimisation: it writes one small
    /// record and *nothing else* — no snapshot, no copy, no flush. A checkpoint is a number, and
    /// the data it refers to is kept by retention whether anybody named it or not.
    ExportSnapshot {
        /// `None` for the anonymous PostgreSQL verb, whose token carries the timestamp itself and
        /// needs no record at all.
        name: Option<String>,
    },
    /// `esker_drop_checkpoint('<name>')`. Forgets the name; the versions it named are retention's.
    DropCheckpoint {
        /// The checkpoint to forget.
        name: String,
    },
    /// `SELECT * FROM esker_checkpoints()`. A name you cannot list is a name you cannot use.
    ListCheckpoints,
    /// `SELECT esker_flashback('<table>', '<snapshot>')` — put a table back, by writing forwards.
    ///
    /// ADR 0021 Decision 3's fourth verb. **Compensating writes, never a rewrite of history**: the
    /// difference between now and the target is written as an ordinary transaction at a fresh
    /// `commit_ts`, so every version that existed before is still there and still readable `AS OF`
    /// an instant before the correction. An undo is itself undoable, and the audit trail survives
    /// the fix — which is the whole argument, because the alternative destroys evidence exactly
    /// when somebody is trying to work out what happened.
    Flashback {
        /// The table to put back, folded.
        table: String,
        /// The snapshot to put it back to, as a token or a checkpoint name — the same namespace
        /// `SET TRANSACTION SNAPSHOT` and `esker_diff` read.
        to: String,
    },
    /// `SELECT * FROM esker_schema_jobs()` — what schema changes are in flight, and where each is.
    ///
    /// The `psql`-visible progress ADR 0020 asks for: a human watching a `CREATE INDEX
    /// CONCURRENTLY` sees the state advance, which is the only way to tell "slow" from "stuck".
    ListSchemaJobs,
    /// `SELECT * FROM esker_columnar_replicas()` — which tables want a columnar copy, and how
    /// many ([ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) Decision 5).
    ///
    /// A setting nobody can read back is a setting nobody can check, and this one is acted on by
    /// a different process entirely — so seeing what the catalog says is the only way to tell
    /// "PD has not got to it yet" from "the ALTER never landed".
    ListColumnarReplicas,
    /// `SELECT esker_schema_step('<index>')` — take the next step of one job, and say what it did.
    ///
    /// **The step clock made explicit.** PD publishes the interval a step must wait
    /// ([ADR 0028](../../../docs/adr/0028-the-schema-lease.md)); this is the step itself, so that
    /// what waits and what acts are separable — which is what makes the whole state machine
    /// testable without a timer, and what lets an operator drive a stuck job by hand.
    SchemaStep {
        /// The index whose job to step.
        index: String,
    },
    /// `SELECT * FROM esker_diff('<table>', '<from>'[, '<to>'])`.
    ///
    /// Two scans and a merge, and **not a changelog** — saying so matters, because "diff" invites
    /// the other reading. It compares two *states*: a key written and then written back is
    /// invisible to it, and five updates look like one. A real changelog is the Raft log, and
    /// reading it is a different feature.
    Diff {
        /// The table, folded like any other relation name.
        table: String,
        /// The older snapshot, as a token or a checkpoint name.
        from: String,
        /// The newer snapshot, or `None` for the present — which is the common case and the
        /// reason this is an arity rather than a magic value: a checkpoint may legitimately be
        /// called `now`, and a string that sometimes means a name and sometimes means the clock
        /// would be a trap.
        to: Option<String>,
    },
}

impl TimeMachineVerb {
    /// The command tag. Every one of these is reached through a `SELECT`, so every one is
    /// PostgreSQL's `SELECT <count>` — built by the executor, which knows the count.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        "SELECT"
    }
}
