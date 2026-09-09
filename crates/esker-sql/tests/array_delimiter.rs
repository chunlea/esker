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
//! **The `::regproc` casts below are not decoration.** These queries used to read
//! `WHERE typinput = 'array_in'`, and that is a statement a **real server refuses**:
//! `22P02 invalid input syntax for type oid: "array_in"`, because `=` over a `regproc` is `oideq`
//! and the unadorned literal goes to `oidin`. They passed here only because `typinput` was a
//! `text` column — a test passing on the mechanism it was not testing — and reddened the moment
//! the column was declared its real type (ADR 0098). `typinput = 'array_in'::regproc` is the form
//! a real server answers, and `typinput::text = 'array_in'` is the other one; both were measured.
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
         ON e.oid = t.typelem WHERE t.typinput = 'array_in'::regproc AND t.typdelim <> e.typdelim",
    );
    assert!(
        mismatched.is_empty(),
        "these arrays do not carry their element's delimiter: {mismatched:?}"
    );
    // And the one that is not a comma is still there, so the rule above cannot be satisfied by a
    // table where every delimiter is a comma.
    assert_eq!(
        node.rows(
            "SELECT t.typname FROM pg_type t WHERE t.typinput = 'array_in'::regproc AND t.typdelim <> ','"
        ),
        vec![vec!["_box"]]
    );
    // The other half of the same measurement: on a real server `box` is the **only** base type
    // whose delimiter is not a comma, asked with `typinput <> 'array_in'::regproc` so that `box` — whose
    // `typelem` is `point` there — is counted as the base type it is.
    assert_eq!(
        node.rows(
            "SELECT typname FROM pg_type WHERE typtype = 'b' AND typinput <> 'array_in'::regproc \
             AND typdelim <> ',' ORDER BY typname"
        ),
        vec![vec!["box"]]
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

/// **Every `typarray` names a row that is there**, which is not a tautology: the pointer and the
/// row are written by two different rules, and a `typarray` naming nothing is what
/// `TypeError: can't quote Array` was before sixteen array types arrived at once.
#[test]
fn no_typarray_dangles() {
    let mut node = parity::Node::new(&[]);
    let dangling = node.rows(
        "SELECT t.typname, t.typarray FROM pg_type t WHERE t.typarray <> 0 \
         AND NOT EXISTS (SELECT 1 FROM pg_type a WHERE a.oid = t.typarray)",
    );
    assert!(
        dangling.is_empty(),
        "these types point at an array row that is not there: {dangling:?}"
    );
}

/// A base type without an array is a **decision**, so it is listed with the others.
///
/// On a real server almost every base type has one — 65 of them below oid 10000, of which only six
/// internal ones (`pg_node_tree`, `pg_ndistinct`, `pg_dependencies`, the two BRIN summaries and
/// their sibling) do not, measured. This node has fewer types and a few deliberate gaps; the point
/// of the list is that adding a type makes somebody choose rather than inherit a `0`.
#[test]
fn every_base_type_has_an_array_or_is_listed() {
    let mut node = parity::Node::new(&[]);
    let without: Vec<String> = node
        // **`typinput <> 'array_in'::regproc`, not `typelem = 0`.** A `box`'s `typelem` is `point` on a
        // real server and it is not an array — the input function is what says which a row is.
        // Written the other way this guard passed only because this node reports `box`'s
        // `typelem` as 0, so it was resting on a divergence rather than on the rule.
        .rows(
            "SELECT typname FROM pg_type WHERE typtype = 'b' AND typinput <> 'array_in'::regproc \
             AND typarray = 0 ORDER BY typname",
        )
        .into_iter()
        .map(|row| row[0].clone())
        .collect();
    // Each of these is a named gap with a reason, not an oversight:
    //
    //   int2vector, oidvector                catalog types a client reads and never stores an
    //                                        array of
    //   lquery                               `ltree`'s *pattern* type: it appears in a `WHERE`
    //                                        and is not a column anybody declares, so an array of
    //                                        one has no writer. `ltree` itself has `_ltree`.
    //
    // This list is the test. Writing it out found `lquery`, which had inherited a `0` rather than
    // being decided — nine names were expected and the node answered ten. **`name` left the list**
    // when `_name` (1003) was built: run 106 lost ten tests because `array_agg` over a `name`
    // column had no array type to answer with (`tests/name_array.rs`).
    // **The five geometric shapes left this list with ADR 0091.** They had been one named gap with
    // one reason — no suite test declares an array of one — until r1's wire sweep found
    // `array_agg` over a `circle` coming back a scalar `text`, which made the reason false for
    // three of the five; splitting it would have left a worse gap than it closed.
    // **`regclass` left this list with `_regclass` (2210).** It had been a catalog type "a client
    // reads and never stores an array of", and r1's wire sweep found `array_agg(c::regclass)`
    // coming back a scalar `text` — the same shape that took `name` off this list a run earlier.
    // The reason was false for the same reason it was false there: a client does not have to
    // *store* an array of a type to be handed one.
    let expected = ["int2vector", "lquery", "oidvector"];
    assert_eq!(
        without, expected,
        "a base type gained or lost its array without this list being updated"
    );
}
