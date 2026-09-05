//! `LIKE` and `NOT LIKE`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a function of its own literals.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT 100 LIKE '1%'",
        "**The same message about a different width**: `integer ~~ unknown` there and \
         `bigint ~~ unknown` here, because an unsuffixed integer literal is an `int4` on a real \
         server and an `int8` on this one. The standing literal-width divergence, not a `LIKE` \
         one — the operator, the code, the DETAIL and the HINT all agree.",
        "UNMEASURED",
    )],
};

#[test]
fn every_like_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_like.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 10,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The two wildcards, and the escape that turns each of them back into a character.
#[test]
fn the_wildcards_and_the_escape() {
    let mut node = parity::Node::new(&["CREATE TABLE l (s text)"]);
    for value in ["abc", "a%c", "a_c", "ABC", ""] {
        node.run(&format!("INSERT INTO l (s) VALUES ('{value}')"))
            .unwrap();
    }
    // `%` spans anything including nothing; `_` is exactly one character.
    assert_eq!(
        node.rows("SELECT s FROM l WHERE s LIKE 'a%c' ORDER BY s"),
        [["a%c"], ["a_c"], ["abc"]]
    );
    assert_eq!(
        node.rows("SELECT s FROM l WHERE s LIKE 'a_c' ORDER BY s"),
        [["a%c"], ["a_c"], ["abc"]]
    );
    // **The escape needs no `ESCAPE` clause**: `\%` is the character, not the wildcard.
    assert_eq!(node.rows("SELECT s FROM l WHERE s LIKE 'a\\%c'"), [["a%c"]]);
    assert_eq!(node.rows("SELECT s FROM l WHERE s LIKE 'a\\_c'"), [["a_c"]]);
    // A bare `%` takes everything, the empty string included.
    assert_eq!(
        node.rows("SELECT count(*) FROM l WHERE s LIKE '%'"),
        [["5"]]
    );
    // Case-sensitive.
    assert_eq!(
        node.rows("SELECT count(*) FROM l WHERE s LIKE 'A%'"),
        [["1"]]
    );
}

/// **NULL on either side is NULL**, so it matches nothing and `NOT LIKE` does not rescue it.
#[test]
fn a_null_is_unknown_rather_than_false() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT NULL LIKE 'a%', 'abc' LIKE NULL"),
        [["\\N", "\\N"]]
    );
    let mut node = parity::Node::new(&["CREATE TABLE l (s text)"]);
    node.run("INSERT INTO l (s) VALUES (NULL)").unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM l WHERE s LIKE '%'"),
        [["0"]]
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM l WHERE s NOT LIKE '%'"),
        [["0"]],
        "unknown either way — a NULL is not matched by the negation either"
    );
}

/// `NOT LIKE` is the negation, and a non-text operand is `42883` naming `~~`.
#[test]
fn not_like_negates_and_a_number_has_no_operator() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("SELECT 'abc' NOT LIKE 'a%'"), [["f"]]);
    assert_eq!(node.rows("SELECT 'abc' NOT LIKE 'b%'"), [["t"]]);
    let error = node.run("SELECT 100 LIKE '1%'").unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    assert!(error.to_string().contains("~~"), "{error}");
}

/// `ILIKE` is the same matcher with both sides folded, and `ESCAPE` names another character.
#[test]
fn ilike_folds_and_escape_replaces_the_backslash() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 'ABC' ILIKE 'a%', 'abc' ILIKE 'A_C', 'abc' NOT ILIKE 'a%'"),
        [["t", "t", "f"]]
    );
    // A NULL is unknown under `ILIKE` too.
    assert_eq!(node.rows("SELECT NULL ILIKE 'a%'"), [["\\N"]]);
    // **`ESCAPE '#'` replaces the backslash rather than joining it**: `#%` is the character and a
    // lone `\` is now ordinary.
    assert_eq!(node.rows("SELECT 'a%c' LIKE 'a#%c' ESCAPE '#'"), [["t"]]);
}
