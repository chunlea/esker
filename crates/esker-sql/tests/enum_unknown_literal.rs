//! **An unknown literal beside an enum takes the enum** — debt #81, the half #75 left, held to
//! PostgreSQL 19's answers.
//!
//! `select_common_type` is one rule with four constructs over it: a set operation's arms, a
//! `CASE`'s results, a `COALESCE`'s arguments, and `GREATEST`/`LEAST`. Each resolves the branches
//! that *have* a type and then reads each `unknown` as that type — and an enum is not the
//! exception this node made it. `SELECT m FROM t UNION SELECT 'sad'` was
//! `42804 UNION types mood and text cannot be matched` where a real server answers two rows, and
//! `CASE WHEN false THEN m ELSE 'sad' END` was `22P02 invalid input syntax for type smallint` —
//! the enum's **storage**, which is the one thing a client must never be told about an enum
//! (ADR 0050).
//!
//! The same measurement found a second thing: none of the three expression constructs carried the
//! enum at all, so `CASE WHEN true THEN m ELSE m END` answered `2` — the ordinal — where 19beta1
//! answers `ok`. Both are this file's subject, because they are one mechanism: the identity the
//! storage type cannot hold has to travel with the branches.
//!
//! `corpus/pg19_enum_unknown_literal.txt` is the capture and the replay is the test of record; the
//! tests after it pin one construct each, with the guards on what must not move.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
    "CREATE TYPE other_mood AS ENUM ('sad', 'ok')",
    "CREATE TABLE t (id bigint primary key, m mood, n other_mood)",
    "INSERT INTO t VALUES (1, 'ok', 'sad')",
];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **Two entries left here on 2026-09-16, and the prose that explained them left with them.**
    // They declared `SELECT 'r', m FROM h_t WHERE id = 1 EXCEPT SELECT 'r', 'sad'` and the
    // `INTERSECT` beside it as divergences, on the ground that "`UNION` is the only set operator
    // this node implements" and that the word in `SetOperationTypes` was hard-coded for that
    // reason. #105 implemented both operators and threaded the word through
    // `Unifying::SetOperation`, so neither statement diverges any longer — and the second entry's
    // own promise, "listed separately so the ratchet can say when either one stops needing it",
    // is what says to delete them rather than leave them declared and green.
    //
    // The two corpus rows they covered (`pg19_enum_unknown_literal.txt:43` and `:46`) are enforced
    // from now on; 19beta1 answers `r|ok` to both, which is what this node must answer too.
    answers: &[],
};

/// One statement's answer with the `DETAIL` and `HINT` cut off — the type names are what is being
/// asserted, and those sentences are the same for every refusal here.
fn said(node: &mut parity::Node, sql: &str) -> String {
    let answer = node.answer(sql).to_string();
    answer
        .split_once(" DETAIL:")
        .or_else(|| answer.split_once(" HINT:"))
        .map_or(answer.clone(), |(head, _)| head.to_owned())
}

#[test]
fn every_enum_unknown_literal_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_enum_unknown_literal.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 70,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **A set operation's unknown arm takes the other arm's enum**, and comes back as a label.
#[test]
fn a_set_operations_unknown_arm_takes_the_enum() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT m FROM t UNION SELECT 'sad' ORDER BY 1"),
        [["sad"], ["ok"]],
        "the labels, in the enum's own order"
    );
    assert_eq!(
        node.rows("SELECT m FROM t UNION ALL SELECT 'sad'"),
        [["ok"], ["sad"]]
    );
    assert_eq!(
        node.rows("SELECT m FROM t UNION SELECT NULL ORDER BY 1"),
        [["ok"], ["\\N"]],
        "a bare NULL arm is a NULL of that enum, and still a row"
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(v) FROM (SELECT m AS v FROM t UNION SELECT 'sad') q LIMIT 1"),
        [["mood"]],
        "the set is the enum, not the int2 its ordinal is stored in"
    );
}

/// **A `CASE` and a `COALESCE` over an enum are that enum**, whether the other branch is a column,
/// a cast, or an unknown literal.
#[test]
fn a_case_and_a_coalesce_over_an_enum_are_that_enum() {
    let mut node = parity::Node::new(FIXTURE);
    for (sql, answer) in [
        ("SELECT CASE WHEN true THEN m ELSE 'sad' END FROM t", "ok"),
        ("SELECT CASE WHEN false THEN m ELSE 'sad' END FROM t", "sad"),
        ("SELECT CASE WHEN true THEN m ELSE m END FROM t", "ok"),
        (
            "SELECT CASE WHEN true THEN m ELSE 'sad'::mood END FROM t",
            "ok",
        ),
        ("SELECT COALESCE(m, 'sad') FROM t", "ok"),
        ("SELECT COALESCE(m, m) FROM t", "ok"),
        ("SELECT COALESCE(NULL::mood, 'sad')", "sad"),
    ] {
        assert_eq!(node.rows(sql), [[answer]], "{sql}");
    }
    assert_eq!(
        node.rows("SELECT pg_typeof(CASE WHEN false THEN m ELSE 'sad' END) FROM t"),
        [["mood"]],
        "it answered `smallint` before, which is the ordinal's type and never the enum's"
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(COALESCE(m, 'sad')) FROM t"),
        [["mood"]]
    );
    // A NULL branch leaves the enum alone, and the value is still NULL.
    assert_eq!(
        node.rows("SELECT CASE WHEN false THEN m ELSE NULL END FROM t"),
        [["\\N"]]
    );
}

/// **`GREATEST` and `LEAST` are the same rule's third construct** — they pick one of their
/// arguments, so an enum stays an enum and an unknown argument is one of its labels.
#[test]
fn greatest_and_least_over_an_enum_are_that_enum() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT GREATEST(m, 'sad'), LEAST(m, 'happy') FROM t"),
        [["ok", "ok"]],
        "the labels, compared in declaration order"
    );
    assert_eq!(node.rows("SELECT GREATEST(m, m) FROM t"), [["ok"]]);
    assert_eq!(
        node.rows("SELECT GREATEST(m, NULL), LEAST(m, NULL) FROM t"),
        [["ok", "ok"]],
        "neither is strict: a NULL argument is skipped"
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(GREATEST(m, 'sad')) FROM t"),
        [["mood"]]
    );
}

/// **A literal that is no label of that enum fails as that enum** — the same sentence an `INSERT`
/// of it gives, because the literal is read as the enum rather than compared with it.
#[test]
fn a_literal_that_is_no_label_fails_as_the_enum() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "SELECT m FROM t UNION SELECT 'nope'",
        "SELECT CASE WHEN false THEN m ELSE 'nope' END FROM t",
        "SELECT COALESCE(m, 'nope') FROM t",
        "SELECT GREATEST(m, 'nope') FROM t",
        "SELECT LEAST(m, 'nope') FROM t",
    ] {
        assert_eq!(
            said(&mut node, sql),
            "!22P02 invalid input value for enum mood: \"nope\"",
            "{sql}"
        );
    }
}

/// **Two enums are one category and no conversion**, and each construct says its own word.
#[test]
fn two_enums_cannot_be_converted_and_the_construct_says_so() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        said(&mut node, "SELECT CASE WHEN true THEN m ELSE n END FROM t"),
        "!42846 CASE/WHEN could not convert type mood to other_mood",
        "a CASE's list starts at its ELSE, so the ELSE is the type and the THEN is the branch"
    );
    assert_eq!(
        said(&mut node, "SELECT COALESCE(m, n) FROM t"),
        "!42846 COALESCE could not convert type other_mood to mood",
        "a COALESCE's list is left to right"
    );
    assert_eq!(
        said(&mut node, "SELECT LEAST(m, n) FROM t"),
        "!42846 LEAST could not convert type other_mood to mood"
    );
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION SELECT n FROM t"),
        "!42846 UNION could not convert type other_mood to mood",
        "the set operation's own sentence, unchanged"
    );
}

/// **An enum beside another category is `42804`**, named by the enum and not by its storage.
#[test]
fn an_enum_beside_another_category_is_refused_by_name() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        said(
            &mut node,
            "SELECT CASE WHEN true THEN m ELSE 'sad'::text END FROM t"
        ),
        "!42804 CASE types text and mood cannot be matched"
    );
    assert_eq!(
        said(&mut node, "SELECT COALESCE(m, 'sad'::text) FROM t"),
        "!42804 COALESCE types mood and text cannot be matched"
    );
    assert_eq!(
        said(
            &mut node,
            "SELECT CASE WHEN true THEN m ELSE 1::integer END FROM t"
        ),
        "!42804 CASE types integer and mood cannot be matched"
    );
    assert_eq!(
        said(&mut node, "SELECT COALESCE(m, 1::smallint) FROM t"),
        "!42804 COALESCE types mood and smallint cannot be matched",
        "the storage an enum shares with a smallint is not the type"
    );
    assert_eq!(
        said(&mut node, "SELECT LEAST(m, 1::integer) FROM t"),
        "!42804 LEAST types mood and integer cannot be matched"
    );
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION SELECT 'sad'::text"),
        "!42804 UNION types mood and text cannot be matched",
        "the set operation's own sentence, unchanged"
    );
}

/// **A domain is not an enum, and the measurement says where each one is kept.**
///
/// The guard on the arm this row's first patch got wrong. Measured on 19beta1
/// (`esker-coord/s2-h81d.out`): a domain column is its domain, `min` and `max` of it are the
/// **base** — the aggregate resolves to the base type's operator family — and `array_agg` keeps it.
/// A branch construct keeps the domain when every side is that domain and settles on the base when
/// one side is not, which is the same shape the enum rule has and the reason a blanket "composites
/// carry no domain" was wrong.
#[test]
fn a_domain_is_kept_where_postgresql_19_keeps_it() {
    let mut node = parity::Node::new(&[
        "CREATE DOMAIN d AS text",
        "CREATE TABLE dt (id int, s d, t text)",
        "INSERT INTO dt VALUES (1, 'a', 'b')",
    ]);
    // **`array_agg` is asserted here now, and it is `debts-v1.1.md` #106.** It was measured by this
    // guard and left unasserted while it had no number: 19beta1 answers `d[]` for
    // `pg_typeof(array_agg(s))` — an array *of the domain* — where this node answers `text[]`, an
    // array of its base. The pair around it is what makes it a rule rather than a coincidence:
    // `min` and `max` of the same column resolve to the **base**, because the aggregate is
    // resolved to the base type's operator family, while `array_agg` keeps the domain. So the two
    // cannot share an answer, and a fix that made them agree would break the two lines above.
    for (sql, answer) in [
        ("SELECT pg_typeof(s) FROM dt", "d"),
        ("SELECT pg_typeof(min(s)) FROM dt", "text"),
        ("SELECT pg_typeof(max(s)) FROM dt", "text"),
        ("SELECT pg_typeof(array_agg(s)) FROM dt", "d[]"),
        (
            "SELECT pg_typeof(CASE WHEN true THEN s ELSE s END) FROM dt",
            "d",
        ),
        (
            "SELECT pg_typeof(CASE WHEN true THEN s ELSE t END) FROM dt",
            "text",
        ),
        ("SELECT pg_typeof(COALESCE(s, s)) FROM dt", "d"),
        ("SELECT pg_typeof(COALESCE(s, t)) FROM dt", "text"),
        ("SELECT pg_typeof(GREATEST(s, s)) FROM dt", "d"),
    ] {
        assert_eq!(node.rows(sql), [[answer]], "{sql}");
    }
}

/// **What must not move**: the constructs over ordinary types keep the rules #75 gave them.
#[test]
fn the_rules_for_ordinary_types_are_what_they_were() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE u (t text, i integer)",
        "INSERT INTO u VALUES ('a', 1)",
    ]);
    assert_eq!(
        said(&mut node, "SELECT 1 UNION ALL SELECT 'abc'"),
        "!22P02 invalid input syntax for type integer: \"abc\"",
        "an unknown arm beside an integer still takes the integer and fails as one"
    );
    assert_eq!(
        node.rows("SELECT 'lit' UNION ALL SELECT t FROM u"),
        [["lit"], ["a"]]
    );
    assert_eq!(
        said(&mut node, "SELECT t FROM u UNION SELECT i FROM u"),
        "!42804 UNION types text and integer cannot be matched"
    );
    assert_eq!(
        said(&mut node, "SELECT COALESCE(1, 'x'::text)"),
        "!42804 COALESCE types integer and text cannot be matched"
    );
    assert_eq!(
        node.rows("SELECT COALESCE(i, 0) FROM u"),
        [["1"]],
        "and an unknown-free COALESCE is untouched"
    );
    assert_eq!(
        node.rows("SELECT GREATEST(i, 2) FROM u"),
        [["2"]],
        "GREATEST over two integers keeps the promotion it had"
    );
}
