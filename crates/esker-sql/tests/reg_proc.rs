//! **`regproc` is an oid that prints as a function's name**, against PostgreSQL 19beta1.
//!
//! Oid 24, `typlen` 4, `typcategory` `N`, `typinput` `regprocin`, `typarray` 1008. A real server
//! declares it on **forty** catalog columns; this node has exactly one of them,
//! `pg_type.typinput`, because the other thirty-nine are in catalogs it does not serve
//! (`pg_aggregate`, `pg_operator`, `pg_ts_parser`, …). The census is in
//! `tests/captures/pg19_reg_proc.txt`.
//!
//! Three facts carry it, and each is measured:
//!
//! * **An oid no function has prints as the number.** `42::regproc` is `int4in` and
//!   `24::regproc` is `24`. There are far more oids without a function than `pg_proc` has rows, so
//!   the digits are the common case — which is why the name rides in the datum beside the oid,
//!   exactly as a `regtype`'s does.
//! * **`min`/`max` decay to `oid`**, where a `regtype` keeps its own type. That makes `regproc`
//!   the fifth member of the decay family after `varchar`, `name`, `cidr` and `"char"`, and the
//!   first whose landing type is not `text`.
//! * **A comparison reads an unadorned literal as an `oid`, not as a name.**
//!   `typinput = 'array_in'` is `22P02 invalid input syntax for type oid: "array_in"` on a real
//!   server, because `=` over a `regproc` is `oideq`. An *assignment* is the other way round:
//!   `regprocin` resolves a name. `tests/array_delimiter.rs` wrote the refused form and passed,
//!   because `typinput` was a `text` column here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::pgwire::session::Execute;
use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// `pg_type` carries `regproc` and `regproc[]`, and `format_type` prints them plainly.
#[test]
fn pg_type_has_the_regproc_rows() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT oid, typname, typlen, typtype, typcategory, typdelim, typinput, typarray \
             FROM pg_type WHERE typname IN ('regproc', '_regproc') ORDER BY oid"
        ),
        vec![
            // **`N`, with the integers** — an identifier PostgreSQL groups with the numbers, the
            // same call it makes for `oid` (ADR 0097).
            vec!["24", "regproc", "4", "b", "N", ",", "regprocin", "1008"],
            vec!["1008", "_regproc", "-1", "b", "A", ",", "array_in", "0"],
        ]
    );
    assert_eq!(
        node.rows("SELECT format_type(24, -1), format_type(1008, -1)"),
        vec![vec!["regproc", "regproc[]"]]
    );
}

/// **The column the suite reads**, declared as what it is.
#[test]
fn typinput_is_a_regproc_on_the_wire() {
    let mut node = parity::Node::new(&[]);
    let parsed = esker_sql::parse::parse_statements("SELECT typinput FROM pg_type").unwrap();
    let described = node.executor.describe(&parsed[0], &[]).unwrap();
    let fields = described.fields.expect("a SELECT returns rows");
    assert_eq!(fields[0].type_oid, 24);
    // Four bytes, which is the oid's width — the name is the output function's business.
    assert_eq!(fields[0].type_size, 4);
    // And the value a client reads is still the name.
    assert_eq!(
        node.rows("SELECT typinput FROM pg_type WHERE typname = 'int4'"),
        vec![vec!["int4in"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(typinput) FROM pg_type LIMIT 1"),
        vec![vec!["regproc"]]
    );
}

/// **An oid no function has prints as the number**, which is the half reasoning gets wrong.
#[test]
fn an_oid_without_a_function_prints_its_digits() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 42::regproc, 24::regproc"),
        vec![vec!["int4in", "24"]]
    );
    // A name resolves, whitespace is trimmed, and a schema qualification is dropped.
    assert_eq!(
        node.rows("SELECT 'int4in'::regproc, ' int4in '::regproc, 'pg_catalog.int4in'::regproc"),
        vec![vec!["int4in", "int4in", "int4in"]]
    );
    // **A name nothing has is `42883`**, an undefined *function* — the input function resolves
    // rather than parses, so it is not a syntax error.
    let error = node.run("SELECT 'nosuchfn'::regproc").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_FUNCTION);
    assert!(
        error.to_string().contains("\"nosuchfn\""),
        "the refusal did not quote the name: {error}"
    );
}

/// **A comparison reads the literal as an `oid`**, which refuses a name.
#[test]
fn a_bare_literal_beside_one_is_read_as_an_oid() {
    let mut node = parity::Node::new(&[]);
    let error = node
        .run("SELECT count(*) FROM pg_type WHERE typinput = 'array_in'")
        .unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::INVALID_TEXT_REPRESENTATION);
    assert!(
        error.to_string().contains("oid"),
        "the refusal did not name the type the literal was read as: {error}"
    );
    // The two forms a real server answers, and they agree with each other.
    let cast = node.rows("SELECT count(*) FROM pg_type WHERE typinput = 'array_in'::regproc");
    let text = node.rows("SELECT count(*) FROM pg_type WHERE typinput::text = 'array_in'");
    assert_eq!(cast, text);
    assert_ne!(cast[0][0], "0", "no array type was found at all");
    // And the oid form, which is what the operator is really about.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_type WHERE typinput = 750"),
        cast
    );
}

/// **`min`/`max` decay to `oid`** — the fifth member of that arm, and the first that is not `text`.
#[test]
fn the_aggregates_decay_to_an_oid_and_the_array_keeps_the_type() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT pg_typeof(min(typinput)), pg_typeof(max(typinput)) FROM pg_type"),
        vec![vec!["oid", "oid"]]
    );
    assert_eq!(
        node.rows(
            "SELECT pg_typeof(array_agg(typinput)) FROM (SELECT typinput FROM pg_type LIMIT 2) s"
        ),
        vec![vec!["regproc[]"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(count(typinput)) FROM pg_type"),
        vec![vec!["bigint"]]
    );
}

/// **Every name `typinput` can answer resolves to a function**, which is the rule and not a list.
///
/// `pg_catalog::typinput` returns a `&'static str` per type; `value::reg_proc` holds the oid each
/// of those names has on a real server. A type added to one without the other prints its digits
/// where a real server prints a name, and nothing else would catch it — the *rendered* value is
/// the name either way, because the name rides in the datum.
#[test]
fn every_typinput_resolves_to_a_function() {
    let mut node = parity::Node::new(&[]);
    let digits =
        node.rows("SELECT typname, typinput FROM pg_type WHERE typinput::oid = 0 ORDER BY typname");
    assert!(
        digits.is_empty(),
        "these types name an input function `value::reg_proc` does not know: {digits:?}"
    );
}
