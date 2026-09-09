//! **A bare literal compared against a `regclass` is an oid** — `debts-v1.1.md` #41, first half.
//!
//! `=` over a `regclass` is `oideq`, so an unadorned literal is handed to `oidin` and a *name* is
//! `22P02 invalid input syntax for type oid: "ra"` — not resolved. That is the rule
//! [ADR 0098](../../../docs/adr/0098-regproc-is-an-oid-that-prints-as-a-function.md) measured for
//! `regproc`, one type over, and the reason this half is two lines: **it needs no catalog at all.**
//!
//! The row it comes from claimed a bare name should reach a `regclass` target anywhere, and the
//! measurement split it: an *assignment* does resolve the name — `INSERT`, `UPDATE SET` and a
//! column `DEFAULT` all store the relation — and that half is the one that needs a catalog at a
//! point that has none. This file is the half that does not.
//!
//! Measured in `tests/captures/pg19_regclass_literal.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **`IN` was declared here and is closed.** It answers on a real server because a list is
    // coerced through the *type's* input function rather than the operator's operand type — so it
    // follows the *assignment* rule, and it closed with that half (`tests/regclass_name_at_runtime.rs`)
    // exactly as this entry said it would.
    answers: &[],
};

#[test]
fn every_regclass_literal_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_regclass_literal.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **The three shapes that read the literal as an oid**, and the two that answer.
#[test]
fn a_comparison_reads_the_literal_as_an_oid() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ra (id int8)",
        "CREATE TABLE rh (id int8, r regclass)",
    ]);
    node.run("INSERT INTO rh VALUES (1, 'ra'::regclass)")
        .unwrap();
    for statement in [
        "SELECT id FROM rh WHERE r = 'ra'",
        // The operand order does not change which side is `unknown`.
        "SELECT id FROM rh WHERE 'ra' = r",
        "SELECT id FROM rh WHERE r = ANY('{ra}')",
        // `<>` is the same operator family and the same reading.
        "SELECT id FROM rh WHERE r <> 'ra'",
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "{statement}"
        );
        assert_eq!(
            error.to_string(),
            "invalid input syntax for type oid: \"ra\"",
            "{statement}"
        );
    }
    // **The two spellings that answer**, and they are the ones a client writes: adorn the literal,
    // or compare the printed form.
    assert_eq!(
        node.rows("SELECT id FROM rh WHERE r = 'ra'::regclass"),
        vec![vec!["1"]]
    );
    assert_eq!(
        node.rows("SELECT id FROM rh WHERE r::text = 'ra'"),
        vec![vec!["1"]]
    );
    // **A literal of digits is an oid and simply compares** — no rows, not an error, which is what
    // says this arm reads the literal rather than refusing every string.
    assert_eq!(
        node.rows("SELECT id FROM rh WHERE r = '0'"),
        Vec::<Vec<String>>::new()
    );
}
