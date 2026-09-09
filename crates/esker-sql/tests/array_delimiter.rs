//! What `pg_type` says an array's delimiter is, against PostgreSQL 19beta1.
//!
//! `type_lookup_test.rb` reads two rows out of the adapter's type map and asks each for its
//! delimiter:
//!
//! ```ruby
//! box_array = @connection.send(:type_map).lookup(1020)
//! int_array = @connection.send(:type_map).lookup(1007)
//! assert_equal ";", box_array.delimiter
//! assert_equal ",", int_array.delimiter
//! ```
//!
//! Two things have to be true for that to work, and they are two different failures. The map is
//! built from `pg_type`, and a row becomes an **array** entry — the only kind with a `delimiter`
//! method at all — when its `typinput` is `array_in`; a row that is not there at all makes
//! `lookup` answer a plain `Type::Value`, which is the `undefined method 'delimiter'` the suite
//! reports. Then the delimiter itself is `typdelim`, and **an array's is its element's**.
//!
//! `_box` is the only array type in all of `pg_type` whose delimiter is not a comma — measured,
//! by asking a real server for every array type whose `typdelim <> ','` and getting exactly one
//! row. So this pair of assertions is the whole rule: one array that is special and one that is
//! not.
//!
//! Measured in `tests/captures/pg19_array_delimiter.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **The two OIDs the suite looks up**, described the way its type map reads them.
#[test]
fn the_two_oids_the_suite_looks_up_are_arrays_with_the_right_delimiter() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT oid, typname, typelem, typdelim, typinput FROM pg_type \
             WHERE oid IN (1007, 1020) ORDER BY oid"
        ),
        vec![
            vec!["1007", "_int4", "23", ",", "array_in"],
            // `;`, and the element is `box` (603).
            vec!["1020", "_box", "603", ";", "array_in"],
        ]
    );
}

/// **An array's delimiter is its element's**, which is a rule and not a list of one.
///
/// Asserted over every array type rather than over `_box` alone: a table that happened to hold
/// `;` for `_box` and nothing else would pass the suite's two assertions and still be wrong for
/// the next element type that has its own delimiter.
#[test]
fn every_arrays_delimiter_is_its_elements() {
    let mut node = parity::Node::new(&[]);
    let mismatched = node.rows(
        "SELECT t.typname, t.typdelim, e.typdelim FROM pg_type t JOIN pg_type e \
         ON e.oid = t.typelem WHERE t.typinput = 'array_in' AND t.typdelim <> e.typdelim",
    );
    assert!(
        mismatched.is_empty(),
        "these arrays do not carry their element's delimiter: {mismatched:?}"
    );
    // And the one that is not a comma is still there, so the rule above cannot be satisfied by a
    // table where every delimiter is a comma.
    assert_eq!(
        node.rows(
            "SELECT t.typname FROM pg_type t WHERE t.typinput = 'array_in' AND t.typdelim <> ','"
        ),
        vec![vec!["_box"]]
    );
}

/// A `box[]` is a type this node stores, not only a row in the catalog.
#[test]
fn a_box_array_is_a_column_type() {
    let mut node = parity::Node::new(&["CREATE TABLE g (id int8, shape box[])"]);
    node.run("INSERT INTO g VALUES (1, '{(1,1),(0,0);(3,3),(2,2)}')")
        .unwrap();
    // **Semicolons between the elements**, which is what the delimiter is for: a `box[]` written
    // with commas is a different array.
    assert_eq!(
        node.rows("SELECT shape FROM g"),
        vec![vec!["{(1,1),(0,0);(3,3),(2,2)}"]]
    );
    let outcome = node.run("SELECT shape FROM g").unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("no rows");
    };
    assert_eq!(fields[0].type_oid, 1020);
}
