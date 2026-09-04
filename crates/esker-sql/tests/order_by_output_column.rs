//! `ORDER BY <name>` prefers an **output** column to an input one.
//!
//! `debts-v1.md` #9, opened one unit ago. `crate::exec::query::order_keys` implemented the narrow
//! half of PostgreSQL's rule — two output columns of one name are `42702` — and its comment said
//! the other half, the *preference*, would be invented because nothing had measured it. An
//! `ALTER TYPE` capture then measured it by accident: the corpus wrote the ambiguous spelling and
//! the two servers disagreed about the **ordering** rather than about the enum.
//!
//! ```text
//! SELECT m::text      FROM t ORDER BY m   -- the TEXT: m::text is *named* m
//! SELECT m::text AS x FROM t ORDER BY m   -- the enum: the input column
//! ```
//!
//! An enum is the sharpest way to see it, because its two orders disagree: declaration order is
//! `sad, ok, happy` and alphabetical is `happy, ok, sad`. The alias is the whole difference.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn moods() -> parity::Node {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TYPE ob_mood AS ENUM ('sad', 'ok', 'happy')",
        "CREATE TABLE ob_t (m ob_mood)",
        "INSERT INTO ob_t VALUES ('sad'), ('ok'), ('happy')",
    ] {
        node.run(statement).unwrap();
    }
    node
}

/// **The derived output name wins**, so the sort is the text's and not the enum's.
#[test]
fn a_derived_output_name_beats_the_column_it_shadows() {
    let mut node = moods();
    assert_eq!(
        node.rows("SELECT m::text FROM ob_t ORDER BY m"),
        vec![vec!["happy"], vec!["ok"], vec!["sad"]]
    );
}

/// **An alias moves the name off the projection**, so `m` is the input column again — which is why
/// one keystroke changes the answer, and why the corpus that found this now writes the alias.
#[test]
fn an_alias_leaves_the_name_to_the_input_column() {
    let mut node = moods();
    assert_eq!(
        node.rows("SELECT m::text AS as_text FROM ob_t ORDER BY m"),
        vec![vec!["sad"], vec!["ok"], vec!["happy"]]
    );
}

/// The plain case is unchanged: a target entry that **is** the column produces the same expression
/// either way, which is what keeps every aggregated query on the path it already took.
#[test]
fn a_column_that_names_itself_orders_as_it_always_did() {
    let mut node = moods();
    assert_eq!(
        node.rows("SELECT m FROM ob_t ORDER BY m"),
        vec![vec!["sad"], vec!["ok"], vec!["happy"]]
    );
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE ob_g (g text, n int8)").unwrap();
    node.run("INSERT INTO ob_g VALUES ('b', 1), ('a', 2)")
        .unwrap();
    // The shape that caught a first attempt at this: the grouping key must still resolve as one.
    assert_eq!(
        node.rows("SELECT g, count(*) FROM ob_g GROUP BY g ORDER BY g"),
        vec![vec!["a", "1"], vec!["b", "1"]]
    );
}

/// Two output columns of one name are still `42702`, which is the half that already worked.
#[test]
fn two_outputs_of_one_name_are_still_ambiguous() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TABLE ob_two (a int8, b int8)").unwrap();
    let error = node
        .run("SELECT a AS x, b AS x FROM ob_two ORDER BY x")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42702");
}
