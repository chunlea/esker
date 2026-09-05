//! **A `WHERE` takes a boolean, and PostgreSQL decides that by type and not by shape.**
//!
//! `check_predicate` decided it by *shape*: a list of `Expr` variants that are predicates, with a
//! fallback that reports the type it got. Every boolean-valued **catalog function** was outside
//! that list, so `WHERE v @@ to_tsquery('english', 'cat')` — the shape `schema_test.rb` writes and
//! the reason `corpus/pg19_tsvector.txt` part 3 could not replay — was refused with
//!
//! ```text
//! 42804 argument of WHERE must be type boolean, not type boolean
//! ```
//!
//! **That sentence is the proof.** A server cannot refuse a boolean for not being a boolean, so
//! any expression the fallback can name as `boolean` was one the list should have taken. Twelve
//! catalog functions answer `ColumnType::Bool`, and four of them are the spellings a client is
//! most likely to put in a `WHERE`: `@@`, `&&`, `@>` and `?`.
//!
//! **`HAVING` was never affected**, and finding out why is what settled the fix: it is checked by
//! `Aggregation::check_boolean`, which reads the type and accepts any boolean. The rule already
//! existed in this crate, written correctly, one clause over.
//!
//! Every row below was put to PostgreSQL 19beta1 in one `BEGIN … ROLLBACK` and answered `1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// One row carrying one value of each type these operators read.
const FIXTURE: &[&str] = &[
    "CREATE TABLE wb (id int8, v tsvector, r int4range, h hstore, p path)",
    "INSERT INTO wb VALUES (1, to_tsvector('english','the fat cat'), '[1,10)'::int4range, \
     'a=>1'::hstore, '((0,0),(1,1))'::path)",
];

/// The twelve, as a client spells them. Each keeps the row on a real server.
const PREDICATES: &[&str] = &[
    // The four that are operators, and the four most likely to be written.
    "v @@ to_tsquery('english','cat')",
    "r && '[5,20)'::int4range",
    "r @> 5",
    "h ? 'a'",
    "h @> 'a=>1'::hstore",
    // And the named functions over the same types.
    "NOT isempty(r)",
    "lower_inc(r)",
    "NOT upper_inc(r)",
    "NOT lower_inf(r)",
    "NOT upper_inf(r)",
    "isclosed(p)",
];

/// **`WHERE <boolean catalog function>` keeps the row**, for every one of them.
#[test]
fn a_boolean_catalog_function_is_a_predicate() {
    let mut node = parity::Node::new(FIXTURE);
    for predicate in PREDICATES {
        let sql = format!("SELECT id FROM wb WHERE {predicate}");
        assert_eq!(node.rows(&sql), [["1".to_owned()]], "{sql}");
    }
}

/// **`JOIN … ON` is the other caller of the same check**, and was broken in the same way.
///
/// `check_predicate` is called for `WHERE` at four sites and for `JOIN/ON` at one, so a join
/// condition that is a boolean catalog function got the identical self-refuting sentence. Measured
/// on 19beta1: the join keeps the row.
#[test]
fn a_join_condition_is_the_same_rule() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("CREATE TABLE wc (id int8)").unwrap();
    node.run("INSERT INTO wc VALUES (1)").unwrap();
    assert_eq!(
        node.rows("SELECT wb.id FROM wb JOIN wc ON wb.v @@ to_tsquery('english','cat')"),
        [["1".to_owned()]]
    );
}

/// **A guard, not a regression.** `HAVING` was never broken: it is checked by
/// `Aggregation::check_boolean`, a *different* function that was already type-based — reading the
/// type and accepting any boolean. That is the shape `check_predicate` has now been given, so this
/// asserts the two clauses agree rather than that either changed.
#[test]
fn having_already_had_the_rule_and_still_does() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT count(*) FROM wb GROUP BY v HAVING v @@ to_tsquery('english','cat')"),
        [["1".to_owned()]]
    );
    // Its refusal names the clause and the type, the way `WHERE`'s does.
    assert_eq!(
        node.answer("SELECT count(*) FROM wb GROUP BY id HAVING id")
            .to_string(),
        "!42804 argument of HAVING must be type boolean, not type bigint"
    );
}

/// **The rule is still a rule.** Widening the check to "any boolean" must not widen it to
/// "anything", so the sentence a real server says for a non-boolean is asserted here too — with
/// the type it actually got, which is the half that makes the message useful.
#[test]
fn a_non_boolean_still_names_its_own_type() {
    let mut node = parity::Node::new(FIXTURE);
    for (predicate, ty) in [("id", "bigint"), ("v", "tsvector"), ("h", "hstore")] {
        assert_eq!(
            node.answer(&format!("SELECT id FROM wb WHERE {predicate}"))
                .to_string(),
            format!("!42804 argument of WHERE must be type boolean, not type {ty}"),
            "{predicate}"
        );
    }
}
