//! **A domain survives `array_agg` and does not survive `min`/`max`** — `debts-v1.1.md` #106.
//!
//! Two corpora already cover the two axes and neither covers the pair: `pg19_domain.txt` and
//! `pg19_domain_schema.txt` never aggregate, and the twenty-odd files that name `array_agg` never
//! declare a domain. That cross product is where this defect lives, which is where the last one
//! lived too — the capture for #103 found its defect the same way, by varying an axis nobody had
//! varied.
//!
//! **The three answers are different and all three are right**, which is what makes this a rule
//! rather than a coincidence: `min(s)` is `text`, because the aggregate resolves to the base type's
//! operator family; `array_agg(s)` is `d[]`, because it compares nothing and collects; and
//! `array_agg(t)` over a plain `text` column is `text[]`, the control that says the divergence is
//! about the *domain* and not about `array_agg`. A fix that made the first two agree would break a
//! line to pay a line.
//!
//! `array_agg(DISTINCT s)` is `d[]` too, although `DISTINCT` does compare — the comparison is
//! inside the aggregate, and the type it answers is still an array of what it collected.
//!
//! Measured 2026-09-16, `esker-coord/s2-h106.out`, created and dropped outside a transaction with
//! the drop as a second pass, so the declared types survive the `\gdesc` pass that runs afterwards
//! in a fresh session; its own last line counts the domain back to zero.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus declares its own domain and table.
const CORPUS_FIXTURE: &[&str] = &[];

/// **Empty, and that is the claim.** Every statement here is one this node is expected to answer
/// exactly as 19beta1 does. An entry would say "this node deliberately answers differently", which
/// is not what a defect is — the difference is the bug, so it belongs in a red test rather than in
/// a declared exception.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

/// Every statement of the capture, replayed.
///
/// The count is asserted because a corpus that fails to load replays nothing and passes: "all zero
/// statements agreed" is the shape of a green run that measured nothing at all.
#[test]
fn every_domain_aggregate_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_domain_aggregate.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 14,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **`format_type` of a user type's array prints `d[]`, and this node cannot name that oid at all**
/// — `debts-v1.1.md` #106's second gap, and this test is what was missing when it was left unbuilt.
///
/// Measured on 19beta1 (`esker-coord/s2-h106b.out`), over a domain and an enum alike:
///
/// ```text
/// format_type(t.oid, NULL)       h106f        h106e
/// format_type(t.typarray, NULL)  h106f[]      h106e[]
/// '_h106f'::regtype::text        h106f[]      -- stored `_h106f`, printed `h106f[]`
/// ```
///
/// The array row is **already in this node's `pg_type`**: `catalog::pg_catalog::user_type_rows`
/// emits it with `oid + 1`, `typname` `_{bare}`, `typelem` the type's oid. What is missing is the
/// way back — `Relations::user_type_name` is keyed on `def.oid`, so an oid one higher finds
/// nothing and `format_type` falls through to its unknown-type answer.
///
/// The three assertions are a discrimination and not one check: the type's own name must keep
/// printing bare, the array row must be there with the name `_d`, and only the printed form of that
/// row is wrong. A fix that made the first two move would be reaching past this gap.
#[test]
fn format_type_names_the_array_of_a_user_type() {
    // The table is here for the oid assertion at the end: `array_agg` needs rows, and this test
    // declared only the domain until that assertion was added.
    let mut node = parity::Node::new(&[
        "CREATE DOMAIN d AS text",
        "CREATE TABLE dt (s d)",
        "INSERT INTO dt VALUES ('a')",
    ]);

    // The row exists, and its stored name is the underscore form.
    assert_eq!(
        node.rows(
            "SELECT t.typname, a.typname FROM pg_type t JOIN pg_type a ON a.oid = t.typarray \
             WHERE t.typname = 'd'"
        ),
        [["d", "_d"]]
    );

    // The type's own name prints bare, as it does today.
    assert_eq!(
        node.rows("SELECT format_type(t.oid, NULL) FROM pg_type t WHERE t.typname = 'd'"),
        [["d"]]
    );

    // And the array's prints `d[]`.
    assert_eq!(
        node.rows("SELECT format_type(t.typarray, NULL) FROM pg_type t WHERE t.typname = 'd'"),
        [["d[]"]]
    );

    // **The oid, and not only the name.** A counterfactual walked through this: mutating
    // `pg_typeof_of` to answer the type's own oid instead of its array's left every assertion here
    // green, because they all read the *printed* name and the mutation changed only the number.
    // A wrong oid reaches a client in `RowDescription` and in any `regtype` comparison, so it is
    // asserted against the one `pg_type` publishes.
    //
    // Compared in the `oid -> regtype` direction on purpose: that is the cast
    // `value::regtype_of_oid` implements and `tests/array_delimiter.rs` already leans on, where
    // `regtype::oid` is only measured on the PostgreSQL side.
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(array_agg(s)) = \
             (SELECT t.typarray FROM pg_type t WHERE t.typname = 'd')::regtype FROM dt"
        ),
        [["t"]]
    );
}

/// **An enum survives `array_agg` the way a domain does** — the other half of #106's rule, and the
/// reason the fix is not narrowed to one kind of user type.
///
/// Asked of the live oracle directly (`esker-coord/s2-h106c.out`, PostgreSQL 19beta1), because a
/// gate failure had been attributed to the opposite rule — that a domain in a polymorphic argument
/// position matches as its base, so `array_agg(<domain>)` would be the base's array. It does not:
///
/// ```text
/// array_agg(domain)      h106dom[]
/// array_agg(enum)        h106enum[]
/// array_agg(composite)   h106comp[]
/// array_agg(text)        text[]      -- the control
/// ```
///
/// **The composite row is measured and deliberately not asserted here**: whether this node takes a
/// composite-typed column is not something this unit established, and a test red for that would be
/// red for a defect it is not about. It is recorded so the next reader does not have to re-measure.
#[test]
fn an_enum_survives_array_agg_the_way_a_domain_does() {
    let mut node = parity::Node::new(&[
        "CREATE TYPE mood AS ENUM ('sad', 'ok')",
        "CREATE TABLE t (m mood, s text)",
        "INSERT INTO t VALUES ('ok', 'p')",
    ]);

    // The bare column, which already answered the enum before this unit.
    assert_eq!(node.rows("SELECT pg_typeof(m) FROM t"), [["mood"]]);

    // The aggregate, which is what #106 added.
    assert_eq!(
        node.rows("SELECT pg_typeof(array_agg(m)) FROM t"),
        [["mood[]"]]
    );

    // And the control: a plain `text` column is `text[]` on both servers, which is what says the
    // rule is about the user type and not about `array_agg`.
    assert_eq!(
        node.rows("SELECT pg_typeof(array_agg(s)) FROM t"),
        [["text[]"]]
    );
}
