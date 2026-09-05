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
    answers: &[
        // **This node has no MAC address, and RFC 4122 §4.5 says what to do about it**: a node
        // without one uses a *random* node id with the multicast bit set. A real server reads the
        // host's card, so its `uuid_generate_v1()` is unicast and this line is `f` there and `t`
        // here — the one bit of the sixteen bytes that differs, and it differs by the standard's
        // own instruction rather than by an approximation.
        //
        // The half that matters agrees and is checked where it can be: the node id is the **same
        // for every call** of the plain form and **new for every call** of the `mc` one, which is
        // the whole difference between them. A corpus cannot ask it — this node has no
        // `substring` to cut the node bytes out with — so `value::random`'s own test does.
        (
            "SELECT 'r', uuid_generate_v1()::text ~ '^.{24}[0-9a-f][13579bdf]' AS v1_multicast",
            "no MAC address here, so RFC 4122's random multicast node id is used instead",
            "UNMEASURED",
        ),
        // **`uuid_generate_v3` and `v5` hash**, with MD5 and SHA-1, and this project writes its
        // own primitives rather than linking C — so each is a unit of its own and neither is
        // approximated in the meantime. The four **namespace constants** they take are here
        // already, measured, because they cost nothing and are what that unit needs first.
        (
            "SELECT 'r', uuid_generate_v3(uuid_ns_dns(), 'www.postgresql.org')",
            "uuid_generate_v3 is MD5 over a namespace and a name; MD5 is a unit of its own",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', uuid_generate_v5(uuid_ns_dns(), 'www.postgresql.org')",
            "uuid_generate_v5 is SHA-1 over a namespace and a name; SHA-1 is a unit of its own",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', uuid_generate_v3(uuid_ns_dns())",
            "the same gap, reached through the arity error: both refuse and only the code differs",
            "UNMEASURED",
        ),
    ],
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
