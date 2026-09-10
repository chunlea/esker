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
        // and neither was a time-zone gap: the zone reaches the renderer and the second column of
        // the first row proved it — `2011-01-02 12:30:00+13` agreed exactly. What did not agree
        // was what the cast is *applied to*.
        //
        // **The `SELECT` half is closed, by `debts-v1.1.md` #42's remaining half.**
        // `'2011-01-01 23:30:00+00'::timestamptz::date` was `2011-01-01` here and `2011-01-02` on
        // a real server, because the fold rendered the instant with the boot output function and
        // read the day off that text. `lower_cast` now keeps the `Cast` node whenever the
        // operand's type is not the target's, so the conversion happens in `cursor::evaluate`,
        // under the session — which is where the **column** form was right all along
        // (`tests/time_zone.rs`). The entry is gone rather than reworded: a listed divergence
        // that starts agreeing is deleted, and this one was named after its cause, so its cause
        // closing is the whole of it.
        //
        // **The `INSERT` is not closed, and it is now clear that it never was the same row.**
        // The two below read what an earlier `INSERT` *stored*, and the value it stored is the
        // UTC day: that write took the operand through the assignment path, not through the
        // comparison the row above took. `tests/interval_style.rs`'s `'1 mon'::interval::text`
        // is a third site and also still open. The note here used to say one fix closes all
        // three; one fix closed one, which is the useful correction — they share a *symptom*,
        // a value rendered without the session, and not a caller.
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
