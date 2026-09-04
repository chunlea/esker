//! A type's **internal** name is a type name: `bpchar`, `int4`, `varchar`.
//!
//! `schema_test.rb`'s shared setup is three `CREATE DOMAIN`s, and the third one gated the whole
//! file of 78 tests:
//!
//! ```sql
//! CREATE DOMAIN schema_1.text   AS text;      -- fine
//! CREATE DOMAIN schema_1.varchar AS varchar;  -- fine
//! CREATE DOMAIN schema_1.bpchar AS bpchar;    -- 0A000 the type bpchar is not supported
//! ```
//!
//! **`bpchar` is what PostgreSQL calls `character(n)` in `pg_type`** — blank-padded character —
//! so it is not a second-class spelling, it is the one the catalog itself reports. `character(4)`
//! worked and the name the catalog prints for that same type did not.
//!
//! Anything the grammar has no variant for arrives as a *custom* type name, which is where a
//! user-defined type is looked up — so the name was a user type nobody had declared. Resolving
//! built-in names there first is one arm, and it fixes the three paths at once: a cast, a column
//! and a domain.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

#[test]
fn an_internal_type_name_is_a_type_name() {
    let mut node = parity::Node::new(&[]);

    // A cast.
    assert_eq!(node.rows("SELECT 'a'::bpchar"), vec![vec!["a"]]);
    // A column, and `format_type` prints the name back.
    node.run("CREATE TABLE itn (c bpchar, n int4, v varchar)")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT format_type(a.atttypid, a.atttypmod) FROM pg_attribute a \
             WHERE a.attrelid = 'itn'::regclass AND a.attname = 'c'"
        ),
        vec![vec!["bpchar"]]
    );
    // A domain over one, which is the statement the suite sends.
    node.run("CREATE DOMAIN itn_d AS bpchar").unwrap();
    assert_eq!(
        node.rows("SELECT typtype FROM pg_type WHERE typname = 'itn_d'"),
        vec![vec!["d"]]
    );
}

/// **A name nobody declared is still `0A000`**, which is the half this must not break: the arm
/// resolves built-in names and does not make every unknown word a type.
#[test]
fn a_name_that_is_no_type_is_still_refused() {
    let mut node = parity::Node::new(&[]);
    let error = node.run("CREATE TABLE itn_bad (c nosuchtype)").unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert_eq!(error.to_string(), "the type nosuchtype is not supported");
}
