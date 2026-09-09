//! `must appear in the GROUP BY clause` — run 57's third row, 6 tests over 2 files.
//!
//! **A functional dependency, and the capture confirms it is the whole row**: PostgreSQL accepts a
//! bare column when the `GROUP BY` contains that table's primary key, because grouping by a key
//! means one row per group of that table and every other column of it therefore has exactly one
//! value. `ActiveRecord` writes it constantly — `group(:id)` on a relation selecting `*`.
//!
//! It is **per table**. `GROUP BY f.id` frees every column of `f` and none of `a`, in the same
//! select list; an implementation that read it as "some key is grouped" would accept a query that
//! really is ambiguous.
//!
//! # Run 104: the clause it was not applied to
//!
//! `ORDER BY`, which is the clause `ActiveRecord` actually reaches the dependency through.
//! `Company.includes(:comments).order(:rating).ids` sends
//!
//! ```text
//! SELECT "companies"."id" FROM "companies"
//!   LEFT OUTER JOIN "comments" ON "comments"."company" = "companies"."id"
//!   GROUP BY "companies"."id" ORDER BY "companies"."rating" ASC
//! ```
//!
//! and the ordered column is nowhere else in the statement — not in the select list, not in the
//! `HAVING` — so the widening walked past it and this node answered `42803` for a query a real
//! server answers with fifteen rows
//! (`calculations_test#test_ids_with_includes_and_non_primary_key_order`).
//!
//! The corpus's ORDER BY section pins the rest of the family with it, all measured: a positional
//! grouping key frees the table, a composite key frees it only when every column is grouped, a
//! `UNIQUE NOT NULL` column determines nothing (PostgreSQL reads the primary key and nothing
//! else), and the other table's column stays bare in `ORDER BY` exactly as in the select list.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the two tables it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_group_by_key_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_group_by_key.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 80, "the corpus shrank: {checked} statements");
}
