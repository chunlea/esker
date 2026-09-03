//! `DROP FUNCTION` — statement 757 of `postgresql_specific_schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is about the function namespace.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **`pg_proc` exists now** and the entry that stood here is deleted (ADR 0031, rule 2).
        // It said the view was not implemented; the define-only `CREATE FUNCTION` unit added it,
        // and this line — a count of functions nobody created — agrees at zero.
        (
            "DROP FUNCTION lower",
            "**`lower` is overloaded on a real server and is not here**, so the two disagree about \
             whether the name is ambiguous. PostgreSQL has `lower(text)` and `lower(anyrange)` \
             and answers `42725 … is not unique`; this node has one `lower` and resolves the name \
             to it, which is the same rule applied to a smaller catalog rather than a different \
             rule. The ambiguity itself is not reachable here until two functions share a name — \
             `DROP FUNCTION` selects by name when no argument list is written, which is what the \
             agreeing lines above check.",
        ),
        (
            "DROP FUNCTION IF EXISTS lower",
            "The same, and it is worth its own line because `IF EXISTS` does **not** rescue it on \
             a real server: `42725` is not absence. Both servers agree that the clause changes \
             nothing here; they disagree only about whether the name is ambiguous.",
        ),
    ],
};

#[test]
fn every_drop_function_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_drop_function.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 20,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The suite's own line: a function that is not there, with `IF EXISTS`, is a success.
#[test]
fn if_exists_on_an_absent_function_is_a_success() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "DROP FUNCTION IF EXISTS partitioned_insert_trigger()",
        // No argument list is a **different statement** — "any arity" — and it is accepted too.
        "DROP FUNCTION IF EXISTS partitioned_insert_trigger",
        "DROP FUNCTION IF EXISTS nosuchfunc(integer, text)",
        "DROP FUNCTION IF EXISTS f1(), f2()",
        // An unknown schema is still just absence.
        "DROP FUNCTION IF EXISTS nosuch.f()",
    ] {
        node.run(statement).unwrap();
    }
}

/// Without `IF EXISTS`, absence is `42883` — and **two different messages**.
///
/// With an argument list there is a signature to name; without one there is not, and PostgreSQL
/// says so in different words. A node that reused one sentence would be wrong half the time.
#[test]
fn absence_without_if_exists_is_42883_in_two_shapes() {
    let mut node = parity::Node::new(&[]);
    let error = node
        .run("DROP FUNCTION partitioned_insert_trigger()")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    assert_eq!(
        error.to_string(),
        "function partitioned_insert_trigger() does not exist"
    );

    let error = node.run("DROP FUNCTION nosuchfunc").unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    assert_eq!(
        error.to_string(),
        "could not find a function named \"nosuchfunc\""
    );
}

/// **A built-in is protected, and `IF EXISTS` does not cover it.**
///
/// This is what makes `DROP FUNCTION` more than a no-op on a server with no user functions: the
/// functions this node *does* have cannot be dropped, and it says so in PostgreSQL's words. A node
/// that answered success to everything would let a schema drop `lower` and report that it had.
#[test]
fn a_built_in_is_required_by_the_database_system() {
    let mut node = parity::Node::new(&[]);
    for written in [
        "DROP FUNCTION lower(text)",
        "DROP FUNCTION IF EXISTS lower(text)",
        // The schema qualifier is accepted and does not appear in the message.
        "DROP FUNCTION IF EXISTS pg_catalog.lower(text)",
    ] {
        let error = node.run(written).unwrap_err();
        assert_eq!(error.sqlstate(), "2BP01", "for {written}");
        assert_eq!(
            error.to_string(),
            "cannot drop function lower(text) because it is required by the database system",
            "for {written}"
        );
    }
    // **The canonical signature, not the written one**: `VARIADIC` goes, and so does the space.
    let error = node
        .run("DROP FUNCTION concat(VARIADIC \"any\")")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "cannot drop function concat(\"any\") because it is required by the database system"
    );
    let error = node
        .run("DROP FUNCTION convert_to(text, name)")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "cannot drop function convert_to(text,name) because it is required by the database system"
    );
}

/// The **signature** decides, not the name: a built-in called with other types is simply absent.
#[test]
fn the_signature_has_to_match() {
    let mut node = parity::Node::new(&[]);
    let error = node.run("DROP FUNCTION lower(integer)").unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    assert_eq!(error.to_string(), "function lower(integer) does not exist");
    // `concat`'s real signature is `concat("any")`, so this one is absent too.
    assert_eq!(
        node.run("DROP FUNCTION concat(text)")
            .unwrap_err()
            .sqlstate(),
        "42883"
    );
    // And `IF EXISTS` covers a signature that does not match, because that is absence.
    node.run("DROP FUNCTION IF EXISTS lower(integer)").unwrap();
}

/// **The protected list is the function vocabulary**, and this is what keeps them in step.
///
/// `DROP FUNCTION` answers "does not exist" for any name outside `BUILT_IN_FUNCTIONS`, so a
/// function added to the language and forgotten there would be callable and droppable at once —
/// reporting success for something that still works. Every name the language resolves must be on
/// the list; this walks the ones with a public spelling.
#[test]
fn every_callable_function_is_protected() {
    let mut node = parity::Node::new(&[]);
    for name in [
        "lower",
        "upper",
        "random",
        "concat",
        "convert_to",
        "now",
        "gen_random_uuid",
        "uuid_generate_v4",
        "nextval",
        "currval",
        "lastval",
        "setval",
        "format_type",
        "pg_get_expr",
        "current_schema",
        "current_schemas",
        "array_length",
        "array_position",
    ] {
        let error = node.run(&format!("DROP FUNCTION {name}")).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            "2BP01",
            "{name} is callable and is not protected from DROP FUNCTION"
        );
    }
}
