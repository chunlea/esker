//! **`pg_attribute.atthasmissing` — the only way a client can ask whether `ADD COLUMN` rewrote.**
//!
//! `ALTER TABLE … ADD COLUMN … DEFAULT 7` is instant here and on a real server: the value is
//! stored on the *column* as its missing value and the decoder pads with it, rather than every row
//! being rewritten to carry it. `atthasmissing` is the flag that says so, and this node did not
//! have the column at all — which I found writing a test for the volatile-default backfill and had
//! to anchor on something else.
//!
//! Measured on 19beta1, five shapes in one transaction:
//!
//! ```text
//! CREATE TABLE (a int DEFAULT 7)                f
//! ADD COLUMN b int DEFAULT 7, table EMPTY       t   {7}
//! ADD COLUMN b int DEFAULT 7, table POPULATED   t   {7}
//! ADD COLUMN b int, no default                  f
//! then ALTER COLUMN b SET DEFAULT 9             t   {7}   -- the missing value is frozen
//! ```
//!
//! **The empty-table row is the one reasoning gets wrong.** PostgreSQL sets the missing value
//! whether or not there is a row to pad, and so does this node — `ColumnDef::missing` is written
//! by `ADD COLUMN` unconditionally and is `None` everywhere else, which makes the flag exactly
//! `missing.is_some()`.
//!
//! `attmissingval` is **not** added. It is an `anyarray` on a real server — `{7}` — and this node
//! has no such type; a column of some other type would be a value nobody measured. Absent, and
//! `42703` like `pg_range`'s `oid`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn has_missing(node: &mut parity::Node, table: &str) -> Vec<Vec<String>> {
    node.rows(&format!(
        "SELECT attname, atthasdef, atthasmissing FROM pg_attribute \
         WHERE attrelid = '{table}'::regclass AND attnum > 0 AND NOT attisdropped ORDER BY attnum"
    ))
}

/// A default written at `CREATE TABLE` has **no** missing value: it applies to rows written after
/// it, and there are no earlier rows for it to stand in for.
#[test]
fn a_create_table_default_is_not_a_missing_value() {
    let mut node = parity::Node::new(&["CREATE TABLE g1_hm1 (a int DEFAULT 7)"]);
    assert_eq!(
        has_missing(&mut node, "g1_hm1"),
        [["a".to_owned(), "t".to_owned(), "f".to_owned()]],
        "atthasdef is true and atthasmissing is not"
    );
}

/// **`ADD COLUMN` sets it whether or not the table has a row.** The half that is easy to assume
/// the other way round.
#[test]
fn add_column_sets_it_on_an_empty_table_too() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1_hm2 (id int)",
        "ALTER TABLE g1_hm2 ADD b int DEFAULT 7",
    ]);
    assert_eq!(
        has_missing(&mut node, "g1_hm2"),
        [
            ["id".to_owned(), "f".to_owned(), "f".to_owned()],
            ["b".to_owned(), "t".to_owned(), "t".to_owned()],
        ]
    );
}

/// The populated case, which is the one the flag exists for.
#[test]
fn add_column_on_a_populated_table_sets_it() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1_hm3 (id int)",
        "INSERT INTO g1_hm3 VALUES (1)",
        "ALTER TABLE g1_hm3 ADD b int DEFAULT 7",
    ]);
    assert_eq!(
        has_missing(&mut node, "g1_hm3"),
        [
            ["id".to_owned(), "f".to_owned(), "f".to_owned()],
            ["b".to_owned(), "t".to_owned(), "t".to_owned()],
        ]
    );
    // And a later `SET DEFAULT` moves the default without moving the missing value — the rows that
    // predate the column still read 7, which is what `atthasmissing` staying true is about.
    node.run("ALTER TABLE g1_hm3 ALTER COLUMN b SET DEFAULT 9")
        .unwrap();
    assert_eq!(
        has_missing(&mut node, "g1_hm3"),
        [
            ["id".to_owned(), "f".to_owned(), "f".to_owned()],
            ["b".to_owned(), "t".to_owned(), "t".to_owned()],
        ]
    );
    node.run("INSERT INTO g1_hm3 VALUES (2, DEFAULT)").unwrap();
    assert_eq!(
        node.rows("SELECT id, b FROM g1_hm3 ORDER BY id"),
        [
            ["1".to_owned(), "7".to_owned()],
            ["2".to_owned(), "9".to_owned()],
        ]
    );
}

/// No default, nothing to pad with.
#[test]
fn add_column_with_no_default_has_no_missing_value() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1_hm4 (id int)",
        "INSERT INTO g1_hm4 VALUES (1)",
        "ALTER TABLE g1_hm4 ADD b int",
    ]);
    assert_eq!(
        has_missing(&mut node, "g1_hm4"),
        [
            ["id".to_owned(), "f".to_owned(), "f".to_owned()],
            ["b".to_owned(), "f".to_owned(), "f".to_owned()],
        ]
    );
}

/// **A volatile default is `f`, because it rewrote.** Measured on 19beta1 for both
/// `gen_random_uuid()` and `(1 + 1)` — `atthasmissing` comes back false, which is how a real
/// server says the table was rewritten rather than padded, and is the assertion I wanted when
/// `ADD COLUMN … DEFAULT gen_random_uuid()` landed.
#[test]
fn a_volatile_default_rewrote_and_says_so() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1_hm5 (id int)",
        "INSERT INTO g1_hm5 VALUES (1)",
        "ALTER TABLE g1_hm5 ADD b uuid DEFAULT gen_random_uuid()",
        "ALTER TABLE g1_hm5 ADD c int DEFAULT (1+1)",
    ]);
    assert_eq!(
        has_missing(&mut node, "g1_hm5"),
        [
            ["id".to_owned(), "f".to_owned(), "f".to_owned()],
            ["b".to_owned(), "t".to_owned(), "f".to_owned()],
            ["c".to_owned(), "t".to_owned(), "f".to_owned()],
        ],
        "a default it has, a missing value it does not"
    );
}
