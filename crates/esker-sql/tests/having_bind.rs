//! **A bind parameter takes its type from the aggregate on the other side of the operator.**
//!
//! `finder_test#test_find_with_group_and_sanitized_having_method`:
//!
//! ```ruby
//! Developer.group(:salary).having("sum(salary) > ?", 10000).select("salary").to_a
//! ```
//!
//! which reaches the wire as `… GROUP BY "developers"."salary" HAVING (sum(salary) > $1)` with one
//! bind. This node answered **zero rows** where PostgreSQL 19 answers three — a wrong answer, not a
//! missing feature, and the quietest kind: the client is told the query worked.
//!
//! # One cause, three faces
//!
//! An aggregate's type was not known where an expression's type is asked for, and each caller of
//! that answer failed differently:
//!
//! * the **parameter's type** fell back to `text`, so `sum(salary) > $1` became a text comparison
//!   whose left operand rendered as nothing at all — false for every group, and `<` true for every
//!   group;
//! * the **operator check** let it through: `sum(salary) > 'x'::text` answered `false` where
//!   `80000::int8 > 'x'::text` was already this node's own byte-identical `42883`. The aggregate was
//!   the hole in a check that was otherwise right;
//! * an aggregate **inside a larger expression** was `XX000 internal error: an aggregate reached
//!   expr_type without being rewritten` — `SELECT (sum(salary) + 0) > $1`, which a real server
//!   answers `t`.
//!
//! `tests/corpus/pg19_having_bind.txt` carries the measurement, including the rule read off
//! `pg_prepared_statements.parameter_types` and the two rows that agree for the wrong reason.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

/// What this node answers differently, and why.
const DIVERGENCES: bind::Divergences = bind::Divergences {
    types: &[],
    answers: &[(
        "SELECT max(name), max(name) > $1 FROM hb_developers",
        "**The collation, not the aggregate.** `max` over text is `ORDER BY … DESC LIMIT 1` and the \
         order is the collation's: this node has `C` and `POSIX` only (ADR 0076), where `P` (0x50) \
         sorts below `f` (0x66) and the maximum of the five names is `fixture_4`. The oracle \
         container is `en_US.utf8`, which ignores case first and answers `Poor dev`. The boolean \
         beside it agrees on both, which is what this row is here to check — the parameter is \
         `text` because the aggregate is, and the comparison is a text one on both servers.",
        "pg19_having_bind.txt:78",
    )],
};

#[test]
fn every_aggregate_bind_answer_is_postgresql_19_s() {
    let checked = bind::replay(include_str!("corpus/pg19_having_bind.txt"), &DIVERGENCES);
    assert!(
        checked >= 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
