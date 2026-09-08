//! **`ALTER COLUMN … TYPE … USING <expression>` — the expression drives the rewrite.**
//!
//! `array_test.rb`'s `test_change_column_from_non_array_to_array` sends
//!
//! ```text
//! ALTER TABLE "pg_arrays" ALTER COLUMN "snippets" TYPE text[]
//!   USING string_to_array("snippets", ',')
//!     ALTER TABLE ... ALTER COLUMN ... TYPE ... USING ... is not supported
//! ```
//!
//! Half of `USING` was already here: `USING <column>::<type>` lowered to the *type* it casts to,
//! which is what the pre-flight check reads — a cast's target is knowable before a row is touched.
//! The general form is not that shape, and it is the one PostgreSQL documents: any expression over
//! the old row, evaluated once per row.
//!
//! # Measured on 19beta1
//!
//! ```text
//! ALTER TABLE g1u_t ALTER COLUMN snippets TYPE text[] USING string_to_array(snippets, ',')
//!
//!  id | snippets |          format_type
//!   1 | {a,b,c}  |          text[]
//!   2 | {}        <- '' becomes the EMPTY array, not one empty element
//!   3 |           <- NULL stays NULL
//!   4 | {single}
//!
//! ALTER COLUMN a TYPE text USING b || '-' || a::text     -> 'seven-7'
//!   -- a USING may name any column of the row, not only the one being retyped
//!
//! ALTER COLUMN a TYPE text[]        (no USING)
//!   42804 column "a" cannot be cast automatically to type text[]
//!   HINT: You might need to specify "USING a::text[]".
//! ```
//!
//! and `string_to_array` itself, whose edges are the reason it was measured rather than written
//! from memory:
//!
//! ```text
//! ('a,b,c', ',')    {a,b,c}     ('', ',')      {}          -- empty, not one empty element
//! ('single', ',')   {single}    (NULL, ',')    NULL
//! ('abc', '')       {abc}       -- an empty delimiter does not split at all
//! ('a,b', NULL)     {a,",",b}   -- a NULL delimiter splits into single characters
//! ('a,,b', ',')     {a,"",b}    -- an empty field is kept
//! ('axxbxxc', 'xx') {a,b,c}     -- the delimiter is a string, not a character
//! ('a,b,NULL', ',', 'NULL')     {a,b,NULL}  -- the third argument names the NULL text
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE g1u_t (id int8 PRIMARY KEY, snippets character varying)",
    "INSERT INTO g1u_t VALUES (1, 'a,b,c'), (2, ''), (3, NULL), (4, 'single')",
];

/// **The `ActiveRecord` statement**, and the four values it has to move.
#[test]
fn the_using_expression_rewrites_every_row() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(
        "ALTER TABLE \"g1u_t\" ALTER COLUMN \"snippets\" TYPE text[] \
         USING string_to_array(\"snippets\", ',')",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT id, snippets FROM g1u_t ORDER BY id"),
        [
            ["1".to_owned(), "{a,b,c}".to_owned()],
            // The empty string becomes the **empty array**, which is the edge a per-value cast
            // would have got wrong in the other direction.
            ["2".to_owned(), "{}".to_owned()],
            ["3".to_owned(), "\\N".to_owned()],
            ["4".to_owned(), "{single}".to_owned()],
        ]
    );
    assert_eq!(
        node.rows(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'g1u_t'::regclass AND attname = 'snippets'"
        ),
        [["text[]"]]
    );
}

/// **The whole row is in scope**, not only the column being retyped — measured, and the half a
/// per-value conversion cannot express.
#[test]
fn a_using_may_name_another_column() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1u_x (a integer, b text)",
        "INSERT INTO g1u_x VALUES (7, 'seven')",
    ]);
    node.run("ALTER TABLE g1u_x ALTER COLUMN a TYPE text USING b || '-' || a::text")
        .unwrap();
    assert_eq!(node.rows("SELECT a FROM g1u_x"), [["seven-7"]]);
}

/// Without a `USING`, the refusal is unchanged — the same `42804` and the same HINT, which is what
/// says the pre-flight check was skipped for the expression form and not removed.
#[test]
fn without_a_using_it_is_still_refused() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("ALTER TABLE g1u_t ALTER COLUMN snippets TYPE text[]")
            .to_string(),
        "!42804 column \"snippets\" cannot be cast automatically to type text[] \
         HINT: You might need to specify \"USING snippets::text[]\"."
    );
}

/// And the cast form still takes the two-hop check it always did.
#[test]
fn the_cast_form_still_works() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1u_c (a character varying)",
        "INSERT INTO g1u_c VALUES ('42')",
    ]);
    node.run("ALTER TABLE g1u_c ALTER COLUMN a TYPE integer USING a::integer")
        .unwrap();
    assert_eq!(node.rows("SELECT a FROM g1u_c"), [["42"]]);
}

/// **`string_to_array`, edge for edge.** Every answer here came off 19beta1 before the function was
/// written; four of them are nothing a reasonable implementation would produce unasked.
#[test]
fn string_to_array_answers_what_postgresql_answers() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT string_to_array('a,b,c', ','), string_to_array('', ','), \
             string_to_array('single', ','), string_to_array(NULL, ','), \
             string_to_array('a,b', NULL), string_to_array('abc', ''), \
             string_to_array('a,,b', ','), string_to_array('axxbxxc', 'xx'), \
             string_to_array('a,b,NULL', ',', 'NULL'), pg_typeof(string_to_array('a', ','))"
        ),
        [[
            "{a,b,c}".to_owned(),
            "{}".to_owned(),
            "{single}".to_owned(),
            "\\N".to_owned(),
            "{a,\",\",b}".to_owned(),
            "{abc}".to_owned(),
            "{a,\"\",b}".to_owned(),
            "{a,b,c}".to_owned(),
            "{a,b,NULL}".to_owned(),
            "text[]".to_owned(),
        ]]
    );
}

/// **The result takes the assignment cast into the column.** `USING length(snippets)` is an
/// `int4` result into a `bigint` column: a real server casts it as it would assign it, and this
/// node refused the row as a codec mismatch. Measured: 5, 0, NULL, 6, and the column is `bigint`.
#[test]
fn the_using_result_takes_the_assignment_cast_into_the_column() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1u_t ALTER COLUMN snippets TYPE bigint USING length(snippets)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT id, snippets FROM g1u_t ORDER BY id"),
        [["1", "5"], ["2", "0"], ["3", "\\N"], ["4", "6"]]
    );
    assert_eq!(
        node.rows(
            "SELECT format_type(atttypid, atttypmod) FROM pg_attribute \
             WHERE attrelid = 'g1u_t'::regclass AND attname = 'snippets'"
        ),
        [["bigint"]]
    );
}

/// **A result with no cast to the column is `42804`, in PostgreSQL's sentence** — not the row
/// codec's mismatch — and it carries the HINT a client is told to act on. Measured, hint included.
#[test]
fn a_using_result_with_no_cast_is_42804() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node
        .run("ALTER TABLE g1u_t ALTER COLUMN snippets TYPE integer USING snippets || 'x'")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42804", "{error}");
    assert_eq!(
        error.to_string(),
        "result of USING clause for column \"snippets\" cannot be cast automatically to type integer"
    );
    assert_eq!(
        error.hint().as_deref(),
        Some("You might need to add an explicit cast.")
    );
}

/// **A column the `USING` does not have is the user's `42703`**, not an internal error about
/// "the USING expression". Measured.
#[test]
fn a_column_the_using_does_not_have_is_42703() {
    let mut node = parity::Node::new(FIXTURE);
    let error = node
        .run("ALTER TABLE g1u_t ALTER COLUMN snippets TYPE text USING nosuch")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42703", "{error}");
    assert_eq!(error.to_string(), "column \"nosuch\" does not exist");
}
