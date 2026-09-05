//! A `date` column assigned a `timestamp with time zone` — the assignment cast `insert_all` needs.
//!
//! Run 51's ranking #4: `column "…" is of type date but expression is of type timestamp with time
//! zone`, **57 tests over 2 files**. No test writes that cast; it is `ActiveRecord` filling the
//! timestamp columns of `books`, where `updated_on` is a `t.date` and `created_at`/`updated_at` are
//! `datetime`, with one `CURRENT_TIMESTAMP` for all three.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **The session zone is spelled differently and means the same thing.** A real server
        // reports the container's `Etc/UTC`; this node reports `UTC`, which is the only zone it
        // has (`crate::parameter`).
        (
            "SELECT 'r', current_setting('TimeZone')",
            "the one zone this node has is spelled UTC and the container's is Etc/UTC",
            "pg19_assignment_cast_date.txt:40",
        ),
        // **`pg_cast` is not a relation here.** Three statements, and they are the *evidence* for
        // this unit rather than part of it: `castcontext` is `'a'` for both timestamp types to
        // `date` and `'i'` in the reverse direction, which is what says the cast is allowed when
        // assigning and not when combining. The rule is now in `exec::assign::coerce`; the table a
        // client could read it out of is a catalog view of its own.
        (
            "SELECT 'r', castsource::regtype, casttarget::regtype, castcontext, castmethod FROM \
             pg_cast WHERE casttarget = 'date'::regtype ORDER BY castsource::regtype::text",
            "pg_cast is not built",
            "pg19_assignment_cast_date.txt:42",
        ),
        (
            "SELECT 'r', castsource::regtype, casttarget::regtype, castcontext FROM pg_cast WHERE \
             castsource = 'date'::regtype ORDER BY casttarget::regtype::text",
            "pg_cast is not built",
            "pg19_assignment_cast_date.txt:43",
        ),
        // **A per-row cast still has only `text` as a target**, which is the standing debt this
        // lane has carried since the array unit — `'…'::date` on a *constant* folds at plan time
        // and works, and `CURRENT_TIMESTAMP::date` cannot, because the value is not known until
        // the row is. The conversion itself now exists (`crate::value::date::from_micros`) and
        // what is missing is a plan node to apply it per row; that is a unit of its own and it
        // would close both of these.
        (
            "SELECT 'r', count(*) FROM bk WHERE updated_on = CURRENT_TIMESTAMP::date",
            "a per-row cast has only text as a target",
            "pg19_assignment_cast_date.txt:54",
        ),
        (
            "SELECT 'r', CURRENT_DATE = CURRENT_TIMESTAMP::date FROM bk2",
            "a per-row cast has only text as a target",
            "UNMEASURED",
        ),
        // **The one thing about this cast that is deliberately not reproduced.** `timestamptz` ->
        // `date` asks which calendar day an instant falls on *here*, so the answer depends on the
        // session zone — the capture takes one instant in `UTC` and in `Pacific/Auckland` and gets
        // two different dates. This node honours `TimeZone` only where it means UTC, so it refuses
        // the `SET` rather than accepting it and answering as though it were UTC, which would be a
        // wrong date rather than a refusal. The five statements after it are that refusal's wake:
        // the whole section is one `SAVEPOINT`, so its row is rolled back on both sides and the
        // counts that follow agree.
        (
            "SET TimeZone = 'Pacific/Auckland'",
            "this node honours TimeZone only where it means UTC, and refuses rather than \
             answering a wrong calendar day",
            "pg19_assignment_cast_date.txt:68",
        ),
    ],
};

#[test]
fn every_assignment_cast_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_assignment_cast_date.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 35,
        "only {checked} statements ran; the corpus did not load"
    );
}
