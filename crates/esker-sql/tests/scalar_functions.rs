//! The scalar functions `CREATE TABLE "defaults"` calls — statement 738 of the specific schema.
//!
//! They were accepted before only by the `DEFAULT` whitelist and were not functions: `SELECT now()`
//! was `0A000`. This is them as ordinary expressions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a constant expression.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT length(concat('a', NULL, 'b'))",
            "`length` is a different function and not one statement 738 calls. The line is in the corpus because it is what pins `concat`'s NULL rule as a *length* — `concat('a', NULL, 'b')` is two characters, not four and not NULL — and that half agrees: the string this node builds is the string PostgreSQL builds.",
        ),
        (
            "SELECT length(convert_to('Ruby on Rails', 'UTF8'))",
            "`length` is a different function and not one statement 738 calls. The line is in the corpus because it is what pins `concat`'s NULL rule as a *length* — `concat('a', NULL, 'b')` is two characters, not four and not NULL — and that half agrees: the string this node builds is the string PostgreSQL builds.",
        ),
        (
            "SELECT convert_to('A', 'LATIN1')",
            "**A real encoding this node cannot transcode to.** `LATIN1` is in PostgreSQL's list, so the answer is a refusal naming it and not the `22023` an unknown name gets — the two are told apart deliberately (`crate::value::encoding`). Returning the UTF-8 bytes under another encoding's name would be a wrong answer wearing a right one's label: `é` is `\\xc3a9` in UTF-8 and `\\xe9` in LATIN1, and only one of them is what was asked for.",
        ),
        (
            "SELECT convert_to('é', 'LATIN1')",
            "**A real encoding this node cannot transcode to.** `LATIN1` is in PostgreSQL's list, so the answer is a refusal naming it and not the `22023` an unknown name gets — the two are told apart deliberately (`crate::value::encoding`). Returning the UTF-8 bytes under another encoding's name would be a wrong answer wearing a right one's label: `é` is `\\xc3a9` in UTF-8 and `\\xe9` in LATIN1, and only one of them is what was asked for.",
        ),
        (
            "SELECT CURRENT_DATE = now()::date",
            "A cast from `timestamptz` to `date`, which this node does not have. `CURRENT_DATE` itself answers and is that instant's date — the line beside it (`CURRENT_DATE` is never NULL) and the type assertion in this file cover what this one would.",
        ),
        (
            "SELECT CURRENT_TIMESTAMP(0) = date_trunc('second', CURRENT_TIMESTAMP)",
            "`CURRENT_TIMESTAMP(p)` and `date_trunc` are two more members of this family and neither is called by statement 738. They are in the corpus so that the day one is needed its answer is already measured; both are refused by name today. `LOCALTIMESTAMP` was in this list until `insert_all` needed it (`tests/values_catalog_function.rs`), which is what the list is for.",
        ),
        (
            "SELECT CURRENT_TIME IS NULL",
            "**`time with time zone` is not one of the stored types** (ADR 0033), so `CURRENT_TIME` is refused by name rather than answered — the one member of this family whose absence is a *type* and not a function. Its unzoned twin `LOCALTIME` is implemented; `tests/values_catalog_function.rs` carries the pair and the `42804` that tells them apart.",
        ),
        (
            "SELECT now() AT TIME ZONE 'UTC' IS NULL",
            "`AT TIME ZONE` is an operator on a timestamp, not a function, and this node has none. The line pins that `now()` is not NULL, which the line above it pins without the operator.",
        ),
    ],
};

#[test]
fn every_scalar_function_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_scalar_functions.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The **declared types**, which a corpus of comparisons cannot pin.
///
/// Every statement in the corpus compares these values or asks whether they are NULL, because an
/// instant is not reproducible and a corpus is replayed. The type each function answers is
/// reproducible, and it is what a client is told before a single row arrives — so it is asserted
/// here, against the OIDs `pg_typeof` reports on a real server: `timestamptz`, `date`, `text`,
/// `bytea`.
#[test]
fn each_function_declares_the_type_postgresql_declares() {
    const TIMESTAMPTZ: u32 = 1184;
    const DATE: u32 = 1082;
    const TEXT: u32 = 25;
    const BYTEA: u32 = 17;

    let mut node = parity::Node::new(&[]);
    let outcome = node
        .run("SELECT now(), CURRENT_TIMESTAMP, CURRENT_DATE, concat('a'), convert_to('a', 'UTF8')")
        .unwrap();
    let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
        panic!("a SELECT answered no rows at all");
    };
    let oids: Vec<u32> = fields.iter().map(|field| field.type_oid).collect();
    assert_eq!(oids, vec![TIMESTAMPTZ, TIMESTAMPTZ, DATE, TEXT, BYTEA]);
}

/// `now()` is the **transaction's** instant, not the statement's.
///
/// Two calls in one transaction are equal — which the corpus shows within a single statement, and
/// this shows across two of them, where a statement-level clock would differ. It is also why the
/// value comes from the TSO rather than from a clock this node reads: `docs/DESIGN.md` §6.
#[test]
fn now_is_the_transactions_instant_and_not_the_statements() {
    let mut node = parity::Node::new(&[]);
    node.run("BEGIN").unwrap();
    let first = node.rows("SELECT now()");
    let second = node.rows("SELECT now()");
    node.run("COMMIT").unwrap();
    assert_eq!(first, second, "now() moved inside one transaction");

    // And a *new* transaction is free to be a later instant, so nothing here asserts it is not.
    assert!(!first.is_empty(), "now() answered no rows");
}
