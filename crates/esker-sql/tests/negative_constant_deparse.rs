//! **How a *negative* numeric constant is printed**, and the five readers that print one.
//!
//! `docs/plans/debts-v1.1.md` #24. `pg19_numeric_literal_deparse.txt` measured the three forms a
//! constant prints in and found one shape it could not decide: `(-1)::bigint` is
//! `('-1'::integer)::bigint` on a real server — **two** nodes — and this node folds the cast into
//! the constant, leaving one. The row was sized as "keeping the cast node changes what every
//! reader of a stored expression prints, so it needs its own corpus".
//!
//! **The corpus says the tree is recoverable and the cast node is not needed.** A negative
//! constant has no literal form — the scanner reads the digits and the unary minus is folded in —
//! so what PostgreSQL holds is a quoted `Const` of the type the *digits* would have had, and that
//! type is a function of the value:
//!
//! ```text
//! (-1)::integer                   '-1'::integer                        natural, nothing wraps
//! (-1)::bigint                    ('-1'::integer)::bigint
//! (-1.5)::double precision        ('-1.5'::numeric)::double precision   natural is numeric
//! (-9223372036854775807)::numeric ('-9223372036854775807'::bigint)::numeric
//! ```
//!
//! Which is the same rule `exec::ddl::numeric_constant` already applied to the constant on its
//! own, one level further out: print the constant under its **natural** type, and wrap that in a
//! cast when the wanted type is not the natural one.
//!
//! # The residue was two losses and a list, and #30 closed all three
//!
//! **A spelling.** `::` binds tighter than unary minus, so `-1::bigint` is an operator over a cast
//! — `(- (1)::bigint)` — where `(-1)::bigint` is a cast over a constant. The plan already held the
//! difference: `lower_expr` builds a `Negate` over the cast for the first and folds the sign into
//! the constant for the second. What printed them alike was `exec::ddl`'s
//! `reprinted_by_pg_get_expr`, where `Negate` sat on the "nothing to reprint" list with the reason
//! that a folded negative literal is stored as a value — true of `DEFAULT - 1`, which keeps no
//! node at all, and false of this one. One line, and it is the same correction `Literal` had
//! received one revision earlier for the same reason.
//!
//! **Two losses.** `(-1.5)::integer` is `-2` once folded and rounding is not invertible;
//! `(9223372036854775807)::double precision` loses the digits to 53 bits. A real server never
//! folds a cast over a constant at all — every form in the capture above holds two nodes — and
//! this node reconstructs the printed text from the folded constant wherever the value survives.
//! Where it does not, the `Cast` node is kept over the literal **as written**, and the conversion
//! happens per row, which is where a real server does it. That is the third clause of the sentence
//! ADR 0086 opened: a datum carries its type, never its modifier (#28), and never the value it was
//! converted from.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the tables it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
/// What this node answers differently, and why: **nothing**.
///
/// The three that stood here were `debts-v1.1.md` #30, and all three were the same sentence one
/// clause further than ADR 0086 had taken it. Deleted with the unit that closed them.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_negative_constant_is_printed_the_way_postgresql_19_prints_it() {
    let checked = parity::replay(
        include_str!("corpus/pg19_negative_constant.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus is not being read"
    );
}
