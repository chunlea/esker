//! **`NULLIF(a, b)`, the third of PostgreSQL's four comparison productions to exist here.**
//!
//! `docs/plans/debts-v1.1.md` #25 is about how a stored call is *printed*, and building its corpus
//! found that this node had no `NULLIF` at all: `nullif(t, 'x')` in a generated column was
//! `0A000 the function nullif is not supported`, because lowering carries an unresolved name out
//! as `CatalogFunc::UserFunc`. `GREATEST` and `LEAST` were already `CatalogFunc` variants and
//! `COALESCE` is an `Expr` of its own, so this was the one of the four that was missing.
//!
//! # The result type is the comparison's left input, not the common type
//!
//! Which is the whole reason it could not just join `GREATEST`'s arm. Measured on 19beta1 —
//! twelve pairs, and the first two are the ones that settle it:
//!
//! ```text
//! nullif(int4, int8)      integer        GREATEST(int4, int8)   bigint
//! nullif(int8, int4)      bigint
//! nullif(int4, numeric)   numeric        nullif(int4, float8)   double precision
//! nullif(float8, int4)    double precision   nullif(numeric, int4)  numeric
//! nullif(date, timestamp) date           nullif(timestamp, date) timestamp without time zone
//! nullif(varchar, 'x')    text           nullif(char(4), 'x')   character
//! nullif(text, 'x')       text           nullif(text, varchar)  text
//! ```
//!
//! PostgreSQL resolves `=` between the two arguments and answers the *left* side of the operator
//! it found. Two rules cover every row: a pair inside one **comparison family** has a cross-type
//! operator to resolve to and keeps the left type (`int48eq`, `date_lt_timestamp` — the three
//! families are the integers, the two floats, and date/timestamp/timestamptz), while a pair across
//! families has none, so both sides coerce and the answer is the common type after all. And a
//! `varchar` has no `=` of its own — `varchar = varchar` is `texteq` — so it is compared as `text`
//! and answers `text`, where `bpchar`, which does have one, answers `character`.
//!
//! # Not strict, and in the other direction from GREATEST
//!
//! `GREATEST(1, NULL, 3)` skips the NULL. Here `nullif(NULL, 1)` is NULL and `nullif(1, NULL)` is
//! `1`: the answer is always the *first* argument or nothing, and a comparison against NULL is
//! unknown rather than equal, so the "they matched" branch is not taken.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE nl (t text, v varchar(10), c char(4), n integer, b bigint, nu numeric, \
     f float8, d date, ts timestamp)",
    "INSERT INTO nl VALUES ('a', 'a', 'a  ', 1, 1, 1, 1, '2020-01-01', '2020-01-01 12:00:00')",
];

/// The twelve pairs, in one statement, asserting the type a client is **told** beside the value.
///
/// Asked through the `RowDescription` rather than `pg_typeof` because that is the answer the type
/// rule decides and the one `ActiveRecord` reads. The oracle's own answer for the same twelve
/// columns is `format_type(atttypid, atttypmod)` over `CREATE TABLE ... AS SELECT`.
///
/// **The eleventh column is a declared divergence and it is about a typmod, not a type.** A real
/// server answers `character(4)` for `nullif(c, 'x')` and this node answers `bpchar` — which is
/// PostgreSQL's own spelling for a `bpchar` whose length is unknown, and it measured that beside
/// it: `greatest(c, 'x')` is `bpchar` there too, and `greatest(v, 'x')` is `character varying`.
/// The difference is that `NULLIF` is the *identity* on its left argument, so PostgreSQL carries
/// that argument's `atttypmod` through where every coercing production drops it. Here an
/// expression has no typmod at all — `exec::query::expr_type` answers a bare `ColumnType` and the
/// `FieldDescription` for a computed column is built with `NO_TYPMOD` — so threading a length
/// through one production would mean giving expressions a typmod, which is a seam and not this
/// unit. Conservative in the direction that matters: a client is told the type and not the
/// length, and `ActiveRecord` reads a column's length from `pg_attribute`, not from a
/// `RowDescription`. `docs/plans/debts-v1.1.md` carries the row.
#[test]
fn the_result_type_is_the_comparisons_left_input() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer(
            "SELECT nullif(n, b), nullif(b, n), nullif(n, nu), nullif(n, f), nullif(f, n), \
             nullif(nu, n), nullif(d, ts), nullif(ts, d), nullif(v, 'x'), nullif(c, 'x'), \
             nullif(t, 'x'), nullif(t, v) FROM nl"
        )
        .to_string(),
        "integer,bigint,numeric,double precision,double precision,numeric,date,\
         timestamp without time zone,text,bpchar,text,text\
         \t\\N|\\N|\\N|\\N|\\N|\\N|2020-01-01|2020-01-01 12:00:00|a|a   |a|\\N"
    );
}

/// **The value, and both NULL directions**, which is the half a type test cannot see.
#[test]
fn the_answer_is_the_first_argument_or_nothing() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer(
            "SELECT nullif(t, 'a'), nullif(t, 'x'), nullif(n, 1), nullif(n, 2), \
             nullif(NULL::int, 1), nullif(n, NULL::int) FROM nl"
        )
        .to_string(),
        "text,text,integer,integer,integer,integer\t\\N|a|\\N|1|\\N|1"
    );
}

/// **A comparison between two immutable operands is immutable**, so it may be an index key and a
/// generated column — which is where the corpus met it.
#[test]
fn nullif_may_be_an_index_key_and_a_generated_column() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "CREATE INDEX i_nl ON nl ((nullif(t, 'x')))",
        "ALTER TABLE nl ADD COLUMN g text GENERATED ALWAYS AS (nullif(t, 'x')) STORED",
        "ALTER TABLE nl ADD COLUMN g2 bigint GENERATED ALWAYS AS (nullif(b, 1)) STORED",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "(a command, no result set)",
            "{sql}"
        );
    }
    // And the stored column computes, which is the assertion an accepted `ALTER` does not make.
    assert_eq!(
        node.answer("SELECT g, g2 FROM nl").to_string(),
        "text,bigint\ta|\\N"
    );
}

/// **Two arguments, and the sqlstate for a third is this node's own.**
///
/// On a real server `NULLIF` is a grammar production with exactly two arguments, so `nullif(1)` is
/// `42601 syntax error at or near ")"`. Here it is a name in the function table and the arity set
/// answers `42883`, the same refusal every other wrong-arity call gets. One sqlstate apart on a
/// statement nothing sends — recorded rather than special-cased in the parser, because making the
/// parser know the name would be a second reader of the function table.
#[test]
fn a_wrong_arity_is_refused_by_this_nodes_own_convention() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in ["SELECT nullif(1) FROM nl", "SELECT nullif(1, 2, 3) FROM nl"] {
        let answer = node.answer(sql).to_string();
        assert!(answer.starts_with("!42883"), "{sql}: {answer}");
    }
}
