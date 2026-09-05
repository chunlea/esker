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
        // **Both of these came out from behind the `SET TimeZone` refusal** that ADR 0080 closed,
        // and neither is a time-zone gap: the zone reaches the renderer and the second column of
        // the first row proves it — `2011-01-02 12:30:00+13` agrees exactly. What does not agree
        // is what the cast is *applied to*.
        //
        // **A cast over a literal is folded at lowering, where there is no session.**
        // `'2011-01-01 23:30:00+00'::timestamptz::date` is `2011-01-02` on a real server and
        // `2011-01-01` here, because the fold renders the instant with the boot output function
        // and reads the day off that text. The **column** form is right — `cursor::evaluate`'s
        // cast renders under the session (`tests/time_zone.rs`) — so this is the literal fold and
        // not the conversion, and it is the same gap `tests/interval_style.rs` declares for
        // `'1 mon'::interval::text`. One fix closes both.
        (
            "SELECT 'r', '2011-01-01 23:30:00+00'::timestamptz::date, '2011-01-01 \
             23:30:00+00'::timestamptz",
            "a cast over a literal is folded at lowering, where there is no session to render the \
             instant in",
            "pg19_assignment_cast_date.txt:69",
        ),
        // **The `INSERT` above is taken now** — `has_assignment_cast` learned the pair and
        // `value::assignment_cast` learned the zone — and these two reads of what it stored are
        // the *same literal fold* as the row above, one statement later. The value written is the
        // UTC day because the operand `'2011-01-01 23:30:00+00'::timestamptz` was folded at
        // lowering, where there is no session; an instant that arrives as an **expression** takes
        // the session's day, which `tests/time_zone.rs` asserts against a column. One fix closes
        // this, the row above it, and `tests/interval_style.rs`'s literal cast.
        (
            "SELECT 'r', name, updated_on FROM bk WHERE name = 'tz probe'",
            "the instant was folded to a date at lowering, where there is no session zone",
            "pg19_assignment_cast_date.txt:71",
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
