//! **Which expressions carry a typmod into the `RowDescription`.**
//!
//! `docs/plans/debts-v1.1.md` #28. A `character(4)` column is `character(4)` to a client, and an
//! expression over it was `bpchar` here whatever it was there. The row was found on
//! `nullif(c, 'x')` and a second reader turned up while closing #20: five statements in
//! `tests/numeric.rs` were declared because `1.0::numeric(10,3)` described itself as bare
//! `numeric`.
//!
//! The row was sized "medium — a type-surface change, the same shape as #19, and touches every
//! reader of a `RowDescription`". It is neither. `OutputColumn` already carries a `typmod` and
//! `plan::Expr::Cast` already holds the one it was written with; what was missing is the rule
//! saying which expressions pass one along, and that rule lives in one function.
//!
//! # The rule, measured
//!
//! ```text
//! a plain column           its own                  SELECT c            character(4)
//! a cast                   the cast's own           c::char(2)          character(2)
//! NULLIF                   its left argument's      nullif(c, 'x')      character(4)
//!   ... unless the comparison changed the type:     nullif(v, 'x')      text
//! COALESCE/GREATEST/LEAST  every input's, if they all agree; else none
//! CASE                     none, always
//! everything else          none                     c || 'x', min(c), n + 1
//! ```
//!
//! **Two oracles disagree about `CASE`**, and the corpus follows the one a client reads:
//! `CREATE TABLE AS` gives its column the *first branch's* modifier, `\gdesc` on the same
//! expression gives none. The `RowDescription` is what this row is about.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **Nothing.** Four rows stood here and none of them was a typmod: they were a
    // `character(n)`'s trailing blanks reached by a comparison, an operator and two functions,
    // which is `docs/plans/debts-v1.1.md` #31 and closed by `exec::query::read_as_text` in the
    // commit after this file landed.
    answers: &[],
};

#[test]
fn every_declared_typmod_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_expression_typmod.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus is not being read"
    );
}

/// **The two readers of one expression agree** — `debts-v1.1.md` #33.
///
/// The corpus above pins what a **column** says: `greatest(c, 'x')` over a `character(4)` is a
/// `bpchar`, measured, and it was right the whole time. `pg_typeof` of the same expression
/// answered `text`, and no corpus row could catch that, because a corpus compares one reader at a
/// time and this defect is only visible when both are asked about the same expression.
///
/// The cause was one name missing from one list. `resolve` reads a `bpchar` argument as `text` on
/// the way into a catalog function — with the blanks stripped, which is what makes `length(c)`
/// answer 1 and not 4 — and the functions excluded from that are the ones that do not consume the
/// value as text. `pg_typeof` does not consume it at all: it reports the argument's type, so
/// reading it as `text` first made it report the coercion this crate had just inserted rather than
/// the expression the user wrote.
///
/// **It was never only `GREATEST`.** The row was written from the one statement that found it;
/// `LEAST`, `COALESCE`, and `greatest(c, c)` with no literal anywhere in it were all the same
/// answer, which is what says the defect was in the argument and not in the ladder.
#[test]
fn pg_typeof_and_the_row_description_agree_about_a_bpchar() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1tm (c character(4), v character varying(4))",
        "INSERT INTO g1tm VALUES ('x', 'x')",
    ]);
    for (expression, printed, oid) in [
        // The statement the row was written from, and the three it did not name.
        ("greatest(c, 'x')", "character", 1042),
        ("least(c, 'x')", "character", 1042),
        ("coalesce(c, 'x')", "character", 1042),
        // **No literal at all**, which is what says this was never about the common-type ladder.
        ("greatest(c, c)", "character", 1042),
        // The adorned literal does not change it either: `text -> bpchar` is implicit too.
        ("greatest(c, 'x'::text)", "character", 1042),
        // And the type next door, which was right all along — `read_as_text` only ever wrapped a
        // `bpchar`, so a `varchar` never went through the coercion that caused this.
        ("greatest(v, 'x')", "character varying", 1043),
    ] {
        // What `pg_typeof` says.
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof({expression}) FROM g1tm")),
            vec![vec![printed.to_owned()]],
            "pg_typeof({expression})"
        );
        // What the `RowDescription` says, about the same expression in the same session.
        let outcome = node.run(&format!("SELECT {expression} FROM g1tm")).unwrap();
        let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
            panic!("{expression}: no rows");
        };
        assert_eq!(
            fields[0].type_oid, oid,
            "the wire and pg_typeof must agree about {expression}"
        );
    }
    // **The value is unchanged by any of this**, which is what says the defect was a report and
    // never an answer: a `character(4)` holding `x` comes back padded, and `length` still ignores
    // the padding the way a real server does.
    assert_eq!(
        node.rows("SELECT greatest(c, 'x'), length(greatest(c, 'x')) FROM g1tm"),
        vec![vec!["x   ".to_owned(), "1".to_owned()]]
    );
}
