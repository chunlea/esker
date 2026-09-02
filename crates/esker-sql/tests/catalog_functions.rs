//! `col_description`, `obj_description` and `pg_get_partkeydef`, against PostgreSQL 19beta1.
//!
//! Not types. `col_description` is boot statement 15 and rung 3's blocker at `443074a`;
//! `obj_description` is boot statements 29 and 32; `pg_get_partkeydef` is 31. All three come out
//! of `ActiveRecord`'s `columns()` and its schema-dump path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
///
/// **Nothing.** Every one of the 23 statements is byte-identical to PostgreSQL 19, which is the
/// unusual case and worth saying out loud: it happens because a server with no comments and no
/// partitions has nothing for these three functions to find, and NULL is exactly what a real
/// server answers when *it* has nothing either. This is not a stub that happens to agree — it is
/// the same answer for the same reason.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_catalog_function_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_functions.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 18,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// NULL for every shape, and **not** an error for any of them.
///
/// The rule an implementation gets wrong by treating "no such object" as a failure: an oid that
/// names nothing, an attnum past the last column, a negative one, a NULL argument, an index rather
/// than a table, and an unknown catalog *name* are all NULL on a real server. There is no
/// not-found error anywhere in this surface.
#[test]
fn nothing_found_is_null_and_never_an_error() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE cf (id int8 PRIMARY KEY, a int4)")
        .unwrap();
    for sql in [
        "SELECT obj_description('cf'::regclass)",
        "SELECT obj_description('cf'::regclass, 'pg_class')",
        "SELECT obj_description('cf'::regclass, 'nosuchcatalog')",
        "SELECT col_description('cf'::regclass, 0)",
        "SELECT col_description('cf'::regclass, 1)",
        "SELECT col_description('cf'::regclass, 99)",
        "SELECT col_description('cf'::regclass, -1)",
        "SELECT col_description(999999, 1)",
        "SELECT pg_get_partkeydef('cf'::regclass)",
        "SELECT pg_get_partkeydef('cf_pkey'::regclass)",
        "SELECT pg_get_partkeydef(999999)",
        "SELECT obj_description(NULL)",
        "SELECT col_description('cf'::regclass, NULL)",
    ] {
        assert_eq!(node.rows(sql), [["\\N"]], "for {sql}");
    }
}

/// The wrong **number** of arguments is `42883`, and the sentence says number, not types.
///
/// Two different conditions share this SQLSTATE on a real server and PostgreSQL words them
/// differently; an implementation with one of the two looks right on half the cases. And the
/// argument a cast produces is named by **what it casts to** — `col_description(regclass)`, not
/// `col_description(unknown)`, which is what naming the literal underneath the cast would give.
#[test]
fn the_wrong_arity_is_42883_and_names_the_cast_s_type() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE cf (id int8 PRIMARY KEY, a int4)")
        .unwrap();
    for (sql, message) in [
        (
            "SELECT col_description('cf'::regclass)",
            "function col_description(regclass) does not exist",
        ),
        (
            "SELECT col_description('cf'::regclass, 2, 3)",
            "function col_description(regclass, integer, integer) does not exist",
        ),
        (
            "SELECT obj_description()",
            "function obj_description() does not exist",
        ),
        (
            "SELECT pg_get_partkeydef()",
            "function pg_get_partkeydef() does not exist",
        ),
        (
            "SELECT pg_get_partkeydef('cf'::regclass, 1)",
            "function pg_get_partkeydef(regclass, integer) does not exist",
        ),
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "42883", "for {sql}");
        assert_eq!(error.to_string(), message, "for {sql}");
        assert_eq!(
            error.detail().as_deref(),
            Some("No function of that name accepts the given number of arguments."),
            "for {sql}"
        );
    }
}

/// The three are readable over a **column of oids**, which is how `ActiveRecord` calls them.
///
/// Boot statement 15 asks `col_description(a.attrelid, a.attnum)` for every row of a join, not for
/// one hand-written oid — so the function has to answer per row rather than be folded once.
#[test]
fn they_answer_over_a_column_of_oids() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE cf (id int8 PRIMARY KEY, a int4, b text)")
        .unwrap();
    let rows = node.rows(
        "SELECT a.attname, col_description(a.attrelid, a.attnum) FROM pg_attribute a WHERE \
         a.attrelid = 'cf'::regclass ORDER BY a.attnum",
    );
    assert_eq!(
        rows,
        vec![vec!["id", "\\N"], vec!["a", "\\N"], vec!["b", "\\N"]]
    );
    assert_eq!(
        node.rows(
            "SELECT c.relname, obj_description(c.oid, 'pg_class'), pg_get_partkeydef(c.oid) FROM \
             pg_class c WHERE c.relname = 'cf'"
        ),
        [["cf", "\\N", "\\N"]]
    );
}
