//! Contract C3 for `format_type(oid, typmod)` — phase 13 unit 1a.
//!
//! The function `ActiveRecord`'s `columns()` stops on, and therefore the function every later unit
//! of this phase is measured behind: `schema.rb`'s statement 193 asks for
//! `format_type(a.atttypid, a.atttypmod)` over `pg_attribute`, and until it answers, nothing in
//! the schema-dump path can be measured at all.
//!
//! The corpus is 29 statements put to a real PostgreSQL 19beta1 in one session and replayed the
//! same way against one node, with no fixture — the function creates nothing and reads nothing.
//! What it pins is the five rules `tests/corpus/pg19_catalog_format_type.txt` names, of which two
//! are the ones an implementation written from memory gets wrong: the `varchar` typmod's `+4`
//! threshold, where `4` prints bare and `5` is `character varying(1)`, and the difference between
//! a **NULL** typmod and a typmod of **`-1`**, which changes the answer for `character` and for
//! nothing else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: `format_type` is a function of its arguments.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why. One decision, three statements of it.
///
/// **`format_type` knows this server's types**, which is the rule `pg_type` already follows
/// (`crate::catalog::pg_catalog`, and the argument is at the top of
/// `tests/corpus/pg19_pg_catalog.txt`). The shape of the answer is identical either way: `???` is
/// exactly what a *real* server prints for an OID it has no `pg_type` row for, so a client sees the
/// same three characters it would see for OID 999999 there — the set of OIDs differs, the function
/// does not. Listing PostgreSQL's standard OIDs here instead would tell a client this node can
/// store a `numeric`, which it answers `0A000` for, and that is a wrong answer rather than a short
/// one. It closes one type at a time as types arrive.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT format_type(26, NULL), format_type(19, NULL), format_type(2206, NULL)",
            "the same, for the three types this node deliberately does not have: `oid`, `name` and \
             `regtype` are the ones `pg_catalog`'s own columns are declared as on a real server, \
             and this node reports those columns as `bigint` and `text` (declared in \
             `tests/pg_catalog.rs`). A `format_type` that named them would name types no \
             `RowDescription` from here ever carries.",
        ),
        (
            "SELECT format_type(1007, NULL), format_type(1009, NULL)",
            "an array type carries its element's typmod and prints the brackets outside — \
             `format_type(1015, 1028)` is `character varying(1024)[]` on a real server. This node \
             has no array types at all, so both are `???`. It is the array lane's to close, and \
             it is named here so a reader finds the gap rather than a wrong answer.",
        ),
    ],
};

#[test]
fn every_format_type_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_format_type.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 26,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The statement `ActiveRecord` actually sends, over the types this node has.
///
/// Not the same assertion as the corpus: that one asks `format_type` about OIDs written as
/// literals, and this asks it about a **column's** OID and typmod, which is the only shape
/// `columns()` ever writes. The two differ in the thing most likely to break — the typmod arriving
/// as a value rather than as a constant the lowering could have folded.
#[test]
fn a_column_s_own_type_and_modifier_are_named() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ft (id int8 PRIMARY KEY, e varchar(5), f varchar, g character(3), \
         m timestamp(3), l timestamptz, n float8)",
    ]);

    // `atttypid` and `atttypmod` are what a real server carries and what ADR 0033 stores: the
    // typmod is PostgreSQL's own number, `9` for a `varchar(5)` and `3` for a `timestamp(3)`.
    assert_eq!(
        node.rows(
            "SELECT format_type(20, -1), format_type(1043, 9), format_type(1043, -1), \
             format_type(1042, 7), format_type(1114, 3), format_type(1184, -1), \
             format_type(701, -1)"
        ),
        vec![vec![
            "bigint",
            "character varying(5)",
            "character varying",
            "character(3)",
            "timestamp(3) without time zone",
            "timestamp with time zone",
            "double precision",
        ]]
    );
}

/// Every type this node has is reachable through its own OID.
///
/// The assertion that cannot be forgotten when a type is added: `format_type` derives its map from
/// `ColumnType::ALL`, so a new type answers here on the commit that adds it, and a `???` in this
/// list means a type was added to the node and left out of the function that prints it.
#[test]
fn no_type_of_this_node_s_is_unknown_to_it() {
    let mut node = parity::Node::new(&[]);
    let oids = [
        16, 17, 20, 21, 23, 25, 114, 700, 701, 1042, 1043, 1114, 1184, 3802,
    ];
    for oid in oids {
        let named = node.rows(&format!("SELECT format_type({oid}, NULL)"));
        assert_ne!(named, vec![vec!["???"]], "oid {oid} has no type behind it");
    }
}
