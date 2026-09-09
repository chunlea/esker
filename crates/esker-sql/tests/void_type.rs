//! **`void` is a type, oid 2278**, against PostgreSQL 19beta1.
//!
//! `pg_advisory_lock(1)` returns a `void` on a real server and this node folded it to an empty
//! string, so the `RowDescription` said `text` (25) where PostgreSQL says 2278. The **value** was
//! already right — a void renders as zero characters and is *not* NULL — so this is the declared
//! type and nothing else.
//!
//! **Half the advisory family is a `boolean`**, measured from `pg_proc.prorettype` as well as from
//! `\gdesc`: `pg_advisory_lock` and `pg_advisory_unlock_all` are 2278, `pg_advisory_unlock` and
//! `pg_try_advisory_lock` are 16. A rule that made "the advisory functions" void would be wrong
//! for two of the four, and `ActiveRecord` reads the boolean ones —
//! `connection_test.rb#test_get_and_release_advisory_lock` asserts on what `pg_try_advisory_lock`
//! returns.
//!
//! It is a **pseudo-type**: `typtype` is `p`, `typcategory` `P`, `typarray` 0. No column can be
//! declared as one and there is no array of it, which is what keeps it out of the storage
//! vocabulary that every other `ColumnType` belongs to.
//!
//! Measured in `tests/captures/pg19_void.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty.** The four catalog type families closed it — `name` (ADR 0084), `"char"`
    // (ADR 0095), `oid` (ADR 0097) and `regproc` (ADR 0098) — and `pg_typeof` answers a
    // `regtype` on both sides (ADR 0093). The *values* never changed a character.
    types: &[],
    answers: &[
        // **`pg_typeof` reads the datum, and a void's datum is a `Datum::Text("")`.** These two are
        // the rows whose type is carried by the *expression*; the `RowDescription` for both is
        // 2278, which is what a client reads and what `the_advisory_family_splits_two_and_two`
        // asserts. The seam ADR 0086, ADR 0089 and `tests/cidr_aggregate.rs` all name.
        // `pg_notify` is not built, which is a named gap of its own and nothing to do with `void`.
        (
            "SELECT 'r', pg_typeof(pg_notify('c','p'))",
            "`pg_notify` is `0A000` by name — `LISTEN`/`NOTIFY` is not built — and it is in this \
             corpus only because it is a third function that returns a `void` on a real server.",
            "pg19_void.txt:56",
        ),
        // **`pg_proc` has no `prorettype` column here**, which is the catalog's own gap and not
        // this type's: the view carries what `ActiveRecord` reads, and a column that is not there
        // is `42703` — the same answer a real server gives. The row is in the corpus because it is
        // the *evidence* for the split above, measured from `pg_proc` rather than from `\gdesc`
        // alone: 2278 for two of the four and 16 for the other two.
        (
            "SELECT proname, prorettype FROM pg_proc WHERE proname IN \
             ('pg_advisory_lock','pg_advisory_unlock','pg_advisory_unlock_all','pg_try_advisory_lock') \
             ORDER BY proname, prorettype",
            "`pg_proc.prorettype` is not a column this node's view carries, so it is `42703`. The \
             row is here as the evidence for which advisory functions return a `void`.",
            "pg19_void.txt:57",
        ),
    ],
};

#[test]
fn every_void_answer_is_postgresql_19_s() {
    let checked = parity::replay(include_str!("corpus/pg19_void.txt"), &[], &DIVERGENCES);
    assert!(
        checked > 14,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The declared type a client is told, **through a `Describe`**.
fn described(node: &mut parity::Node, statement: &str) -> Vec<u32> {
    node.describe(statement)
        .unwrap()
        .fields
        .expect("a SELECT returns rows")
        .into_iter()
        .map(|field| field.type_oid)
        .collect()
}

/// **2278 for the two that return one, 16 for the two that do not.**
#[test]
fn the_advisory_family_splits_two_and_two() {
    let mut node = parity::Node::new(&[]);
    for (statement, oid) in [
        ("SELECT pg_advisory_lock(1) AS v", 2278),
        ("SELECT pg_advisory_unlock_all() AS v", 2278),
        ("SELECT pg_advisory_unlock(1) AS v", 16),
        ("SELECT pg_try_advisory_lock(2) AS v", 16),
    ] {
        assert_eq!(described(&mut node, statement)[0], oid, "{statement}");
    }
}

/// **A void is a value, and the value is zero characters** — it is not NULL and it is not no row.
#[test]
fn a_void_is_an_empty_value_and_not_a_null() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 'r', pg_advisory_unlock_all() IS NULL"),
        vec![vec!["r", "f"]]
    );
    assert_eq!(
        node.rows("SELECT 'r', pg_advisory_unlock_all()::text"),
        vec![vec!["r", ""]]
    );
    assert_eq!(
        node.rows("SELECT 'r', length(pg_advisory_unlock_all()::text)"),
        vec![vec!["r", "0"]]
    );
    // One row, which is what separates a void from a function that returns nothing.
    assert_eq!(
        node.rows("SELECT count(*) FROM (SELECT pg_advisory_unlock_all()) s"),
        vec![vec!["1"]]
    );
}

/// The catalog row, and it is a **pseudo-type**.
#[test]
fn pg_type_has_the_void_row() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT oid, typname, typlen, typtype, typcategory, typdelim, typinput, typarray \
             FROM pg_type WHERE typname = 'void'"
        ),
        vec![vec!["2278", "void", "4", "p", "P", ",", "void_in", "0"]]
    );
    assert_eq!(
        node.rows("SELECT format_type(2278, -1)"),
        vec![vec!["void"]]
    );
}

/// **A `void` is not a column type**, which is what `typtype = 'p'` means — and PostgreSQL names
/// the column, so the check is per column and not per statement.
#[test]
fn no_column_can_be_declared_void() {
    let mut node = parity::Node::new(&["CREATE TABLE keep (c int8)"]);
    for (statement, column) in [
        ("CREATE TABLE v (c void)", "c"),
        // The second column, so the message proves the check is not about the first one.
        ("CREATE TABLE v2 (c int8, d void)", "d"),
        ("ALTER TABLE keep ADD COLUMN d void", "d"),
        ("ALTER TABLE keep ALTER COLUMN c TYPE void", "c"),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            esker_sql::sqlstate::INVALID_TABLE_DEFINITION,
            "{statement}"
        );
        assert_eq!(
            error.to_string(),
            format!("column \"{column}\" has pseudo-type void"),
            "{statement}"
        );
    }
}

/// Two voids in one target list, each described as one.
#[test]
fn two_voids_in_a_row_are_both_described() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        described(
            &mut node,
            "SELECT 'r', pg_advisory_lock(1), pg_advisory_unlock_all()"
        ),
        vec![25, 2278, 2278]
    );
    assert_eq!(
        node.rows("SELECT 'r', pg_advisory_lock(1), pg_advisory_unlock_all()"),
        vec![vec!["r", "", ""]]
    );
}
