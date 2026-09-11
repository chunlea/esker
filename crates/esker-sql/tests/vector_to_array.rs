//! **A vector casts to its array, and the array is subscripted from zero** —
//! [ADR 0107](../../../docs/adr/0107-a-borrowed-representation-needs-somewhere-to-carry-its-identity.md)
//! step 2, the SQL-visible half: **no stored byte moves**.
//!
//! Step 2 was written as *"give the vectors a real array value"*, and its opening instruction was
//! to measure what a stored `int2vector` round-trips as today. Measured — and it refuted two of
//! the ADR's own sentences, both in the direction that makes this step smaller:
//!
//! * *"This node does not store an `int2vector`"* — **it does**. `CREATE TABLE (iv int2vector)` is
//!   accepted, the value goes in, and it reads back. So the storage form is not an open question
//!   and this step does not touch it: the user decided 2026-09-10 that the stored representation
//!   stays as it is and the cost of changing it later is written into the ADR.
//! * *"The zero-based lower bound is the risk"* — **it is already right**: `array_lower` answers 0
//!   and `(iv)[0]` is the first element, both from the computed path.
//!
//! # The cast `pg_cast` does not show
//!
//! Measured on 19beta1 (`127.0.0.1:55432`, 2026-09-10):
//!
//! ```text
//! '1 2 3'::int2vector::int2[]     [0:2]={1,2,3}    pg_cast rows for the pair: **0**, either way
//! '{1,2}'::int2[]::int2vector     42846            one direction only
//! '1 2 3'::text::int2[]           22P02 malformed array literal
//! ```
//!
//! The last line is the one that says what this is: an `int2vector` prints `1 2 3`, which
//! `array_in` refuses, so the cast is **not** a text round trip. PostgreSQL's vectors *are* arrays
//! underneath and the coercion is binary — a third case beside "a `pg_cast` row" and "out of a
//! string type", and one a client reading `pg_cast` cannot see. So this node keeps it as a rule in
//! `casts_to` and **not** a row in `CASTS`, which is also what `pg_cast` reports: a row there
//! would be this node claiming a cast the oracle does not list.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **The cast, both vectors, with the zero-based bounds in the text.**
#[test]
fn a_vector_casts_to_its_array_from_zero() {
    let mut node = parity::Node::new(&[]);
    for (written, answer) in [
        ("'1 2 3'::int2vector::int2[]", "[0:2]={1,2,3}"),
        ("'25 1043'::oidvector::oid[]", "[0:1]={25,1043}"),
        // One element, and the bounds still say where it starts.
        ("'7'::int2vector::int2[]", "[0:0]={7}"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT ({written})::text")),
            vec![vec![answer]],
            "{written} is {answer} on 19beta1"
        );
    }
    assert_eq!(
        node.rows("SELECT pg_typeof('1 2 3'::int2vector::int2[])"),
        vec![vec!["smallint[]"]]
    );
}

/// **One direction only**, which is what says the vectors are not interchangeable with arrays.
#[test]
fn an_array_does_not_cast_to_a_vector() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.answer("SELECT '{1,2}'::int2[]::int2vector")
            .to_string(),
        "!42846 cannot cast type smallint[] to int2vector",
        "19beta1 refuses this half"
    );
}

/// **Through a column**, because the operand's declared type is what tells this cast from a
/// `text` one — and a column is the shape that carries it.
#[test]
fn the_cast_reads_the_columns_declared_type() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE vv (id bigint primary key, iv int2vector)",
        "INSERT INTO vv VALUES (1, '1 2 3')",
    ]);
    assert_eq!(
        node.rows("SELECT (iv::int2[])::text FROM vv"),
        vec![vec!["[0:2]={1,2,3}"]]
    );
    // **The stored value is unchanged**, which is the whole promise of this step.
    assert_eq!(node.rows("SELECT (iv)::text FROM vv"), vec![vec!["1 2 3"]]);
    assert_eq!(
        node.rows("SELECT pg_typeof(iv) FROM vv"),
        vec![vec!["int2vector"]]
    );
    // And a `text` column holding the same characters still cannot become an array, which is the
    // counter-example that makes the arm above a rule about the *type* and not about the bytes.
    node.run("CREATE TABLE tt (t text)").unwrap();
    node.run("INSERT INTO tt VALUES ('1 2 3')").unwrap();
    assert!(
        node.answer("SELECT t::int2[] FROM tt")
            .to_string()
            .starts_with('!'),
        "19beta1: malformed array literal"
    );
}

/// **The zero-based half was already right**, and this pins it so that step 2's other pieces
/// cannot quietly change it.
#[test]
fn a_vector_is_subscripted_from_zero() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT array_lower('1 2 3'::int2vector, 1)::text"),
        vec![vec!["0"]]
    );
    assert_eq!(
        node.rows("SELECT array_upper('1 2 3'::int2vector, 1)::text"),
        vec![vec!["2"]]
    );
    assert_eq!(
        node.rows("SELECT ('1 2 3'::int2vector)[0]::text"),
        vec![vec!["1"]],
        "the first element is at zero, which is the whole of what a vector is"
    );
}

/// **A vector's `typarray` names a row that is there**, which is the sentence this half exists for.
///
/// `value.rs` introduces the array types with it — *"a `typarray` naming a row that is not there is
/// worse than a zero, because a client walks the link in both directions and finds half of it"* —
/// and the two vectors were the pair it had skipped. 19beta1: `int2vector` → `int2vector[]` (1006)
/// and `oidvector` → `oidvector[]` (1013), both built-in oids rather than an extension's.
#[test]
fn a_vector_has_its_array_type() {
    let mut node = parity::Node::new(&[]);
    for (vector, array) in [("int2vector", "_int2vector"), ("oidvector", "_oidvector")] {
        assert_eq!(
            node.rows(&format!(
                "SELECT a.typname, a.typcategory, e.typname FROM pg_type a \
                 JOIN pg_type e ON e.oid = a.typelem WHERE a.typname = '{array}'"
            )),
            vec![vec![array, "A", vector]],
            "{array} is category A with {vector} as its element"
        );
        assert_eq!(
            node.rows(&format!(
                "SELECT typarray::text FROM pg_type WHERE typname = '{vector}'"
            )),
            node.rows(&format!(
                "SELECT oid::text FROM pg_type WHERE typname = '{array}'"
            )),
            "and the link is walkable in both directions"
        );
    }
    // **PostgreSQL's own numbers**, because these two are built in where `lquery[]` is an
    // extension's and gets one of this node's.
    assert_eq!(
        node.rows("SELECT oid::text FROM pg_type WHERE typname = '_int2vector'"),
        vec![vec!["1006"]]
    );
    assert_eq!(
        node.rows("SELECT oid::text FROM pg_type WHERE typname = '_oidvector'"),
        vec![vec!["1013"]]
    );
}

/// **An array of a vector is an array of it**, which is what `ARRAY[…]` asks of its element and
/// what the F6 census pinned as still answering `text[]`.
#[test]
fn an_array_of_a_vector_is_the_vectors_array() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT pg_typeof(ARRAY['1 2'::int2vector])"),
        vec![vec!["int2vector[]"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(ARRAY['25 1043'::oidvector])"),
        vec![vec!["oidvector[]"]]
    );
    // **The element comes back as itself**, which is the half a type name does not prove.
    assert_eq!(
        node.rows("SELECT pg_typeof((ARRAY['1 2'::int2vector])[1])"),
        vec![vec!["int2vector"]]
    );
    assert_eq!(
        node.rows("SELECT ((ARRAY['1 2'::int2vector])[1])::text"),
        vec![vec!["1 2"]]
    );
}

/// **Accumulating a vector gives the vector back**, because a vector *is* an array — the same rule
/// `array_agg(bigint[])` is `bigint[]` by, and the one giving the vectors an array type could have
/// broken in the other direction.
///
/// Measured on 19beta1: `pg_typeof(array_agg(x))` over an `int2vector` is **`int2vector`**, not
/// `int2vector[]`. `tests/captures/pg19_array_of_void.txt` recorded it while measuring F5 —
/// *"`array_agg` over an array is the array, not an array of it"* — and this is where that
/// is now load-bearing: `ColumnType::element_of` cannot say a vector is an array, because a
/// vector's oid is derived through it and would come back as `_int2`'s.
#[test]
fn accumulating_a_vector_gives_the_vector() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT pg_typeof(array_agg(x)) FROM (SELECT '1 2'::int2vector x) s"),
        vec![vec!["int2vector"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(array_agg(x)) FROM (SELECT '25 1043'::oidvector x) s"),
        vec![vec!["oidvector"]]
    );
    // The neighbour that says this is a rule about arrays and not about vectors.
    assert_eq!(
        node.rows("SELECT pg_typeof(array_agg(x)) FROM (SELECT '{1}'::bigint[] x) s"),
        vec![vec!["bigint[]"]]
    );
}

/// **A vector's elements**: the quantifier and `unnest`, which are the two shapes that ask what is
/// *inside* a vector rather than what it is.
///
/// Measured on 19beta1: `2 = ANY('1 2 3'::int2vector)` is `true`, and `unnest` of one yields
/// `smallint` (`oid` for an `oidvector`).
#[test]
fn a_quantifier_and_unnest_see_a_vectors_elements() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE vv (id bigint primary key, iv int2vector)",
        "INSERT INTO vv VALUES (1, '1 2 3')",
    ]);
    // **Both routes**, because they were two answers: the literal went through the lowering
    // shortcut that reads an array literal — and a vector is not written with braces — while the
    // column went through the evaluator, which reads both forms.
    for written in ["'1 2 3'::int2vector", "iv"] {
        assert_eq!(
            node.rows(&format!("SELECT (2 = ANY({written}))::text FROM vv")),
            vec![vec!["true"]],
            "2 = ANY({written})"
        );
        assert_eq!(
            node.rows(&format!("SELECT (9 = ANY({written}))::text FROM vv")),
            vec![vec!["false"]],
            "and a value it does not hold"
        );
    }
    // `unnest` yields the element's own type, not `text`.
    assert_eq!(
        node.rows("SELECT pg_typeof(u) FROM unnest('1 2 3'::int2vector) u LIMIT 1"),
        vec![vec!["smallint"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(u) FROM unnest('25 1043'::oidvector) u LIMIT 1"),
        vec![vec!["oid"]]
    );
    assert_eq!(
        node.rows("SELECT string_agg(u::text, ',') FROM unnest('1 2 3'::int2vector) u"),
        vec![vec!["1,2,3"]],
        "and the values are the vector's, in order"
    );
}

/// **A vector casts to every array its element casts to**, which is what the residue of the cast
/// matrix said and what one rule gives instead of two pairs.
///
/// `int2vector`'s targets on 19beta1 are exactly `{ T[] : smallint -> T }` and `oidvector`'s are
/// `{ T[] : oid -> T }` — read off the capture's own `ok` list, where `smallint` reaches fifteen
/// types and `int2vector` reaches the array of each of those fifteen and nothing else. The node
/// had the two identity pairs (`int2vector -> smallint[]`, `oidvector -> oid[]`) written as a
/// `matches!` of two tuples, and refused the other twenty-five with `42846`.
///
/// Measured on 19beta1, 2026-09-10 — the **zero** lower bound comes through every one of them,
/// which is the half of a vector that is not its element:
///
/// ```text
/// '1 2'::int2vector::integer[]    [0:1]={1,2}      '1 2'::int2vector::text[]     [0:1]={1,2}
/// '1 2'::oidvector::bigint[]      [0:1]={1,2}      '1 2'::int2vector::numeric[]  [0:1]={1,2}
/// ```
#[test]
fn a_vector_casts_to_every_array_its_element_does() {
    let mut node = parity::Node::new(&[]);
    for target in [
        "smallint[]",
        "integer[]",
        "bigint[]",
        "numeric[]",
        "real[]",
        "double precision[]",
        "text[]",
        "character varying[]",
        "name[]",
        "citext[]",
        "oid[]",
    ] {
        assert_eq!(
            node.rows(&format!("SELECT ('1 2'::int2vector::{target})::text")),
            vec![vec!["[0:1]={1,2}"]],
            "'1 2'::int2vector::{target}"
        );
    }
    for target in [
        "oid[]",
        "integer[]",
        "bigint[]",
        "text[]",
        "name[]",
        "character varying[]",
    ] {
        assert_eq!(
            node.rows(&format!("SELECT ('1 2'::oidvector::{target})::text")),
            vec![vec!["[0:1]={1,2}"]],
            "'1 2'::oidvector::{target}"
        );
    }
    // **The element's own refusals are the vector's**, which is what makes this one rule rather
    // than a longer list: `smallint` has no cast to `date` or to `interval`, and 19beta1 refuses
    // both of these as `42846` too.
    for written in [
        "'1 2'::int2vector::date[]",
        "'1 2'::int2vector::interval[]",
        "'1 2'::oidvector::double precision[]",
        // And a non-array target is still refused: a vector does not become a scalar.
        "'1 2'::int2vector::integer",
    ] {
        assert!(
            node.answer(&format!("SELECT {written}"))
                .to_string()
                .starts_with("!42846"),
            "{written}: {}",
            node.answer(&format!("SELECT {written}"))
        );
    }
}
