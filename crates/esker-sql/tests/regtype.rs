//! `'x'::regtype::oid` against PostgreSQL 19beta1's own answers — the cast the scoreboard named.
//!
//! Three scoreboard runs stopped at rung 2 on one statement. ADR 0033 scoped this cast with the
//! type surface "because neither moves the ladder alone", tier 1 shipped without it, and run 3
//! measured exactly the outcome the ADR predicted: six types landed and the ladder moved by zero
//! rungs. `docs/bench/rails-scoreboard.md` run 3 has the numbers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a constant expression.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // A real server's `regtype` is a type of its own — four bytes holding an OID that print as
        // the type's name. This node has no `regtype`, so `'x'::regtype` answers the **name**, as
        // text: the value is byte-identical and only `RowDescription`'s OID differs, `text` where
        // a real server says `regtype`. The same trade `pg_catalog.rs` makes for `pg_type.oid`.
        "SELECT 'integer'::regtype",
        "SELECT 'int4'::regtype",
        "SELECT 'varchar'::regtype",
        // `::oid` answers a `bigint` here, for the same reason: this node has no four-byte `oid`
        // type either, and the *value* is what a client reads.
        "SELECT 'integer'::regtype::oid",
        "SELECT 'int4'::regtype::oid",
        "SELECT 'bigint'::regtype::oid",
        "SELECT 'int8'::regtype::oid",
        "SELECT 'smallint'::regtype::oid",
        "SELECT 'character varying'::regtype::oid",
        "SELECT 'character varying(255)'::regtype::oid",
        "SELECT 'varchar'::regtype::oid",
        "SELECT 'text'::regtype::oid",
        "SELECT 'character(3)'::regtype::oid",
        "SELECT 'bpchar'::regtype::oid",
        "SELECT 'timestamp(6) without time zone'::regtype::oid",
        "SELECT 'timestamp'::regtype::oid",
        "SELECT 'timestamp with time zone'::regtype::oid",
        "SELECT 'boolean'::regtype::oid",
        "SELECT 'bytea'::regtype::oid",
        "SELECT 'real'::regtype::oid",
        "SELECT 'double precision'::regtype::oid",
        "SELECT 'INTEGER'::regtype::oid",
        "SELECT '23'::oid",
    ],
    answers: &[
        (
            "SELECT pg_typeof('int4'::regtype)",
            "`pg_typeof` is not implemented at all, so this is `0A000` naming the function rather \
             than a wrong type — the honest answer under contract C2. It is in the corpus because \
             it is the statement that would *prove* the divergence above: a real server says the \
             expression's type is `regtype`, and this node would say `text`.",
        ),
        (
            "SELECT 'numeric'::regtype::oid",
            "PostgreSQL answers `1700`; this node answers `42704 type \"numeric\" does not \
             exist`, because it has no `numeric`. Answering `1700` would hand a client the OID of \
             a type this node can neither store nor send — the same argument `pg_catalog.rs` \
             makes for keeping `pg_type` short, and the honest half of it: a name it does not \
             have is a name that does not exist here. It closes when `numeric` lands in tier 2.",
        ),
        (
            "SELECT 'int4[]'::regtype::oid",
            "`1007` on a real server, and `42704` here for the same reason as `numeric`: there \
             are no array types on this node. Arrays are tier 2's last item in ADR 0033's \
             roadmap, and this line closes with them.",
        ),
    ],
};

#[test]
fn every_regtype_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_regtype.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 24,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The statement the ladder stops on, answered.
///
/// Named on its own rather than left inside the corpus replay, because this one line is what three
/// scoreboard runs were waiting for and a future reader should be able to find it by name.
#[test]
fn the_statement_that_stopped_rung_2_answers() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 'integer'::regtype::oid"),
        vec![vec!["23"]]
    );
}
