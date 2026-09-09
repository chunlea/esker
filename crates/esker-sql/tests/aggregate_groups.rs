//! **The four small aggregate groups** of r1's wire-108 baseline, measured together.
//!
//! Group 8 is `sum`/`avg` over a `real`, group 9 `min`/`max` over `tsvector` and `tsquery`,
//! group 10 an aggregate over a *domain* column, and group 11 `string_agg`. They are one file
//! because they are one question asked four ways — what does an aggregate answer, and over which
//! types does it exist at all — and because three of the four turn on the same finding ADR 0031
//! wrote down: **the aggregate set is per (aggregate, type) and cannot be derived from the type.**
//!
//! Two of them are the two directions of being wrong. `sum(real)` was `42883` where a real server
//! answers, which is a feature missing; `min(tsvector)` *answered* where a real server refuses,
//! which ADR 0031 ranks as the worse of the two — a node that answers where PostgreSQL raises
//! teaches a client something false rather than nothing.
//!
//! Measured in `tests/captures/pg19_aggregate_groups.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
///
/// **Group 10 is the whole of the first block, and it is a type-system gap rather than an
/// aggregate one.** `information_schema.tables.table_name` is a *domain* on a real server —
/// `sql_identifier`, `typtype = 'd'`, base `name` — with an array type of its own, and
/// `array_agg` of it answers that array. This node has no way to say so: `ColumnType` is a closed
/// enum of storage types, a domain is carried beside a column rather than as one, and the oid a
/// client is sent has to come from the enum. Answering `_name` is the base type's array, which is
/// the right *values* under the wrong name. Closing it is the domain half of the type surface,
/// which is a unit with an ADR in it and not an arm in this one — **`debts-v1.1.md` #37**, which
/// is where the sizing lives so that this list does not have to carry it.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT pg_typeof(array_agg(table_name)) FROM (SELECT table_name FROM information_schema.tables LIMIT 2) s",
            "**A domain's array, which this node has no type for.** `table_name` is the domain \
             `sql_identifier` on a real server and `array_agg` of it is `_sql_identifier` (13360); \
             here the column is its base type `name` and the aggregate answers `_name` (1003). \
             The values are identical — a domain adds a constraint, not a representation — and \
             what differs is the name a client is told.",
            "pg19_aggregate_groups.txt:77",
        ),
        (
            "SELECT pg_typeof(table_name) FROM information_schema.tables LIMIT 1",
            "The same gap one step earlier, and the one that causes it: the *column* is the domain \
             on a real server and its base type here. Every `information_schema` column is one of \
             five such domains there.",
            "pg19_aggregate_groups.txt:78",
        ),
        (
            "SELECT oid, typname, typtype, typbasetype, typarray FROM pg_type WHERE typname = 'sql_identifier'",
            "No row: this node's `pg_type` has no `sql_identifier`, because it has no domain to \
             put there. The oids are recorded in the capture rather than asserted — 13361 and \
             13360 are assigned when `information_schema` is created, not fixed by catalog \
             version — and what a reader needs from them is the *shape*: `typtype = 'd'`, \
             `typbasetype` 19, and an array of its own.",
            "pg19_aggregate_groups.txt:79",
        ),
        (
            "SELECT oid, typname, typtype, typelem FROM pg_type WHERE typname = '_sql_identifier'",
            "The other half of the same absence: a domain's array is an ordinary base-type row \
             pointing back at the domain, which is what makes 13360 a type a client can be sent.",
            "pg19_aggregate_groups.txt:80",
        ),
        (
            "SELECT string_agg(t, ',') FILTER (WHERE t <> 'a') FROM ag",
            "**`FILTER` is not built for any aggregate**, so this is `0A000` naming the clause \
             rather than anything about `string_agg`. It is here because the corpus that built \
             `string_agg` is where a reader will look for it, and because the clause is the one \
             piece of the aggregate grammar this node still refuses by name.",
            "pg19_aggregate_groups.txt:66",
        ),
        // The one-argument form is **not** here: `string_agg(t)` is the same refusal with the
        // same code on both sides, and the `DETAIL` a real server adds — "the given number of
        // arguments", against "the given argument types" for a wrong type — is the sentence this
        // crate does not yet tell apart. The harness compares what it compares, and this one
        // agreed; the entry came off under rule 2 the moment it was written.
        (
            "SELECT string_agg(r, ',') FROM ag",
            "The same refusal naming one argument where a real server names both — \
             `string_agg(real)` against `string_agg(real, unknown)`. This crate resolves the \
             aggregate from its first argument, so the message has one type to print; the code and \
             the fact that the pair does not exist are the same.",
            "pg19_aggregate_groups.txt:68",
        ),
    ],
};

#[test]
fn every_aggregate_group_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_aggregate_groups.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 38,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **A `real` sums to a `real` and averages to a `double precision`** — group 8, both probes.
#[test]
fn a_real_sums_narrow_and_averages_wide() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ag (r real)",
        "INSERT INTO ag VALUES (1.5), (2.25), (NULL)",
    ]);
    for (statement, answer, oid) in [
        ("SELECT sum(r) FROM ag", "3.75", 700),
        ("SELECT avg(r) FROM ag", "1.875", 701),
    ] {
        assert_eq!(
            node.rows(statement),
            vec![vec![answer.to_owned()]],
            "{statement}"
        );
        let outcome = node.run(statement).unwrap();
        let esker_sql::pgwire::session::Outcome::Rows { fields, .. } = outcome else {
            panic!("{statement}: no rows");
        };
        // **The declared type is the point of the group**: 700 is `real` and 701 is
        // `double precision`, and a client that binds against the wrong one reads a different
        // number of digits than the server sent.
        assert_eq!(fields[0].type_oid, oid, "{statement}");
    }
    // Over no rows both are NULL, which is every fold's rule and not this pair's.
    assert_eq!(
        node.rows("SELECT sum(r), avg(r) FROM ag WHERE false"),
        vec![vec!["\\N", "\\N"]]
    );
}

/// **`min`/`max` do not exist over `tsvector` or `tsquery`** — group 9, all four probes.
///
/// This node answered them, which is the class ADR 0031 ranks worst. The list of types that *do*
/// have a `min` is a census of `pg_proc` — twenty-five one-argument overloads — rather than a
/// collection assembled one surprise at a time, and this pair was the last of r1's sweep.
#[test]
fn the_two_text_search_types_have_no_extreme() {
    let mut node = parity::Node::new(&[]);
    for (statement, named) in [
        (
            "SELECT min(v) FROM (VALUES ('a b'::tsvector)) s(v)",
            "min(tsvector)",
        ),
        (
            "SELECT max(v) FROM (VALUES ('a b'::tsvector)) s(v)",
            "max(tsvector)",
        ),
        (
            "SELECT min(q) FROM (VALUES ('a & b'::tsquery)) s(q)",
            "min(tsquery)",
        ),
        (
            "SELECT max(q) FROM (VALUES ('a & b'::tsquery)) s(q)",
            "max(tsquery)",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(
            error.sqlstate(),
            sqlstate::UNDEFINED_FUNCTION,
            "{statement}"
        );
        assert!(
            error.to_string().contains(named),
            "the refusal names the function and its argument type: {error}"
        );
    }
    // **What still answers**, so that this is a rule about `min` and not about the type: `count`
    // takes anything and `array_agg` has an array to build.
    assert_eq!(
        node.rows("SELECT count(v) FROM (VALUES ('a b'::tsvector)) s(v)"),
        vec![vec!["1"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(array_agg(v)) FROM (VALUES ('a b'::tsvector)) s(v)"),
        vec![vec!["tsvector[]"]]
    );
}

/// **`string_agg`** — group 11, and the rules that are not the obvious ones.
#[test]
fn string_agg_joins_a_group() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ag (id int8, t text)",
        "INSERT INTO ag VALUES (1, 'a'), (2, 'b'), (3, NULL)",
    ]);
    assert_eq!(
        node.rows("SELECT string_agg(t, ',') FROM ag"),
        vec![vec!["a,b"]]
    );
    // The `ORDER BY` inside the parentheses orders the join, by a column it does not return.
    assert_eq!(
        node.rows("SELECT string_agg(t, ',' ORDER BY t DESC) FROM ag"),
        vec![vec!["b,a"]]
    );
    assert_eq!(
        node.rows("SELECT string_agg(id::text, '-' ORDER BY id) FROM ag"),
        vec![vec!["1-2-3"]]
    );
    // `DISTINCT` folds before the join, as it does for every aggregate here.
    assert_eq!(
        node.rows("SELECT string_agg(DISTINCT t, ',') FROM ag"),
        vec![vec!["a,b"]]
    );
    // **A NULL delimiter is an empty separator, not a NULL answer** — measured.
    assert_eq!(
        node.rows("SELECT string_agg(t, NULL) FROM ag"),
        vec![vec!["ab"]]
    );
    // **Over no rows it is NULL, not the empty string**, which is `array_agg`'s rule and the
    // answer that surprises: an empty string is a value and a group with no rows has none.
    assert_eq!(
        node.rows("SELECT string_agg(t, ',') FROM ag WHERE false"),
        vec![vec!["\\N"]]
    );
    // **Two overloads and no coercion**: `(text,text)` and `(bytea,bytea)` are the whole of
    // `pg_proc`, so a number is `42883` rather than something rendered.
    let error = node.run("SELECT string_agg(id, ',') FROM ag").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    // And the one-argument form is a different refusal from a wrong type, in the same class.
    let error = node.run("SELECT string_agg(t) FROM ag").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
}
