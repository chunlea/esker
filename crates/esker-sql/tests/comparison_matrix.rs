//! **Which pairs of types compare at all** — `debts-v1.1.md` #43's second mechanism, measured as a
//! matrix before any of it was touched.
//!
//! The row's named symptom is `1::oid = 1::int8` becoming `42883` once the constant fold stops
//! producing a literal for `reconcile` to retype. **It does not reproduce today**: with #42's
//! text-only fold both operands still fold, so `reconcile` retypes them and the comparison
//! answers. The defect is latent, waiting for the fold to narrow further — so measuring the row's
//! own example would have measured nothing.
//!
//! What this measures instead is the surface that mechanism lives in: every ordered pair of the 52
//! scalar types both sides have, compared **over columns** so the answer comes from the resolver
//! and not from the fold. 2,704 pairs, and 59 of them disagreed:
//!
//! ```text
//! 12  PG answers, node refuses   citext against the whole text family, and interval/time
//! 47  PG refuses, node ANSWERS   oid and reg* against numeric and the floats; the two vectors
//!                                against the text family; lquery against itself
//! ```
//!
//! **The worse direction was the larger one**, and none of it was in the row: it came of a flat
//! family tag being asked a question it cannot answer. `oid` compares with the integers and not
//! with `numeric` or the floats, and the integers, floats and `numeric` are one family because
//! they all compare with each other — no single tag expresses that overlap, so the pair is asked
//! before the tags now, the way `json`'s is.
//!
//! **The literal form closed the rest**, and it is the same 2,809 statements: a cast beside a
//! literal met neither the column path's family check nor the two-literal one, because
//! `reconcile` matched it first and *retyped* instead. That is the arm whose own comment says a
//! cast types the other side exactly as a column does — with the second half of the sentence
//! missing, which is `debts-v1.1.md` #43's second mechanism in its purest form and the fourth
//! time in this crate that a rule reached only the caller it was written for.
//!
//! Measured in `tests/captures/pg19_comparison_matrix.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// **Every measured pair, in the literal form.**
///
/// This is the path where `reconcile` retypes a literal, and it does not consult `same_family` at
/// all: it asks `Literal::comparable_with` → `Datum::fits` → `one_representation`. Three gates,
/// none of them the column path's, which is why the same build answered
/// `'x'::citext = 'x'::text` over columns and refused it over literals until `one_representation`
/// gained the pair — **in the direction the gate actually asks**, which is `citext` *held* against
/// a text-shaped target and not the reverse. The first attempt added the reverse, moved nothing,
/// and was reverted.
///
/// It went 59 → 37 → 27 → 0, and the last twenty-seven were **two** findings rather than the one
/// family the previous round named:
///
/// ```text
/// 26  PG refuses, node ANSWERS   a cast beside a typed literal was retyped, never family-checked
///  1  PG answers, node REFUSES   '[1,3)'::int8range refused itself, 42846 int4range -> int8range
/// ```
///
/// **The first is a pattern that matched too early.** Every type whose *value* cannot carry its
/// own name — `xml`, `jsonb`, `name`, `"char"`, `bit`, `int2vector`, `oidvector`, `lquery`, `void`
/// — keeps a `Cast` node out of the fold, so it reaches `reconcile` as a cast rather than as a
/// literal; the arm that matches a cast against a literal retypes it, which is right for an
/// `unknown` and is how `'x'::text = '<a/>'::xml` answered `f` where a real server says
/// `42883 operator does not exist: text = xml`. The previous round read the same rows as "an
/// untyped string escapes both gates" — a diagnosis from reading the two gates rather than from
/// asking which arm the pair actually took.
///
/// **The second was hidden behind the first**, and the node's own sentence is what named it: a
/// `Datum::Range` carries its subtype, an `int4range` and an `int8range` are both ranges of an
/// `int8` here, and `column_type` therefore answers a *representative*. The cast the fold keeps
/// over the constant then looked like one between two different types and refused a statement
/// whose operand is already exactly the type named. Folding the constant instead would answer
/// `int4range = int8range`, which a real server refuses — so the node stays and
/// `exec::query::is_already_of_type` asks `fits`, which is the same question with a single answer.
#[test]
fn every_comparison_pair_agrees_with_postgresql_19() {
    let mut node = parity::Node::new(&[]);
    let capture = include_str!("captures/pg19_comparison_matrix.txt");
    let (mut checked, mut wrong) = (0, Vec::new());
    for line in capture.lines().filter(|line| !line.starts_with('#')) {
        let Some((statement, expected)) = line.split_once('\t') else {
            continue;
        };
        // **The node's own sentence, where it has one.** A row where PostgreSQL answers and this
        // node refuses is only half reported by the word "refuses": the message names which gate
        // fired, and reading it is what told `int8range` apart from the twenty-six pairs beside it.
        let answered = node.run(statement).map_err(|error| error.to_string());
        if answered.is_ok() == expected.starts_with('!') {
            wrong.push(format!(
                "{statement}\n  PostgreSQL {} · node {}",
                if expected.starts_with('!') {
                    "refuses"
                } else {
                    "answers"
                },
                match &answered {
                    Ok(_) => "answers".to_owned(),
                    Err(error) => format!("refuses: {error}"),
                }
            ));
        }
        checked += 1;
    }
    assert!(
        checked > 2_600,
        "only {checked} pairs read; the capture did not load"
    );
    assert!(
        wrong.is_empty(),
        "{} of {checked} disagree:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// **The third pairing: a column against a typed literal** — measured, and handed over measured.
///
/// The other two forms are the one above (two literals) and the column form the module note
/// describes; a **mixed** pair takes neither's arms. `reconcile` matches `(Ordinal, Literal)`
/// before any family check and calls `retype`, whose first gate is `Literal::comparable_with` —
/// which ends in `Datum::fits`, the **assignment** rule. That is exactly the shape the cast arms
/// had until `debts-v1.1.md` #43's second mechanism closed, so it is the place to look next and
/// the reason this capture exists at all.
///
/// 52 columns against 53 literals — `void` is the one spelling that cannot be a column — and
/// PostgreSQL answers 153 of the 2,756. **86 disagreed when it was first measured**, in three
/// mechanisms, and they came out in that order — each one only visible once the one in front of
/// it had gone:
///
/// ```text
/// 44 -> 3   PG refuses, node ANSWERS   a range value `fits` every column there is
///  3 -> 0                              …and `same_family` finishes the job
/// 31 ->     PG answers, node refuses   42883, `comparable_with` ending in `Datum::fits`
/// 11 ->     PG answers, node refuses   42804, `retype` narrowing where it should compare
///  9 -> 0   PG answers, node refuses   an integer value's list that stopped at `oid`
/// ```
///
/// **The 41 were one line.** `Datum::fits` answered a range by
/// `row::range_subtype(ty) == subtype`, and `range_subtype` is total with `Timestamp` as its
/// fallback — so a `tsrange` value fitted *every* non-range column and `c = '…'::tsrange`
/// answered for a `boolean`, a `box`, an `xml`. `esker_keys::row::is_range` is the question it
/// was missing.
///
/// **The 31 and the 11 were two gates in order.** `comparable_with`'s last arm was
/// `value.fits(ty)` — the **assignment** rule, which its own doc comment says is the wrong
/// question for a comparison — so `bigint_col = 1.5::float8`, `date_col = '…'::timestamp`,
/// `citext_col = 'x'::text`, `inet_col = '…'::cidr` and `regtype_col = 1::int8` were each
/// `42883`. Behind them `retype` went on to `literal.assign(ty)`, and assigning an `oid` into a
/// `regproc` is `42804 column "?column?" is of type regproc but expression is of type oid`.
///
/// **And behind those, nine more from a fourth list.** `comparable_with` had three arms of its
/// own for a typed `Datum` — a `numeric`'s, an integer's, an oid-ish one's — each a hand-written
/// set. The integer one reached `oid` and stopped there, so `regtype_col = 1::bigint` was
/// `42883`. All three are gone: `same_family` gives each of them the answers it gave and gives
/// the nine the right one.
///
/// The whole of it is one sentence — **ask the measured table, not the assignment rule** — and it
/// is the sentence `debts-v1.1.md` #43's second mechanism made one arm over.
#[test]
fn every_column_against_a_typed_literal_agrees_with_postgresql_19() {
    let capture = include_str!("captures/pg19_comparison_matrix_column.txt");
    let fixture: Vec<&str> = capture
        .lines()
        .filter_map(|line| line.strip_prefix("# SETUP: "))
        .collect();
    assert_eq!(fixture.len(), 2, "the capture must carry its own fixture");
    let mut node = parity::Node::new(&fixture);
    let (mut checked, mut wrong) = (0, Vec::new());
    for line in capture.lines().filter(|line| !line.starts_with('#')) {
        let Some((statement, expected)) = line.split_once('\t') else {
            continue;
        };
        let answered = node.run(statement).map_err(|error| error.to_string());
        if answered.is_ok() == expected.starts_with('!') {
            wrong.push(format!(
                "{statement}\n  PostgreSQL {} · node {}",
                if expected.starts_with('!') {
                    "refuses"
                } else {
                    "answers"
                },
                match &answered {
                    Ok(_) => "answers".to_owned(),
                    Err(error) => format!("refuses: {error}"),
                }
            ));
        }
        checked += 1;
    }
    assert!(
        checked > 2_700,
        "only {checked} pairs read; the capture did not load"
    );
    assert!(
        wrong.is_empty(),
        "{} of {checked} disagree:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}
