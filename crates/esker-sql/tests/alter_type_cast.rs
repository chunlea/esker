//! Which pairs `ALTER COLUMN … TYPE` converts **without** a `USING`, measured across the family.
//!
//! Run 70 ranked four rows of `column "…" cannot be cast automatically`, 18 tests over four files:
//! 7 to `double precision`, 6 to `timestamp without time zone`, 3 to `timestamp(N) without time
//! zone` and 2 to `character varying`. They are one bug — `converts_implicitly` was narrower than
//! PostgreSQL's assignment casts — and `migration/compatibility_test.rb` alone raises 14 of them,
//! from two statements `ActiveRecord` sends with no `USING` at all:
//!
//! ```text
//! ALTER TABLE "tests"         ALTER COLUMN "some_id"      TYPE float      -- integer -> float8
//! ALTER TABLE "more_testings" ALTER COLUMN "published_at" TYPE timestamp  -- date    -> timestamp
//! ```
//!
//! **The second one is not the pair it reads as.** `published_at` is created `date` by the
//! migration above it, not `timestamp` — a `timestamp -> timestamp` no-op was never the failing
//! case, and assuming it was would have fixed nothing.
//!
//! [ADR 0060](../../../docs/adr/0060-a-using-clause-is-a-licence-not-an-expression.md) is unchanged
//! by this: a `USING` is still a licence rather than an expression. What changes is the set of
//! pairs that need no licence, and the corpus is what fixes it — a rule reasoned about instead
//! would have been wrong at two edges this file pins.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_alter_type_cast_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_alter_type_cast.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 120,
        "only {checked} statements ran; the corpus did not load"
    );
}
