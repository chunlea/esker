//! `gen_random_uuid()` and `uuid_generate_v4()` — what the two extensions promise.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a function of the session.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_uuid_function_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_uuid_functions.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 7,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// One is core and one is gated, and the gate closes again when the install rolls back.
///
/// `gen_random_uuid` has been in core since PostgreSQL 13, so it answers with neither extension
/// installed. `uuid_generate_v4` is `uuid-ossp`'s and is `42883` until that extension is there —
/// which is the whole reason the allowlist has to carry the functions and not just the name.
#[test]
fn one_is_core_and_the_other_is_gated_on_its_extension() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(node.rows("SELECT gen_random_uuid() IS NULL"), [["f"]]);

    let error = node.run("SELECT uuid_generate_v4()").unwrap_err();
    assert_eq!(error.sqlstate(), "42883");
    assert_eq!(
        error.detail().as_deref(),
        Some("There is no function of that name."),
        "the name form of the DETAIL, not the argument-types one"
    );

    node.run("CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"")
        .unwrap();
    assert_eq!(node.rows("SELECT uuid_generate_v4() IS NULL"), [["f"]]);

    // And it is gone again when the install goes back, because the gate reads the catalog.
    let mut node = parity::Node::new(&[]);
    for statement in [
        "BEGIN",
        "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"",
        "ROLLBACK",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.run("SELECT uuid_generate_v4()")
            .unwrap_err()
            .sqlstate(),
        "42883"
    );
}

/// It is a **version 4** UUID, not sixteen random bytes with hyphens in them.
///
/// The version nibble is the 15th character and the variant is the 20th — the two places a
/// formatter that skipped them would still pass a length check. Both are asserted over many
/// values, because one draw can be right by chance.
#[test]
fn every_value_is_a_v4_uuid_and_no_two_are_alike() {
    let mut node = parity::Node::new(&[]);
    let mut seen = BTreeSet::new();
    for _ in 0..64 {
        let rows = node.rows("SELECT gen_random_uuid()::text");
        let value = rows[0][0].clone();
        assert_eq!(value.len(), 36, "36 characters including four hyphens");
        let bytes: Vec<char> = value.chars().collect();
        assert_eq!(bytes[8], '-', "{value}");
        assert_eq!(bytes[13], '-', "{value}");
        assert_eq!(bytes[18], '-', "{value}");
        assert_eq!(bytes[23], '-', "{value}");
        assert_eq!(bytes[14], '4', "the version nibble, in {value}");
        assert!(
            matches!(bytes[19], '8' | '9' | 'a' | 'b'),
            "the variant bits, in {value}"
        );
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase() || c == '-'),
            "lower-case hex, in {value}"
        );
        seen.insert(value);
    }
    assert_eq!(seen.len(), 64, "64 draws, 64 different values");
}

/// Two calls in **one statement** differ, which is what makes it usable as a column default.
#[test]
fn it_is_volatile_within_a_statement() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT gen_random_uuid() = gen_random_uuid()"),
        [["f"]]
    );
}
