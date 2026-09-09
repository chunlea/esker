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
//! # The residue is a spelling, not a value
//!
//! `::` binds tighter than unary minus, so `-1::bigint` is an operator over a cast —
//! `(- (1)::bigint)` — where `(-1)::bigint` is a cast over a constant. Both fold to the same
//! constant here, so the two are indistinguishable after lowering and this node prints the second
//! form for both. That needs the unary minus kept as a node, which is a change to what the plan
//! holds rather than to what the printer decides; it stays declared below.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the tables it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 'r', a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1nn'::regclass AND a.attname = 'g_dec_i4'",
            "**The fold evaluated the cast, so the constant this node holds is a different number.** \
             `(-1.5)::integer` is `('-1.5'::numeric)::integer` on a real server -- a cast over the \
             decimal it was written with -- and `'-2'::integer` here, because folding a cast to \
             `integer` rounds. The *value* is right and reads back as itself; what is gone is the \
             number the expression was written with, and rounding is not invertible, so no rule \
             over the folded constant can recover it. This is the shape #24's row was sized for and \
             the one place it really does need the cast node kept ([ADR 0086](../../../docs/adr/0086-a-folded-cast-keeps-the-type-it-named.md)); \
             `q_big_f8` below is the same cause with a different loss.",
            "pg19_negative_constant.txt:124",
        ),
        (
            "SELECT 'r', a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1nn'::regclass AND a.attname = 'q_big_f8'",
            "**The same lossy fold, one type over.** \
             `(9223372036854775807)::double precision` is \
             `('9223372036854775807'::bigint)::double precision` there and \
             `(9.223372036854776e+18)::double precision` here: folding the cast converted the \
             integer to a float, which cannot hold those digits, so the constant printed is the \
             float's. The *form* is right -- a cast over a constant of the value's natural type, \
             which is what this unit implemented -- and the digits inside it are the fold's. Same \
             cause as `g_dec_i4`, and the same fix: keep the cast node.",
            "pg19_negative_constant.txt:180",
        ),
        (
            "SELECT 'r', a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1nd'::regclass ORDER BY a.attnum",
            "**One of three, and it is a *spelling* rather than a form.** `DEFAULT -1::bigint` is \
             `(- (1)::bigint)` there -- `::` binds tighter than unary minus, so it is an operator \
             over a cast -- and `(-1::BIGINT)` here, the expression as written. The other two \
             columns of this row agree, which is the half this unit closed: \
             `DEFAULT (-1)::bigint` and `DEFAULT (-1.5)::double precision` are now \
             `('-1'::integer)::bigint` and `('-1.5'::numeric)::double precision` in both. \
             The residue is not a printer gap: both spellings fold to the same constant, so after \
             lowering there is nothing to tell them apart and the printer would have to invent a \
             choice. Keeping the unary minus as a node is a change to what the plan holds. The \
             `u_*` rows above are the same shape in a generated column, where this node prints the \
             `(-1)::bigint` form for both.",
            "pg19_negative_constant.txt:188",
        ),
    ],
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
