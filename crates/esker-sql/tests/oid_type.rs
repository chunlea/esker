//! **`oid` is a four-byte unsigned integer, and the catalog's columns are declared as one.**
//!
//! r1's wire sweep and every `pg_catalog` corpus in this tree say the same thing in different
//! words: a real server's `pg_class.oid`, `pg_type.oid`, `attrelid`, `atttypid` — 308 columns in
//! all, 46 distinct names — are `oid` (26), and this node answered `bigint` (20). The digits were
//! always identical; what a client is *told* was not.
//!
//! **The type already existed** (ADR 0077 built it for `regtype`, which is an `oid` underneath).
//! What this unit does is declare the columns, and it does that in one place: a view's rows are
//! coerced to the types its column list declares (`CatalogView::as_declared`), so a builder that
//! writes a `Datum::Int8` — which every one of them does, because an id is a `u64` here — cannot
//! leave a row disagreeing with its own `RowDescription`.
//!
//! **The half that cannot be declared, and why: a column that names a *relation* stays `bigint`.**
//! An `oid` is four bytes, and two things put a relation's oid outside them — a primary key's index
//! takes `pg_relations::PRIMARY_KEY_OID_BASE + table_id` (bit 62) because it has no record of its
//! own, and a catalog view takes `VIEW_ID_BASE`, near `i64::MAX`, because it has no record either.
//! `pg_constraint`'s four derived bases sit between them at bits 56–61. Every other id in this
//! crate comes from a counter that starts at `catalog::FIRST_USER_ID` (16384) and increments.
//!
//! **The rule is about queries, not stored values.** [`no_declared_oid_column_saturates`] caught
//! `pg_class.oid` and `pg_attribute.attrelid`, which really do carry a `PRIMARY_KEY_OID_BASE`
//! value; the other two witnesses were client queries — `ActiveRecord`'s serial-sequence join
//! `WHERE dep.classid = 'pg_class'::regclass` and `WHERE i.indrelid = '"pg_type"'::regclass` — that
//! answered `22003 … out of range for type oid` because the **comparison** coerced a catalog id
//! into four bytes. So a column is judged by what a client may compare it against:
//! `pg_attrdef.adrelid`'s values all fit and it is a `bigint` all the same (ADR 0097).
//!
//! Measured in `tests/captures/pg19_oid_type.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::pgwire::session::Execute;

#[path = "parity_harness/mod.rs"]
mod parity;

/// A schema with one of everything that owns an oid.
const FIXTURE: &[&str] = &[
    "CREATE TABLE oidp (id int8 PRIMARY KEY, note text NOT NULL, n int4 CHECK (n > 0))",
    "CREATE TABLE oidc (id int8 PRIMARY KEY, parent int8 REFERENCES oidp(id))",
    "CREATE INDEX oidc_parent_idx ON oidc (parent)",
    "CREATE SEQUENCE oid_s",
    "CREATE VIEW oidv AS SELECT id FROM oidp",
    "CREATE TYPE oid_mood AS ENUM ('sad', 'ok')",
];

/// **The saturation guard.** `u32::MAX` is what `as_declared` writes for an id that does not fit,
/// and no id in this fixture is legitimately 4294967295 — so a row carrying one is a column that
/// should not have been declared `oid`.
///
/// This is the test that decides the split rather than a list somebody remembered: flipping a
/// column whose values come from one of the five derived bases reddens it immediately.
#[test]
fn no_declared_oid_column_saturates() {
    let mut node = parity::Node::new(FIXTURE);
    let mut saturated = Vec::new();
    for (view, column) in DECLARED {
        let rows = node.rows(&format!(
            "SELECT count(*) FROM {view} WHERE {column} = 4294967295"
        ));
        if rows.first().and_then(|row| row.first()).map(String::as_str) != Some("0") {
            saturated.push(format!("{view}.{column}"));
        }
    }
    assert!(
        saturated.is_empty(),
        "these columns are declared `oid` and carry a saturated id, so their values do not fit \
         four bytes: {saturated:?}"
    );
}

/// Every column this node declares `oid`, which is every column of the oracle's census it has and
/// whose ids fit — `captures/pg19_oid_type.txt` holds the census and the query that took it.
const DECLARED: &[(&str, &str)] = &[
    ("pg_type", "oid"),
    ("pg_type", "typelem"),
    ("pg_type", "typbasetype"),
    ("pg_type", "typcollation"),
    ("pg_type", "typnamespace"),
    ("pg_type", "typarray"),
    ("pg_range", "rngtypid"),
    ("pg_range", "rngsubtype"),
    ("pg_class", "relnamespace"),
    ("pg_class", "relam"),
    ("pg_am", "oid"),
    ("pg_cast", "castsource"),
    ("pg_cast", "casttarget"),
    ("pg_opclass", "oid"),
    ("pg_opclass", "opcmethod"),
    ("pg_opclass", "opcintype"),
    ("pg_namespace", "oid"),
    ("pg_collation", "oid"),
    ("pg_language", "oid"),
    ("pg_proc", "oid"),
    ("pg_proc", "pronamespace"),
    ("pg_proc", "prolang"),
    ("pg_trigger", "oid"),
    ("pg_trigger", "tgfoid"),
    ("pg_database", "oid"),
    ("pg_locks", "classid"),
    ("pg_locks", "objid"),
    ("pg_sequence", "seqtypid"),
    ("pg_enum", "enumtypid"),
    ("pg_attribute", "atttypid"),
    ("pg_attribute", "attcollation"),
    ("pg_attrdef", "oid"),
    ("pg_constraint", "connamespace"),
    ("pg_constraint", "contypid"),
];

/// **The wire says 26**, which is the whole point: the digits never changed.
#[test]
fn the_declared_type_reaches_the_client() {
    let mut node = parity::Node::new(FIXTURE);
    for (view, column) in DECLARED {
        let statement = format!("SELECT {column} FROM {view}");
        let parsed = esker_sql::parse::parse_statements(&statement).unwrap();
        let described = node.executor.describe(&parsed[0], &[]).unwrap();
        let fields = described.fields.expect("a SELECT returns rows");
        assert_eq!(
            fields[0].type_oid, 26,
            "{view}.{column} is declared {} where a real server says 26",
            fields[0].type_oid
        );
        // Four bytes, which is what separates it from the `bigint` it used to be.
        assert_eq!(fields[0].type_size, 4, "{view}.{column}");
    }
}

/// `pg_type` carries `oid` and `oid[]`, and `format_type` prints them without quotes.
#[test]
fn pg_type_has_the_oid_rows() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT oid, typname, typlen, typtype, typcategory, typdelim, typinput, typarray \
             FROM pg_type WHERE typname IN ('oid', '_oid') ORDER BY oid"
        ),
        vec![
            // **`typcategory` is `N`** — numeric, with the integers, not a category of its own.
            // That is why `CASE WHEN true THEN 1::oid ELSE 1::int8 END` has a common type where
            // the same shape over a `"char"` is `42804` (ADR 0095): `oid` unifies with the
            // integers and wins, measured.
            vec!["26", "oid", "4", "b", "N", ",", "oidin", "1028"],
            vec!["1028", "_oid", "-1", "b", "A", ",", "array_in", "0"],
        ]
    );
    assert_eq!(
        node.rows("SELECT format_type(26, -1), format_type(1028, -1)"),
        vec![vec!["oid", "oid[]"]]
    );
}

/// **It sorts as a number, not as a string**, which is the reason the type is not `text`.
#[test]
fn it_orders_numerically_and_compares_with_the_integers() {
    let mut node = parity::Node::new(&[]);
    // `'10' < '9'` is true for text and false for a number.
    assert_eq!(node.rows("SELECT '10'::oid < '9'::oid"), vec![vec!["f"]]);
    assert_eq!(node.rows("SELECT 1::oid < 2::oid"), vec![vec!["t"]]);
    assert_eq!(
        node.rows("SELECT 1::oid = 1::int4, 1::oid = 1::int8, 1::oid = 1::int2"),
        vec![vec!["t", "t", "t"]]
    );
    // **And the reg* types compare with the integers by their oid, not by their rank.** Every
    // oid-ish value ranks *as* an `Oid` in `pg_cmp`'s total order and every integer ranks with it,
    // so a pair with no arm of its own came back `Equal`: `'pg_class'::regclass = 1::int2` was
    // **`t`**, and so was every regclass against every `int2` or `int4`. The arms had been written
    // one pairing at a time — `oid` against all three widths, `regclass` against `oid` and `int8`
    // — and the eight nobody wrote are where a wrong *value* came out.
    //
    // `int4`'s own oid is 23, which is what makes the second column discriminating: a row that is
    // `t` for the right reason beside rows that must be `f`.
    //
    // Measured on 19beta1, 2026-09-10, inside `BEGIN … ROLLBACK`.
    assert_eq!(
        node.rows(
            "SELECT 'int4'::regtype = 1::int2, 'int4'::regtype = 23::int2, \
             'pg_class'::regclass = 1::int2, 'int4in'::regproc = 1::int2, \
             'pg_class'::regclass = 'int4'::regtype"
        ),
        vec![vec!["f", "t", "f", "f", "f"]]
    );
    // **And against an integer the fold could not swallow**, which is the same comparison with one
    // side arriving as a `Cast` node rather than as a constant: `(23.4)::integer` keeps its node
    // because rounding is not invertible (`debts-v1.1.md` #30), so the oid-ish literal is retyped
    // against the cast. It was `42883 operator does not exist: regtype = integer` —
    // `Literal::comparable_with` fell through to `Datum::fits`, the *assignment* rule, which says
    // an `oid` is not an `integer` — and then `42804 ... is of type integer but expression is of
    // type regtype`, because `retype` went on to narrow it as an assignment too. Two readers of
    // one fact, one behind the other, and both are `debts-v1.1.md` #43's shape.
    assert_eq!(
        node.rows(
            "SELECT 'int4'::regtype = (99.4)::integer, 'int4'::regtype = (23.4)::integer, \
             1::oid = (1.5)::integer"
        ),
        vec![vec!["f", "t", "f"]]
    );
    // And the widening stops where a real server stops it: an `oid` has no operator against a
    // `numeric` or either float, which is the family table's rule and is measured beside the rows
    // above.
    assert!(node.run("SELECT 1::oid = (1.5)::numeric").is_err());
}

/// **Unsigned, and the two directions are not symmetric** — measured, both.
#[test]
fn a_negative_wraps_and_too_large_is_refused() {
    let mut node = parity::Node::new(&[]);
    // `oidin` reinterprets the bits rather than refusing.
    assert_eq!(node.rows("SELECT '-1'::oid"), vec![vec!["4294967295"]]);
    assert_eq!(
        node.rows("SELECT (-1)::int4::oid"),
        vec![vec!["4294967295"]]
    );
    // And the upper bound is a range error, not a wrap.
    assert!(node.run("SELECT '4294967296'::oid").is_err());
    // **Hexadecimal**, which `oidin` accepts and no reader guesses: `0x10` is 16.
    assert_eq!(node.rows("SELECT '0x10'::oid"), vec![vec!["16"]]);
    // **And a leading zero is octal**, which is the other half of `strtoul`'s rule and the half
    // that changes an answer silently: `'010'` is 8 here and 10 to `int4in`, measured. `'08'` is
    // then a *syntax* error, because 8 is not an octal digit.
    assert_eq!(
        node.rows("SELECT '010'::oid, '010'::int4"),
        vec![vec!["8", "10"]]
    );
    assert!(node.run("SELECT '08'::oid").is_err());
    // Neither of PostgreSQL's own newer prefixes, and no digit separators — `int4in` takes all
    // three and `oidin` takes none.
    assert!(node.run("SELECT '0o17'::oid").is_err());
    assert!(node.run("SELECT '0b101'::oid").is_err());
    assert!(node.run("SELECT '1_000'::oid").is_err());
    // A negative hexadecimal wraps like any other negative.
    assert_eq!(node.rows("SELECT '-0x10'::oid"), vec![vec!["4294967280"]]);
    assert_eq!(
        node.rows("SELECT '0xffffffff'::oid"),
        vec![vec!["4294967295"]]
    );
    assert!(node.run("SELECT '0x100000000'::oid").is_err());
    // Whitespace is trimmed; a non-number is a syntax error rather than a range one.
    assert_eq!(
        node.rows("SELECT ' 42 '::oid, ' 0x10 '::oid"),
        vec![vec!["42", "16"]]
    );
    assert_eq!(node.rows("SELECT '+10'::oid"), vec![vec!["10"]]);
    assert!(node.run("SELECT 'abc'::oid").is_err());
    assert!(node.run("SELECT ''::oid").is_err());
}

/// **`min`/`max` keep the type**, which is where `oid` parts company with `varchar`, `name`,
/// `cidr` and `"char"` — all four of those decay to `text` and this one does not. Measured.
#[test]
fn the_aggregates_keep_it_and_arithmetic_is_refused() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows("SELECT pg_typeof(min(atttypid)) FROM pg_attribute"),
        vec![vec!["oid"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(array_agg(oid)) FROM (SELECT oid FROM pg_type LIMIT 2) s"),
        vec![vec!["oid[]"]]
    );
    // `count` is a `bigint` over anything.
    assert_eq!(
        node.rows("SELECT pg_typeof(count(oid)) FROM pg_type"),
        vec![vec!["bigint"]]
    );
    // **No arithmetic at all**: a real server has no `oid + integer`, which is what says an oid is
    // an identifier and not a number you may do sums with.
    assert!(node.run("SELECT 1::oid + 1").is_err());
    assert!(node.run("SELECT sum(oid) FROM pg_type").is_err());
}
