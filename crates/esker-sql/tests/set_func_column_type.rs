//! **A set-returning function in `FROM` carries its element type** — wire v3 family **F10**.
//!
//! The same call has two readers and they were two answers:
//!
//! ```text
//! SELECT unnest(ARRAY['2020-01-01'::date])            date    the projection, always right
//! SELECT u FROM unnest(ARRAY['2020-01-01'::date]) u   text    the FROM clause
//! ```
//!
//! The projection asks `expr_type`'s `Expr::SetFunc` arm, which asks
//! `exec::table_function::result_type`, which asks `expr_type` of the argument — complete.
//! The `FROM` clause asks `exec::subquery::unnest_element`, which read **two** of the shapes an
//! argument can have — a qualified column and an already-folded typed literal — and answered
//! `text` for everything else. `ARRAY[…]` is everything else, and so is a cast that has not
//! folded.
//!
//! **It cannot simply call `expr_type`**: this def is what a scope is built *from*, so there is no
//! scope yet. What it can do is read every shape that carries its type **syntactically**, which is
//! what a constructor and a cast do.
//!
//! **Why it matters more than a type name**: the first field of a `RowDescription` is what
//! `ActiveRecord` decodes by, so a `date` column described as `text` comes back a String. The same
//! failure as the `->` fetch and the domain-column wire regression.
//!
//! Measured over all 100 spellings of the wire v3 probe list, four array shapes each
//! (`tests/captures/pg19_array_of_void.txt`): **50 of them** took `text` in the `FROM` shape and
//! their own element type in the projection.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **The same argument, the two shapes, one answer.**
#[test]
fn the_from_clause_and_the_projection_agree() {
    let mut node = parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS hstore"]);
    for (written, element) in [
        ("ARRAY['2020-01-01'::date]", "date"),
        ("ARRAY[1::bigint]", "bigint"),
        ("ARRAY[1::int4]", "integer"),
        ("ARRAY['x'::text]", "text"),
        ("ARRAY['a=>1'::hstore]", "hstore"),
        ("ARRAY['{\"a\":1}'::jsonb]", "jsonb"),
        (
            "ARRAY['11111111-1111-1111-1111-111111111111'::uuid]",
            "uuid",
        ),
        ("ARRAY['[1,2]'::int4range]", "int4range"),
        ("'{1,2}'::bigint[]", "bigint"),
        ("'{x}'::text[]", "text"),
    ] {
        assert_eq!(
            node.rows(&format!(
                "SELECT pg_typeof(u) FROM unnest({written}) u LIMIT 1"
            )),
            vec![vec![element]],
            "unnest({written}) in FROM is a {element} on 19beta1"
        );
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof(unnest({written})) LIMIT 1")),
            vec![vec![element]],
            "and the projection has always said so"
        );
    }
}

/// **The declared type is the wire type**, which is the half a client reads.
///
/// `pg_typeof` folds at resolution and could agree while the `RowDescription` did not — that pair
/// is exactly the `->` fetch's bug — so the column's own description is asserted too.
#[test]
fn the_row_description_carries_the_element_type() {
    let mut node = parity::Node::new(&[]);
    // `answer` renders "<declared type>\t<value>", so the first field is what the wire carries.
    let answer = node
        .answer("SELECT u FROM unnest(ARRAY['2020-01-01'::date]) u")
        .to_string();
    assert_eq!(
        answer.split('\t').next().unwrap_or(""),
        "date",
        "a date column described as text comes back a String: {answer}"
    );
}

/// **A column argument still works**, which is the shape that already did — `unnest(tags)` is what
/// a schema dump writes, and it must not become the fix's casualty.
#[test]
fn an_unnest_over_a_column_still_carries_its_element() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE t (id bigint, tags text[], nums int8[])",
        "INSERT INTO t VALUES (1, '{a,b}', '{7,8}')",
    ]);
    assert_eq!(
        node.rows("SELECT pg_typeof(u) FROM t, unnest(t.nums) u LIMIT 1"),
        vec![vec!["bigint"]]
    );
    assert_eq!(
        node.rows("SELECT u FROM t, unnest(t.tags) u ORDER BY u"),
        vec![vec!["a"], vec!["b"]]
    );
}

/// **`generate_series` keeps its own rule**, which is not `unnest`'s: it answers its arguments'
/// type, and this node's integer constants make that an `int8` where PostgreSQL's are `int4` —
/// the standing constant-width divergence, and not something this unit moves.
#[test]
fn generate_series_is_unchanged() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT pg_typeof(g) FROM generate_series(1, 3) g LIMIT 1"),
        vec![vec!["bigint"]]
    );
}

/// **Everything `Executor::bound` does was blind to a `FROM` function's argument**, and that is
/// one walker, not one feature.
///
/// `bind::walk_mut` reaches a `FROM` entry's derived table and its `VALUES` rows and did not reach
/// its **function's arguments** — so `::regclass`, `current_schema()`, a cast to a user type, a
/// user function and the parameter substitution itself all skipped them. The `regclass` one said
/// so out loud:
///
/// ```text
/// SELECT u FROM unnest(ARRAY['t'::regclass]) u
///   XX000 internal error: regclass() reached the row evaluator unresolved
/// ```
///
/// while `SELECT unnest(ARRAY['t'::regclass])` — the same call one clause over — answered, because
/// the projection's copy *was* walked. An `XX000` is this crate's own invariant reporting that it
/// was reached in a state it does not handle, which is why it is the row to start from.
#[test]
fn the_bound_pass_reaches_a_from_functions_arguments() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id bigint)"]);
    assert_eq!(
        node.rows("SELECT u::text FROM unnest(ARRAY['t'::regclass]) u"),
        vec![vec!["t"]],
        "a regclass in a FROM function's argument is resolved before the plan, like every other"
    );
    assert_eq!(
        node.rows("SELECT u FROM unnest(ARRAY[current_schema()]) u"),
        vec![vec!["public"]],
        "and so is a current_schema()"
    );
}

/// **The read-only twin sizes the parameter list and the walker substitutes**, so a clause in one
/// and not the other is a parameter counted and never filled — the pair rule `bind` states in its
/// own comment, and the `FROM` function's argument was in neither. Both were fixed together.
///
/// **It cannot be observed through a parameter today**, and this test says why rather than
/// pretending: an `ARRAY[…]` holding one is a *named* refusal
/// (`0A000 an ARRAY constructor holding $1 is not supported`), so the walker never reaches the
/// case. The gap was reachable through `::regclass` and `current_schema()` instead — the test
/// above — and the refusal is pinned here so that the day the constructor takes a parameter, this
/// line goes red and points at the pair.
#[test]
fn an_array_constructor_holding_a_parameter_is_still_a_named_refusal() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.answer("PREPARE p (bigint) AS SELECT u::text FROM unnest(ARRAY[$1]) u")
            .to_string(),
        "!0A000 an ARRAY constructor holding $1 is not supported"
    );
}

/// **The spelling that unit did not close, closed with F8's `reg*` step** — the hundredth of a
/// hundred.
///
/// `unnest(ARRAY['{t}'::regclass[]])` is a `regclass` on 19beta1 and was `text` here: a
/// `regclass[]` literal carries no cast and no folded value, because `lower_regclass_array`
/// rewrites it into an `Expr::Array` of per-element resolutions — so `syntactic_type` had nothing
/// to read. It reads two more shapes now, an array's own element type and a
/// `CatalogFunc::RegClass`, which is what that rewrite leaves behind.
#[test]
fn a_regclass_array_literal_unnests_as_a_regclass() {
    let mut node = parity::Node::new(&["CREATE TABLE t (id bigint)"]);
    assert_eq!(
        node.rows("SELECT pg_typeof(u) FROM unnest(ARRAY['{t}'::regclass[]]) u LIMIT 1"),
        vec![vec!["regclass"]],
        "19beta1 answers regclass, and the first field of a RowDescription is what a client \
         decodes by"
    );
    assert_eq!(
        node.rows("SELECT u::text FROM unnest(ARRAY['{t}'::regclass[]]) u LIMIT 1"),
        vec![vec!["t"]],
        "and the value was already right, which is what made the type the whole of the row"
    );
}
