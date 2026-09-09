//! A column of `xml`, against PostgreSQL 19beta1.
//!
//! Run 74's row: 5 tests in `adapters/postgresql/xml_test.rb`, one column, `t.xml "payload"`.
//! The file itself is small; what the corpus is mostly about is the *shape* of the type — a
//! string with no equality operator at all, which is `json`'s shape and reaches five different
//! refusals for it — `=`, `DISTINCT`, `ORDER BY`, `CREATE INDEX` and `min`, each its own
//! sentence.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus creates its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog type trade: `typname` and `udt_name` are `name` on a real server and
    // `data_type` is `information_schema`'s own domain, all `text` here, all comparing
    // identically. **Every value agrees** — `xml` is oid 142 with `typarray` 143, `typlen` -1,
    // `typcategory` `U` and `typinput` `xml_in`, and the column reports `xml` in all three views.
    types: &[
        "SELECT 'r', typname, oid, typarray, typlen, typcategory, typinput, typtype, typdelim \
         FROM pg_type WHERE typname IN ('xml','_xml') ORDER BY typname",
        "SELECT 'r', '{\"<a/>\"}'::xml[], pg_typeof('{\"<a/>\"}'::xml[])",
        // **A cast's declared type, and one reason for all nine**: an `xml` value is a
        // `Datum::Text`, as a `json` value is, so `RowDescription` carries `text`'s oid where a
        // real server carries 142. The **rows are right** in every one of them, and a *column* of
        // `xml` reports correctly — its type comes from the catalog rather than from its values,
        // which the `format_type` and `information_schema` statements above check. It is only a
        // bare literal or cast that loses it. `tests/json.rs` carries the identical list for the
        // identical reason; a `Datum` variant of its own closes both at once.
    ],
    answers: &[
        // **`pg_typeof` reads the value, not the plan**, which is [`esker_sql::plan::CatalogFunc`]
        // `PgTypeof`'s stated design, and an `xml` is a `Datum::Text` exactly as a `json` is. The
        // *column* is an `xml` everywhere it is asked — `pg_type`, `information_schema` and
        // `format_type` all say so three statements above — and it is the value that carries no
        // type of its own. The same trade `varchar` makes, arriving one type later.
        (
            "SELECT 'r', pg_typeof(payload) FROM xml_data_type ORDER BY id LIMIT 1",
            "pg_typeof reads the value, and an xml value is a Datum::Text",
            "UNMEASURED",
        ),
        // **An `E'…'` literal is not lowered here at all**, whatever it is cast to: the parser
        // gives it as an `EscapedStringLiteral` and nothing in this crate reads one, so the cast
        // is `0A000` before the text is looked at. Nothing to do with `xml` — these two
        // statements are here because a *multi-line* document is the only way to ask whether the
        // two line numbers in a refusal are one counter or two, and they are not:
        // `esker_sql::value::xml`'s unit test proves this node gets both right, by calling the
        // validator directly. The day `E'…'` lowers, these two lines agree and rule 2 deletes
        // them.
        (
            "SELECT 'r', E'<a>\\n</b>'::xml",
            "an E'…' literal is not lowered here, whatever the cast",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', E'<a>\\n<b>\\n'::xml",
            "an E'…' literal is not lowered here, whatever the cast",
            "UNMEASURED",
        ),
        // **The declaration is dropped on the way in here and on the way out there.** A real
        // server keeps it in the value and strips it in `xml_out`, so `('<?xml …?><a/>'::xml)`
        // prints `<a/>` — which this node agrees with, two statements above — and
        // `(…)::text` is the 25 characters that were written, where this node has 4.
        //
        // The alternative was stripping in the output function, which would need every path that
        // turns a row into bytes to know the column's declared type; the value is a
        // `Datum::Text`, as `json`'s is, and carries no type of its own. One divergence on the
        // `::text` path, which no client reads, buys a node where every path agrees with every
        // other — [`esker_sql::value::xml`] carries the whole argument.
        (
            "SELECT 'r', length(('<?xml version=\"1.0\"?><a/>'::xml)::text)",
            "the declaration is dropped on input here and on output there",
            "UNMEASURED",
        ),
        // **`array_agg` over an `xml` needs an `xml[]` to build**, and this node's aggregate
        // vocabulary has no array of one. The scalar type is complete; the aggregate is the gap,
        // and it is the same one `array_agg(point)` has.
    ],
};

#[test]
fn every_xml_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_xml.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 70,
        "only {checked} statements ran; the corpus did not load"
    );
}
