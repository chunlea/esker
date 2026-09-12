//! **A set operation over two enums is refused, and named** — `debts-v1.1.md` #57's last reader.
//!
//! `exec::query::append` unified every arm's declared type and then dropped the user-defined one
//! where the arms disagreed:
//!
//! ```rust,ignore
//! // And a user-defined type only where both arms are the same one.
//! if columns[at].user_type != column.user_type {
//!     columns[at].user_type = None;
//! }
//! ```
//!
//! Two enums are both `int2` in the row, so dropping the identity left a set of `smallint` whose
//! values are ordinals — a **wrong answer**, and the same trap `reconcile_enum`'s "both are" arm
//! had one clause over. Measured on 19beta1 in a `BEGIN … ROLLBACK`, with `mood` =
//! `('sad','ok','happy')` and `other_mood` = `('sad','ok')`:
//!
//! ```text
//! SELECT m FROM t UNION SELECT n FROM t        42846 UNION could not convert type other_mood
//!                                                    to mood
//! SELECT n FROM t UNION SELECT m FROM t        42846 UNION could not convert type mood to
//!                                                    other_mood      <- the arms' own order
//! SELECT m FROM t UNION SELECT 'sad'::text     42804 UNION types mood and text cannot be matched
//! SELECT m FROM t UNION SELECT 1::integer      42804 UNION types mood and integer cannot be matched
//! SELECT m FROM t UNION SELECT 1::smallint     42804 UNION types mood and smallint cannot be
//!                                                    matched          <- the storage is not the type
//! ```
//!
//! The last one is the one a node that unified *representations* answers rows for, and this one
//! did.
//!
//! **Two sentences and two codes, and which one you get is the category.** Two enums are one
//! category (`E`), so a real server tries the conversion and says `could not convert`; an enum
//! beside a `text` or an `integer` is two categories and never gets that far. It is the same pair
//! `money`/`numeric` already draws in `append`'s comments — this is that rule with the identity
//! `ColumnType` cannot spell.
//!
//! **The operator names itself in all three sentences, and this node cannot show it.**
//! `INTERSECT could not convert`, `EXCEPT types … cannot be matched` and `each INTERSECT query
//! must have the same number of columns` are all measured on 19beta1 —
//! `SqlError::SetOperationArity`'s doc comment asserted the opposite ("the sentence a real server
//! gives whichever operator is written"), a claim with no test under it. But `INTERSECT` and
//! `EXCEPT` are `0A000 … is not supported` here (`exec::set_arm_supported`), so no statement this
//! node can plan reaches those sentences with anything but `UNION` in it. The comments say what
//! was measured; the word is left hard-coded rather than threaded through a path nothing can
//! exercise, and the `0A000` is pinned below so the claim has a test under it at last.
//!
//! **Two gaps this found and does not fix.** The first is **#76**: a set operation inside a
//! derived table or a `WITH` never reaches `Executor::resolve_user_cast`, so its arms' casts are
//! not folded and the set ships ordinals under `smallint` — the same statement at the top level is
//! right. It is also what keeps the enum-beside-another-type half of this file's rule unwritten,
//! because while an arm's type can be missing for that reason, a missing one cannot be read as
//! "not an enum". The second is **#75**: an **unknown literal arm never takes the other
//! arm's type**. `tests/captures/pg19_set_operations.txt`'s header lists four rules for that and
//! three of them are not implemented — `SELECT 1 UNION ALL SELECT 'abc'` is `22P02 invalid input
//! syntax for type integer: "abc"` there and `42804 UNION types integer and text cannot be
//! matched` here, and so are `SELECT 1 UNION ALL SELECT NULL` and `SELECT m FROM t UNION SELECT
//! 'sad'`, which both *answer* on a real server. Nothing pinned the capture against the node. The
//! divergences are written down below as this node answers them, so the day they close a test
//! says so.
//!
//! **What must not move**: the same enum on both sides answers, an unquoted literal takes the
//! enum and comes back as a **label**, and a bare `NULL` arm is still a row.
//!
//! **The oracle capture is `tests/captures/pg19_set_operation_enums.txt`** — two
//! `BEGIN … ROLLBACK` sessions with a `SAVEPOINT` per probe, taken 2026-09-11, holding every
//! sentence this file asserts and every one it records as a divergence.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Frontend, Target};
use esker_sql::pgwire::session::Session;

const FIXTURE: &[&str] = &[
    "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
    "CREATE TYPE other_mood AS ENUM ('sad', 'ok')",
    "CREATE TABLE t (id bigint primary key, m mood, n other_mood)",
    "INSERT INTO t VALUES (1, 'ok', 'sad')",
];

/// One statement's answer with the `DETAIL` and `HINT` cut off — they are the same sentences for
/// every refusal here and what is being asserted is the type names.
fn said(node: &mut parity::Node, sql: &str) -> String {
    let answer = node.answer(sql).to_string();
    answer
        .split_once(" DETAIL:")
        .or_else(|| answer.split_once(" HINT:"))
        .map_or(answer.clone(), |(head, _)| head.to_owned())
}

/// The same, through `Parse`/`Describe`/`Bind`/`Execute` with one parameter — a set operation is
/// planned on both roads and only one of them is the fold.
fn said_bound(sql: &str, value: &str) -> String {
    let mut node = parity::Node::new(FIXTURE);
    let mut session = Session::new();
    for message in [
        Frontend::Parse {
            statement: "s".to_owned(),
            sql: sql.to_owned(),
            param_types: Vec::new(),
        },
        Frontend::Describe {
            target: Target::Statement,
            name: "s".to_owned(),
        },
        Frontend::Bind {
            portal: "p".to_owned(),
            statement: "s".to_owned(),
            param_formats: Vec::new(),
            params: vec![Some(value.as_bytes().to_vec())],
            result_formats: Vec::new(),
        },
        Frontend::Execute {
            portal: "p".to_owned(),
            max_rows: 0,
        },
    ] {
        let mut out = Vec::new();
        session.handle(&message, &mut node.executor, &mut out);
        let text = String::from_utf8_lossy(&out);
        let parts: Vec<&str> = text.split('\u{0}').collect();
        if parts
            .iter()
            .any(|part| *part == "SERROR" || *part == "VERROR")
        {
            let code = parts
                .iter()
                .find(|part| part.starts_with('C') && part.len() == 6)
                .map_or("?????", |part| &part[1..]);
            let message = parts
                .iter()
                .find(|part| part.starts_with('M'))
                .map_or("", |part| &part[1..]);
            let refusal = format!("!{code} {message}");
            return refusal
                .split_once(" DETAIL:")
                .or_else(|| refusal.split_once(" HINT:"))
                .map_or(refusal.clone(), |(head, _)| head.to_owned());
        }
    }
    "answered".to_owned()
}

/// **Two enums in one set operation cannot be converted**, and the sentence names both.
#[test]
fn two_enums_in_one_set_operation_are_refused() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION SELECT n FROM t"),
        "!42846 UNION could not convert type other_mood to mood",
        "the later arm is the one that could not be converted"
    );
    assert_eq!(
        said(&mut node, "SELECT n FROM t UNION SELECT m FROM t"),
        "!42846 UNION could not convert type mood to other_mood",
        "and the other order is the other sentence"
    );
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION ALL SELECT n FROM t"),
        "!42846 UNION could not convert type other_mood to mood",
        "ALL changes the duplicates and not the typing"
    );
    // A cast in an arm is the same fact, and so is a bound parameter under one.
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION SELECT 'sad'::other_mood"),
        "!42846 UNION could not convert type other_mood to mood"
    );
    assert_eq!(
        said_bound("SELECT m FROM t UNION SELECT $1::other_mood", "sad"),
        "!42846 UNION could not convert type other_mood to mood",
        "the same statement as a driver sends it"
    );
}

/// **Why the operator word cannot be shown to be wrong yet.**
///
/// All three of this path's sentences name the operator on 19beta1 — measured 2026-09-11 —
/// and this crate writes `UNION` into all three. No statement reaches them with another word,
/// because the other two operators are refused before they are planned. Pinned here so that the
/// day `INTERSECT` is built, the test that stops passing says which sentences to re-measure.
#[test]
fn the_other_two_operators_are_refused_before_a_sentence_can_name_them() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        said(&mut node, "SELECT m FROM t INTERSECT SELECT n FROM t"),
        "!0A000 INTERSECT is not supported",
        "19beta1: 42846 INTERSECT could not convert type other_mood to mood"
    );
    assert_eq!(
        said(&mut node, "SELECT m FROM t EXCEPT SELECT n FROM t"),
        "!0A000 EXCEPT is not supported",
        "19beta1: 42846 EXCEPT could not convert type other_mood to mood"
    );
}

/// **An enum beside another category is refused, and the sentence names the enum** —
/// `debts-v1.1.md` **#57**'s remaining half, which #76 unblocked.
///
/// Two categories, so a real server never tries the conversion and never reaches the `42846` two
/// enums get. What stopped this being written was that an **absent `user_type` was ambiguous**:
/// `resolve_user_cast` reached the top-level select's arms and no further, so a set operation one
/// clause down arrived carrying no type at all, and refusing on the asymmetry turned statements
/// that answer into `42804`s — b4 tried it and the suite caught it. Since #76 an absent type
/// means *not a user type*, and this arm is the one line the code's own comment promised.
///
/// **The last two mattered most**: an enum is an `int2` in the row, so a node unifying
/// *representations* answered **rows** for them — `1 ; 2`, the ordinals, under `smallint`.
///
/// Measured on 19beta1 2026-09-11, one `BEGIN … ROLLBACK` with a savepoint per statement:
///
/// ```text
/// SELECT m FROM t UNION SELECT 'sad'::text     UNION types mood and text cannot be matched
/// SELECT 'sad'::text UNION SELECT m FROM t     UNION types text and mood cannot be matched
/// SELECT m FROM t UNION SELECT 1::integer      UNION types mood and integer cannot be matched
/// SELECT m FROM t UNION SELECT 1::smallint     UNION types mood and smallint cannot be matched
/// ```
#[test]
fn an_enum_beside_another_category_is_refused_and_the_enum_is_named() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION SELECT 'sad'::text"),
        "!42804 UNION types mood and text cannot be matched"
    );
    assert_eq!(
        said(&mut node, "SELECT 'sad'::text UNION SELECT m FROM t"),
        "!42804 UNION types text and mood cannot be matched",
        "the names are in the arms' order"
    );
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION SELECT 1::integer"),
        "!42804 UNION types mood and integer cannot be matched",
        "it answered the ordinals under `integer` before this"
    );
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION SELECT 1::smallint"),
        "!42804 UNION types mood and smallint cannot be matched",
        "and under `smallint`, which is the enum's own storage — the hardest of the four"
    );
    // **One clause down, which is what #76 bought.** The arm reaches the rule with its type, so
    // the refusal is the same sentence wherever the set is written.
    assert_eq!(
        said(
            &mut node,
            "SELECT v FROM (SELECT m AS v FROM t) q UNION SELECT 1::smallint"
        ),
        "!42804 UNION types mood and smallint cannot be matched",
        "19beta1 writes the same sentence for this one"
    );
}

/// **What an enum-only rule must not touch**, all of it measured in the same capture.
///
/// A **domain** is its base type for this purpose — `SELECT d FROM t UNION SELECT 2::integer`
/// answers on 19beta1 and `pg_typeof` is `integer`, and beside a `text` the refusal names
/// `integer` and not the domain. A **range** is its own `ColumnType` and was already named right.
/// Neither is an enum, so neither reaches the rule above.
#[test]
fn a_domain_and_a_range_keep_the_behaviour_they_had() {
    let mut node = parity::Node::new(&[
        "CREATE DOMAIN posint AS integer CHECK (VALUE > 0)",
        "CREATE TABLE dr (d posint, r int4range)",
        "INSERT INTO dr VALUES (1, '[1,5)')",
    ]);
    assert_eq!(
        node.rows("SELECT d FROM dr UNION SELECT 2::integer ORDER BY 1"),
        vec![vec!["1"], vec!["2"]],
        "a domain unifies with its base type, which an enum never does"
    );
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(x) FROM (SELECT d AS x FROM dr UNION SELECT 2::integer) q LIMIT 1"
        ),
        vec![vec!["integer"]]
    );
    assert_eq!(
        said(&mut node, "SELECT d FROM dr UNION SELECT 'a'::text"),
        "!42804 UNION types integer and text cannot be matched",
        "the domain is named by its base, which is 19beta1's own sentence"
    );
    assert_eq!(
        node.rows("SELECT r FROM dr UNION SELECT '[2,3)'::int4range ORDER BY 1"),
        vec![vec!["[1,5)"], vec!["[2,3)"]]
    );
    assert_eq!(
        said(&mut node, "SELECT r FROM dr UNION SELECT 'a'::text"),
        "!42804 UNION types int4range and text cannot be matched"
    );
}

/// **And an enum that arrives through something other than a column or a cast still answers.**
///
/// `max(m)` keeps its argument's type (`output_columns` reads that case), so the aggregate's
/// column carries the enum and the two sides are the same one. 19beta1 answers `sad` then `ok`;
/// a rule that read "no `user_type`" as "not an enum" without this case would refuse it.
#[test]
fn an_aggregate_of_an_enum_is_still_that_enum() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT max(m) FROM t UNION SELECT 'sad'::mood ORDER BY 1"),
        vec![vec!["sad"], vec!["ok"]]
    );
}

/// **A set operation inside a derived table loses its arms' casts** — `debts-v1.1.md` **#76**,
/// the gap the rule above waits on.
///
/// `Executor::resolve_user_cast` builds its list of projections from the statement's own select
/// and its `set_arms`, and no further — so a set operation one clause down, in a derived table or
/// in a `WITH`, is not in it. The arm's `'sad'::mood` is never folded against the catalog, the
/// column it produces carries no type, and the set is declared `smallint` and sends **ordinals**
/// where 19beta1 sends labels. It is #57 (a)'s walker shape one level further out, and the same
/// statement **at the top level answers correctly**, which is what says it is the nesting and not
/// the cast.
///
/// **Re-measured on 19beta1 2026-09-11 for this fix**, in one `BEGIN … ROLLBACK` with a savepoint
/// per statement so that no refusal swallows the rest — every expectation below is that server's:
///
/// ```text
/// SELECT m FROM t UNION SELECT 'sad'::mood ORDER BY 1                      sad ; ok
/// SELECT v FROM (SELECT m AS v FROM t UNION SELECT 'sad'::mood) s …        sad ; ok
/// WITH c AS (… UNION SELECT 'sad'::mood) SELECT v FROM c …                 sad ; ok
/// SELECT pg_typeof(v) FROM (… UNION SELECT 'sad'::mood) s LIMIT 1          mood
/// WITH c AS (… UNION SELECT 'sad'::mood) SELECT pg_typeof(v) FROM c …      mood
/// ```
///
/// Red before the fix at `1 ; 2` — the ordinals, which is the set declared `smallint`.
///
/// **`pg_typeof` is asserted and not only the rows**, because the rows alone pass for a node that
/// prints a label it does not know the type of: the mechanism is that the arm carries an identity
/// through `query::append`, and the type is where that shows.
#[test]
fn a_nested_set_operation_loses_its_arms_user_type() {
    let mut node = parity::Node::new(FIXTURE);
    // The control, and the reason this is the nesting: the same statement one clause up is right.
    assert_eq!(
        node.rows("SELECT m FROM t UNION SELECT 'sad'::mood ORDER BY 1"),
        vec![vec!["sad"], vec!["ok"]],
        "at the top level the labels come back, which is what `resolve_user_cast` reaches"
    );
    assert_eq!(
        node.rows("SELECT v FROM (SELECT m AS v FROM t UNION SELECT 'sad'::mood) s ORDER BY 1"),
        vec![vec!["sad"], vec!["ok"]],
        "a derived table one clause down: 19beta1 answers the labels"
    );
    assert_eq!(
        node.rows(
            "WITH c AS (SELECT m AS v FROM t UNION SELECT 'sad'::mood) SELECT v FROM c ORDER BY 1"
        ),
        vec![vec!["sad"], vec!["ok"]],
        "and a `WITH` is the same relation by another name"
    );
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(v) FROM (SELECT m AS v FROM t UNION SELECT 'sad'::mood) s LIMIT 1"
        ),
        vec![vec!["mood"]],
        "the column is the enum, not the storage it shares with every other enum"
    );
    assert_eq!(
        node.rows(
            "WITH c AS (SELECT m AS v FROM t UNION SELECT 'sad'::mood) \
             SELECT pg_typeof(v) FROM c LIMIT 1"
        ),
        vec![vec!["mood"]],
        "and through a `WITH`"
    );
}

/// **A set operation two clauses down, and one inside an expression subquery** — the same walk,
/// where a single level of recursion would not have reached.
///
/// Measured on 19beta1 in the same capture: both answer `sad` then `ok`.
#[test]
fn a_set_operation_reached_through_two_clauses_keeps_its_type() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(
            "SELECT v FROM (SELECT v FROM (SELECT m AS v FROM t UNION SELECT 'sad'::mood) a) b \
             ORDER BY 1"
        ),
        vec![vec!["sad"], vec!["ok"]],
        "a derived table inside a derived table"
    );
    assert_eq!(
        node.rows(
            "WITH c AS (SELECT v FROM (SELECT m AS v FROM t UNION SELECT 'sad'::mood) a) \
             SELECT v FROM c ORDER BY 1"
        ),
        vec![vec!["sad"], vec!["ok"]],
        "and a `WITH` over a derived table over the set"
    );
}

/// **What must not move.**
#[test]
fn one_enum_across_the_arms_still_answers() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT m FROM t UNION SELECT m FROM t"),
        vec![vec!["ok"]],
        "one enum on both sides, and the label rather than the ordinal"
    );
    assert_eq!(
        node.rows("SELECT m FROM t UNION SELECT 'sad'::mood ORDER BY 1"),
        vec![vec!["sad"], vec!["ok"]],
        "a cast to the same enum, in the ordinals' order"
    );
    // **Two declared divergences, and neither is the enum's.** An unknown literal arm never takes
    // the other arm's type here, whatever the other arm is: `SELECT 1 UNION ALL SELECT 'abc'` is
    // `22P02 invalid input syntax for type integer: "abc"` on 19beta1 and `SELECT 1 UNION ALL
    // SELECT NULL` is an `integer` there — both are this `42804`. The capture's own header
    // (`pg19_set_operations.txt`) lists four rules for unknown arms and three of them are not
    // built; nothing pinned it against the node until this. Written as this node answers them so
    // the day they close, a test says so.
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION SELECT 'sad' ORDER BY 1"),
        "!42804 UNION types mood and text cannot be matched",
        "19beta1 answers two rows, `sad` then `ok`: an unknown literal takes the enum (#81). \
         Since #57 the refusal at least names the enum rather than the `int2` it is stored as"
    );
    assert_eq!(
        said(&mut node, "SELECT m FROM t UNION SELECT NULL ORDER BY 1"),
        "!42804 UNION types mood and text cannot be matched",
        "19beta1 answers two rows: a bare NULL takes the other arm's type too (#81)"
    );
}

/// **And the sentences that had no enum in them are unchanged.**
#[test]
fn the_ordinary_set_operation_refusals_are_what_they_were() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE u (t text, i integer)",
        "INSERT INTO u VALUES ('a', 1)",
    ]);
    assert_eq!(
        said(&mut node, "SELECT t FROM u UNION SELECT i FROM u"),
        "!42804 UNION types text and integer cannot be matched"
    );
    assert_eq!(
        said(&mut node, "SELECT 1, 2 UNION ALL SELECT 3"),
        "!42601 each UNION query must have the same number of columns"
    );
    // **The unknown-arm divergence closed** — `debts-v1.1.md` #75, and this assertion is what
    // said so: it was written as this node's `42804` with 19beta1's answer beside it, and the fix
    // made the two the same sentence. An unknown literal takes the other arm's type and then
    // fails to read as it, so the complaint is about the **value**.
    assert_eq!(
        said(&mut node, "SELECT 1 UNION ALL SELECT 'abc'"),
        "!22P02 invalid input syntax for type integer: \"abc\"",
        "19beta1's own sentence, since #75"
    );
    assert_eq!(
        said(&mut node, "SELECT 'lit' UNION ALL SELECT t FROM u"),
        "text\tlit ; a",
        "the one unknown-arm rule that does work: both arms are text anyway"
    );
}
