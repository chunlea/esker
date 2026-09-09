//! **`<op> ANY (array)` and `<op> ALL (array)`** — the quantifier over a list of values.
//!
//! `docs/plans/debts-v1.1.md` #21, and its first sentence was wrong in the useful direction: the
//! node does have an `ALL` quantifier, and it has had the whole three-valued rule for as long as
//! subqueries have compared. `plan::SubqueryKind::Quantified { op, all }` carries all six
//! comparison operators and `exec::subquery::quantified` decides them — empty settles it with no
//! comparison, one decisive element settles it past any NULL, otherwise a NULL leaves it unknown.
//! What was missing is the **array** form of the same thing for every operator but `=`:
//! `parse::lower` answered `0A000 ALL over an array` and `0A000 the quantifier <op> ANY`.
//!
//! So this is not a new rule; it is the rule reaching its second right-hand side. The corpus
//! measures both sides against the oracle in one session so they cannot drift apart, including the
//! two refusals PostgreSQL keeps (`42809` for a non-array right side, `42883` for an element type
//! with no operator) and the deparse, where **`NOT IN` and `<> ALL` are the same printed text**.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the one table it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The standing `regtype`-is-`text` trade**, and the row underneath it agrees: `pg_typeof` of
    // a quantified comparison is `boolean` on both servers, declared `regtype` there and `text`
    // here ([ADR 0077](../../../docs/adr/0077-regtype-is-an-oid-that-prints-as-a-name.md)).
    // Listed for its *type* rather than as an answer, which is parity rule 4's whole subject.
    types: &["SELECT 'r', pg_typeof(1 = ALL (ARRAY[1]))"],
    answers: &[(
        "SELECT 'r', 1 = ALL (ARRAY[ARRAY[1,1], ARRAY[1,1]])",
        "**A multidimensional array, which this node's type surface does not have.** PostgreSQL's \
         arrays are flat with dimensions on the side, so `ARRAY[ARRAY[1,1], ARRAY[1,1]]` is one \
         `integer[]` holding four `1`s and `ALL` compares against each of them — `t`. Here an \
         array is a column type over **one element type** \
         ([ADR 0047](../../../docs/adr/0047-an-array-is-a-column-type-over-one-element-type.md)), \
         so an array of arrays has `text` elements and the comparison is \
         `42883 operator does not exist: integer = text`. The refusal names the types rather than \
         answering a wrong row, which is the contract for a type this node does not have; the \
         quantifier itself is not involved, and the row is here because that is the honest place \
         to record what a reader of this corpus would otherwise have to discover.",
        "pg19_all_quantifier.txt:76",
    )],
};

#[test]
fn every_quantified_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_all_quantifier.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 60, "the corpus shrank: {checked} statements");
}

/// **The rule at its own edges**, asserted directly because a corpus row cannot say *why*.
///
/// Each pair below is one clause of the three-valued rule, and the two members of a pair differ in
/// exactly the thing the clause is about — so a wrong implementation cannot pass both.
#[test]
fn the_quantifier_is_three_valued_over_the_elements() {
    let mut node = parity::Node::new(&[]);
    // An empty array settles it with **no comparison at all**, which is why a NULL operand does
    // not make it unknown: nothing is compared, so nothing is unknown.
    assert_eq!(
        node.rows("SELECT 1 = ALL (ARRAY[]::integer[])"),
        vec![vec!["t"]]
    );
    assert_eq!(
        node.rows("SELECT 1 = ANY (ARRAY[]::integer[])"),
        vec![vec!["f"]]
    );
    assert_eq!(
        node.rows("SELECT NULL = ALL (ARRAY[]::integer[])"),
        vec![vec!["t"]],
        "an empty ALL is true before the operand is looked at"
    );
    // One decisive element settles it **past** a NULL, in both directions.
    assert_eq!(
        node.rows("SELECT 1 = ALL (ARRAY[2, NULL])"),
        vec![vec!["f"]]
    );
    assert_eq!(
        node.rows("SELECT 1 = ANY (ARRAY[1, NULL])"),
        vec![vec!["t"]]
    );
    // With nothing decisive, a NULL leaves it unknown — and this is the pair that catches an
    // implementation that treats NULL as a short circuit rather than as an unknown to remember.
    assert_eq!(
        node.rows("SELECT 1 = ALL (ARRAY[1, NULL])"),
        vec![vec!["\\N"]]
    );
    assert_eq!(
        node.rows("SELECT 1 <> ALL (ARRAY[2, NULL])"),
        vec![vec!["\\N"]]
    );
    assert_eq!(
        node.rows("SELECT 1 = ANY (ARRAY[2, NULL])"),
        vec![vec!["\\N"]]
    );
    // And the operator is the operator, not a rewrite of `=`.
    assert_eq!(node.rows("SELECT 3 > ALL (ARRAY[1, 2])"), vec![vec!["t"]]);
    assert_eq!(node.rows("SELECT 1 > ALL (ARRAY[1, 2])"), vec![vec!["f"]]);
    assert_eq!(
        node.rows("SELECT 1 > ALL (ARRAY[2, NULL])"),
        vec![vec!["f"]]
    );
    assert_eq!(
        node.rows("SELECT 1 > ALL (ARRAY[0, NULL])"),
        vec![vec!["\\N"]]
    );
}

/// **The two refusals PostgreSQL keeps**, which a node that accepted everything would lose.
#[test]
fn a_non_array_right_side_and_a_type_with_no_operator_are_refused() {
    let mut node = parity::Node::new(&[]);
    let scalar = node.run("SELECT 1 = ALL (1)").unwrap_err();
    assert_eq!(scalar.sqlstate(), "42809");
    assert_eq!(
        scalar.to_string(),
        "op ANY/ALL (array) requires array on right side"
    );
    let mixed = node.run("SELECT 1 = ALL (ARRAY['a'])").unwrap_err();
    assert_eq!(mixed.sqlstate(), "42883");
    assert_eq!(mixed.to_string(), "operator does not exist: integer = text");
}

/// **`NOT IN` and `<> ALL` are one stored expression**, and it computes.
///
/// The deparse is in the corpus; what is here is the consequence, because
/// `tests/corpus/pg19_all_quantifier.txt` reads `pg_attrdef` and a printed form that does not read
/// back computes the wrong value ([ADR 0090](../../../docs/adr/0090-a-stored-expression-is-deparsed-by-the-statement-that-writes-it.md)).
#[test]
fn a_quantified_generated_column_stores_one_text_and_computes_it() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE gq (id int8 PRIMARY KEY, c1 integer, t text)",
        "ALTER TABLE gq ADD COLUMN q_notin boolean GENERATED ALWAYS AS (c1 NOT IN (1, 2)) STORED",
        "ALTER TABLE gq ADD COLUMN q_all boolean GENERATED ALWAYS AS (c1 <> ALL (ARRAY[1, 2])) STORED",
        "ALTER TABLE gq ADD COLUMN q_gt boolean GENERATED ALWAYS AS (c1 > ALL (ARRAY[1, 2])) STORED",
        "INSERT INTO gq (id, c1, t) VALUES (1, 3, 'z'), (2, 1, 'a')",
    ]);
    assert_eq!(
        node.rows("SELECT c1, q_notin, q_all, q_gt FROM gq ORDER BY id"),
        vec![vec!["3", "t", "t", "t"], vec!["1", "f", "f", "f"]],
    );
    // The two spellings are the same text in the catalog, which is what makes them one node.
    let printed = node.rows(
        "SELECT a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d \
         ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'gq'::regclass AND \
         a.attname IN ('q_notin', 'q_all') ORDER BY a.attname",
    );
    assert_eq!(
        printed[0][1], printed[1][1],
        "one printed form, two spellings"
    );
    assert_eq!(printed[0][1], "(c1 <> ALL (ARRAY[1, 2]))");
    // And it recomputes, so the stored text is parsed again rather than only once.
    node.run("UPDATE gq SET c1 = 9 WHERE id = 2").unwrap();
    assert_eq!(
        node.rows("SELECT q_notin, q_all, q_gt FROM gq WHERE id = 2"),
        vec![vec!["t", "t", "t"]],
    );
}
