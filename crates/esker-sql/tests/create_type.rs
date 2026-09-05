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
        vec![
            "custom_time_format|e|E|-1|f".to_owned(),
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
