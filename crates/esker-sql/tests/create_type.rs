//! `CREATE TYPE` — `AS RANGE`, `AS (…)` composite and `AS ENUM` — against PostgreSQL 19beta1.
//!
//! Run 45's third row: 51 tests across three files, and the capture's own finding is that the row
//! is **not** enums — `adapters/postgresql/range_test.rb` is 46 of the 51, `composite_test.rb` is
//! 4 and `timestamp_test.rb` is 1. Each file's `setup` runs the `CREATE TYPE` verbatim, so a
//! statement this node refuses errors every test in the file before it starts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own types and tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        // **An unknown type name is `0A000` here and `42704` there, for every statement that names
        // one.** Not something `CREATE TYPE` introduced: this node cannot tell a type PostgreSQL
        // has and it does not — `money`, say — from one nobody has, and for the first of those
        // `0A000 the type money is not supported` is the right answer. Telling them apart needs
        // the list of PostgreSQL's own type names, which is a unit of its own.
        (
            "CREATE TYPE badrange AS RANGE ( subtype = nosuchtype )",
            "an unknown type name is 0A000 here and 42704 there, everywhere a type is named",
            "pg19_create_type.txt:87",
        ),
    ],
};

#[test]
fn every_create_type_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_create_type.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// What `pg_type` says about each of the three kinds.
///
/// Held here rather than in the corpus because the capture reads them through
/// `typarray::regtype::text`, a per-row cast this node does not have yet: the columns answer, the
/// spelling the capture used to read them does not. The values are the capture's own —
/// `r`/`R`, `c`/`C`, `e`/`E`, `typlen` `-1`, and `typrelid <> 0` for the composite alone.
#[test]
fn pg_type_reports_the_three_kinds_and_the_array_type_each_one_made() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE TYPE floatrange AS RANGE ( subtype = float8, subtype_diff = float8mi )")
        .unwrap();
    node.run("CREATE TYPE full_address AS ( city VARCHAR(90), street VARCHAR(90) )")
        .unwrap();
    node.run("CREATE TYPE custom_time_format AS ENUM ('past', 'present', 'future')")
        .unwrap();

    let rows = |sql: &str, node: &mut parity::Node| match node.run(sql).unwrap() {
        esker_sql::pgwire::session::Outcome::Rows { rows, .. } => rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| {
                        cell.as_ref().map_or("\\N".to_owned(), |bytes| {
                            String::from_utf8_lossy(bytes).into_owned()
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect::<Vec<_>>(),
        other @ esker_sql::pgwire::session::Outcome::Done { .. } => {
            panic!("expected rows, got {other:?}")
        }
    };

    assert_eq!(
        rows(
            "SELECT typname, typtype, typcategory, typlen, typrelid <> 0 FROM pg_type WHERE \
             typname IN ('floatrange','full_address','custom_time_format') ORDER BY typname",
            &mut node
        ),
        // An enum is **four bytes** on a real server — `typlen` 4, `typbyval` t, measured — where
        // the range and the composite are varlena.
        vec![
            "custom_time_format|e|E|4|f".to_owned(),
            "floatrange|r|R|-1|f".to_owned(),
            "full_address|c|C|-1|t".to_owned(),
        ]
    );
    // **The array type nobody asked for.** `CREATE TYPE` makes one, so `typarray` names a row that
    // is really there — `_floatrange`, the internal spelling of `floatrange[]`.
    assert_eq!(
        rows(
            "SELECT typname, typcategory FROM pg_type WHERE typname = '_floatrange'",
            &mut node
        ),
        vec!["_floatrange|A".to_owned()]
    );
    // And it goes away with its type.
    node.run("DROP TYPE floatrange").unwrap();
    assert!(
        rows(
            "SELECT typname FROM pg_type WHERE typname IN ('floatrange','_floatrange')",
            &mut node
        )
        .is_empty()
    );
}

/// **`typinput` is the kind's function, not the type's name** — `debts-v1.1.md` #37.
///
/// This node wrote `format!("{name}_in")`, so a domain called `dom_probe` claimed a `dom_probe_in`
/// that exists on no server, and nothing pinned the column: four kinds, one wrong rule, and it is
/// the column `ActiveRecord`'s type-map load selects on **every connection**
/// (`WHERE t.typtype IN ('r', 'e', 'd')`, 712 occurrences across 164 captured files).
///
/// Measured on 19beta1, 2026-09-10, one `CREATE` per kind
/// (`tests/captures/pg19_domain_type.txt`):
///
/// ```text
/// c_probe  c  record_in     _c_probe  b  array_in
/// d_probe  d  domain_in     _d_probe  b  array_in
/// e_probe  e  enum_in       _e_probe  b  array_in
/// r_probe  r  range_in      _r_probe  b  array_in
/// ```
///
/// **A domain's is `domain_in` and not its base's**, which is the one a reader guesses wrong: the
/// *output* side is the base's — `int4out` for a domain over `integer`, measured — because reading
/// a domain checks its constraints and printing one does not. This node has no `typoutput` column,
/// so only the half that is here is answered here.
#[test]
fn typinput_is_the_kinds_function_and_not_the_types_name() {
    let mut node = parity::Node::new(&[
        "CREATE DOMAIN d_probe AS integer",
        "CREATE TYPE e_probe AS ENUM ('a','b')",
        "CREATE TYPE c_probe AS (x int, y int)",
    ]);
    assert_eq!(
        node.rows(
            "SELECT typname, typtype, typinput FROM pg_type \
             WHERE typname IN ('d_probe','e_probe','c_probe','_d_probe','_e_probe','_c_probe') \
             ORDER BY typname"
        ),
        vec![
            vec!["_c_probe".to_owned(), "b".to_owned(), "array_in".to_owned()],
            vec!["_d_probe".to_owned(), "b".to_owned(), "array_in".to_owned()],
            vec!["_e_probe".to_owned(), "b".to_owned(), "array_in".to_owned()],
            vec!["c_probe".to_owned(), "c".to_owned(), "record_in".to_owned()],
            vec!["d_probe".to_owned(), "d".to_owned(), "domain_in".to_owned()],
            vec!["e_probe".to_owned(), "e".to_owned(), "enum_in".to_owned()],
        ]
    );
}
