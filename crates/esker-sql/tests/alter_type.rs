//! `ALTER TYPE` — rename the type, add a label, rename a label.
//!
//! Run 70's row, 4 tests in `enum_test.rb`, and exactly three statements `ActiveRecord` sends:
//!
//! ```text
//! ALTER TYPE "mood" RENAME TO "feeling"
//! ALTER TYPE "mood" ADD VALUE 'angry' BEFORE 'ok'
//! ALTER TYPE "mood" RENAME VALUE 'ok' TO 'okay'
//! ```
//!
//! **`ADD VALUE` in the middle rewrites rows here and moves nothing on a real server**, and the
//! capture is what shows why: `pg_enum.enumsortorder` is a **`real`**, so PostgreSQL gives the new
//! label sort order **1.5** and leaves every stored row alone. This node stores an enum value as
//! the *ordinal of its label's position* ([ADR 0050](../../../docs/adr/0050-a-user-defined-type-is-a-value.md)),
//! which is what makes ordering, grouping and indexing the ordinal's — and it is precisely the
//! representation that cannot leave those rows alone: a label inserted before the end shifts every
//! later ordinal, and a node that skipped the rewrite would answer the **wrong label** for every
//! row written before the `ALTER`.
//!
//! The two error codes are a pair worth keeping: a label that is not there is `22023` and one that
//! is already taken is `42710`. PostgreSQL calls the first an invalid *parameter*, because the
//! label is an argument to the statement rather than an object being looked up.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own type and table.
const CORPUS_FIXTURE: &[&str] = &[];

const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `enumlabel` and `typname` are `name` on a real server and `text` here, with identical
    // characters — the standing trade every `pg_catalog` column makes.
    types: &[
        "SELECT 'r', enumlabel, enumsortorder FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid WHERE t.typname = 'at_mood' ORDER BY enumsortorder",
        "SELECT 'r', enumlabel FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid WHERE t.typname = 'at_mood' ORDER BY enumsortorder",
        "SELECT 'r', typname FROM pg_type WHERE typname IN ('at_mood','at_feeling')",
    ],
    answers: &[(
        "SELECT 'r', enumlabel, enumsortorder FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid WHERE t.typname = 'at_mood' ORDER BY enumsortorder",
        "**The labels and their order agree; the numbers do not.** PostgreSQL gives the inserted \
         label `1.5` and leaves the others at 1, 2, 3 — a float is what lets it put one *between* \
         two without touching a row. This node's number **is** the label's position, because that \
         is what every stored row holds (ADR 0050), so the insert renumbers to 1, 2, 3, 4 and \
         rewrites the rows to match.\n\nWhat these numbers encode is the order, and the order is \
         identical — every `ORDER BY` on the enum, and every label read back, agrees on this \
         corpus. A client comparing an `enumsortorder` against a remembered number would see the \
         difference, and nothing in `ActiveRecord` does: it sorts inside `array_agg` and never \
         reads the value.",
        "pg19_alter_type.txt:6",
    )],
};

#[test]
fn every_alter_type_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_alter_type.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}
