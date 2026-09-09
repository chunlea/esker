//! `name[]` — the array type `name` never had, against PostgreSQL 19beta1.
//!
//! Run 106 lost ten tests to one alias. `ActiveRecord` reads an enum's labels with
//! `postgresql_adapter.rb:539`:
//!
//! ```sql
//! SELECT type.typname AS name, type.OID AS oid, n.nspname AS schema,
//!        array_agg(enum.enumlabel ORDER BY enum.enumsortorder) AS value
//!   FROM pg_enum AS enum JOIN pg_type AS type ON (type.oid = enum.enumtypid) …
//! ```
//!
//! `pg_enum.enumlabel` is a `name` column, so a real server declares `value` as **`name[]`, OID
//! 1003**. This node aliased `name` to `text` in every derivation and answered a *scalar* `text`,
//! OID 25 — not even `text[]`. The bytes are identical; `ActiveRecord` decodes by the declared
//! OID, so a `text` stays a Ruby string and the dump reads `create_enum "mood", "{sad,okay,happy}"`
//! where `["sad", "okay", "happy"]` is expected.
//!
//! **The third instance of one pattern** — run 98's `->` over a `json` column (OID 25 for 114),
//! run 105's enum over the extended protocol (21 for the enum's own oid), this. Right bytes, wrong
//! declared type, invisible to `psql` and to `pg_typeof`; only a `Describe` sees it. So the wire
//! assertions below go through `Executor::describe` rather than through a corpus.
//!
//! One rule runs the other way: **`min`/`max` of a `name` is a `text`**, because a real server has
//! no `min(name)` and coerces the argument. This node answered `name`.
//!
//! Measured in `tests/captures/pg19_name_array.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The same fixture `tests/name_type.rs` uses, because the corpus was captured with it.
const CORPUS_FIXTURE: &[&str] = &[
    "CREATE TABLE b4_nm (id int8, data name)",
    "INSERT INTO b4_nm VALUES (1,'plain'),(2,'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'),(3,''),(4,'MiXeD'),(5,'apple'),\
     (6,'Apple')",
];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **These differ in a declared type and nothing else.** `pg_typeof` agrees on both sides now
    // — it is resolved at plan time from the declared type (ADR 0093) — so what is left in this
    // list is the catalog's own `oid` and `regproc`, each its own unit on the type-surface queue.
    types: &[
        // `oid` and `oid[]` on a real server where this node says `bigint` and `bigint[]` — the
        // catalog-oid family, its own unit. `typname` agrees on both since ADR 0084.
        "SELECT typname AS name, oid, array_agg(oid) FROM pg_type WHERE typname = 'name' GROUP \
         BY typname, oid",
        // **Every row of this one agrees now**, which is why it is here rather than in `answers`:
        // the value that differed was `name`'s `typelem`, 0 here against 18 on a real server,
        // and it is 18 since `"char"` became a row (ADR 0095). What is left is `oid` answered as
        // a `bigint` and `typinput` as `text` — the two families still on the type-surface queue.
        "SELECT oid, typname, typlen, typtype, typcategory, typdelim, typelem, typarray, \
         typinput FROM pg_type WHERE oid IN (19, 1003) ORDER BY oid",
    ],
    answers: &[
        // ----- `pg_typeof` over a value whose type lives in the *expression* --------------------
        //
        // **These are all one fact and it is not about `name`.** `pg_typeof` reads the datum, and
        // `text`, `varchar`, `bpchar` and `name` are one `Datum::Text` — so where the type is
        // carried by the expression rather than by the value, this function cannot see it. The
        // `RowDescription` for every one of these statements is correct, which is what a client
        // reads and what `tests/name_array.rs`'s wire assertions check; `pg_typeof` is the one
        // caller that asks the value instead. Closing it means resolving `pg_typeof` against the
        // declared type at plan time, where a scope exists — its own unit, and it would close the
        // `regtype`/`text` half (ADR 0077) at the same time.
        (
            "SELECT pg_typeof('{a,b}'::name[] || 'c'::name)",
            "The same, through the array `||`.",
            "pg19_name_array.txt:112",
        ),
        // ----- two array functions this node does not have ---------------------------------------
        (
            "SELECT array_dims('{a,b}'::name[]), array_length('{a,b}'::name[], 1), \
             cardinality('{a,b}'::name[])",
            "`array_dims` is `0A000` by name — more of the array function surface, and the \
             refusal `tests/array.rs` already records for it. `array_length` and `cardinality` \
             answer.",
            "pg19_name_array.txt:104",
        ),
        (
            "SELECT array_to_string('{a,b}'::name[], '-')",
            "`array_to_string` is `0A000` by name, the same named gap.",
            "pg19_name_array.txt:107",
        ),
        // **A domain over `name`, and this node has no domain there.**
        // `information_schema.tables.table_name` is the `sql_identifier` domain on a real server,
        // and an array of a domain is an array of that domain — its own oid. This node declares
        // the column `name` and answers `name[]`: the element type right and the domain missing.
        (
            "SELECT pg_typeof(array_agg(table_name)) FROM (SELECT table_name FROM \
             information_schema.tables LIMIT 2) s",
            "`information_schema.tables.table_name` is the `sql_identifier` domain over `name` on \
             a real server, so an aggregate over it is `information_schema.sql_identifier[]`. \
             This node has no domain there and answers `name[]` — the element type right and the \
             domain absent.",
            "pg19_name_array.txt:98",
        ),
        // `SELECT n.nspname … = ANY (current_schemas(false))` is **not** listed, and it was
        // expected to be: the two servers hold different catalogs, so the rows looked like they
        // could not agree. Both answer `public` and the column is a `name` on both, so the
        // harness deleted the entry for us — which is the second ratchet rule paying for itself
        // on the one statement in this file that is the enum query's own predicate.
    ],
};

#[test]
fn every_name_array_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_name_array.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 38,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The declared type a client is told, **through a `Describe`** — the only path that sees it.
fn described(node: &mut parity::Node, statement: &str) -> Vec<(String, u32)> {
    node.describe(statement)
        .unwrap()
        .fields
        .expect("a SELECT returns rows")
        .into_iter()
        .map(|field| (field.name, field.type_oid))
        .collect()
}

/// **The statement the suite sends**, described the way `ActiveRecord` reads it.
#[test]
fn the_enum_labels_query_declares_a_name_array() {
    let mut node = parity::Node::new(&[
        "CREATE TYPE mood AS ENUM ('sad', 'okay', 'happy')",
        "CREATE TABLE m (id int8, current_mood mood)",
    ]);
    let fields = described(
        &mut node,
        "SELECT type.typname AS name, type.OID AS oid, n.nspname AS schema, \
         array_agg(enum.enumlabel ORDER BY enum.enumsortorder) AS value \
         FROM pg_enum AS enum JOIN pg_type AS type ON (type.oid = enum.enumtypid) \
         JOIN pg_namespace n ON type.typnamespace = n.oid \
         WHERE n.nspname = ANY (current_schemas(false)) \
         GROUP BY type.OID, n.nspname, type.typname",
    );
    // **1003 is `_name`.** Told 25 instead, `ActiveRecord` keeps the literal as a string and dumps
    // `create_enum "mood", "{sad,okay,happy}"` — the ten failures of run 106.
    assert_eq!(fields[3], ("value".to_owned(), 1003));
    // The rest of the row, which was already right and must stay so: `typname` and `nspname` are
    // `name` columns (19) since ADR 0084 and the oid is a `bigint` here.
    assert_eq!(fields[0].1, 19);
    assert_eq!(fields[2].1, 19);
    // And the labels themselves, in `enumsortorder`, which is declaration order and not the
    // alphabet (ADR 0050).
    assert_eq!(
        node.rows(
            "SELECT array_agg(enum.enumlabel ORDER BY enum.enumsortorder) FROM pg_enum AS enum \
             JOIN pg_type AS type ON (type.oid = enum.enumtypid) WHERE type.typname = 'mood'"
        ),
        vec![vec!["{sad,okay,happy}"]]
    );
}

/// Every derivation that answers `name[]` on a real server, over the wire.
#[test]
fn every_aggregate_over_a_name_declares_a_name_array() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    for statement in [
        "SELECT array_agg(c::name) AS v FROM (VALUES ('x')) s(c)",
        "SELECT ARRAY[c::name] AS v FROM (VALUES ('x')) s(c)",
        "SELECT ARRAY['a'::name,'b'::name] AS v",
        "SELECT array_agg(relname) AS v FROM (SELECT relname FROM pg_class LIMIT 2) s",
        "SELECT array_agg(attname) AS v FROM (SELECT attname FROM pg_attribute LIMIT 2) s",
        "SELECT array_agg(data) AS v FROM b4_nm",
        "SELECT '{a,b}'::name[] AS v",
        "SELECT current_schemas(false) AS v",
    ] {
        assert_eq!(
            described(&mut node, statement)[0].1,
            1003,
            "{statement} is not declared name[]"
        );
    }
}

/// **`min` and `max` of a `name` are a `text`**, which is the one rule that runs the other way.
#[test]
fn min_and_max_of_a_name_are_text() {
    let mut node = parity::Node::new(CORPUS_FIXTURE);
    for statement in [
        "SELECT min(c::name) AS v FROM (VALUES ('x')) s(c)",
        "SELECT max(c::name) AS v FROM (VALUES ('x')) s(c)",
        "SELECT min(data) AS v FROM b4_nm",
    ] {
        assert_eq!(
            described(&mut node, statement)[0].1,
            25,
            "{statement} is not declared text"
        );
    }
    // The values are the same either way, and the ordering is still byte order (ADR 0076).
    assert_eq!(
        node.rows("SELECT min(c::name), max(c::name) FROM (VALUES ('b'),('a')) s(c)"),
        vec![vec!["a", "b"]]
    );
}

/// A `name[]` is a value this node holds, not only a type it names.
#[test]
fn a_name_array_round_trips() {
    let mut node = parity::Node::new(&["CREATE TABLE na (id int8, tags name[])"]);
    node.run("INSERT INTO na VALUES (1, '{alpha,beta}')")
        .unwrap();
    assert_eq!(node.rows("SELECT tags FROM na"), vec![vec!["{alpha,beta}"]]);
    assert_eq!(described(&mut node, "SELECT tags FROM na")[0].1, 1003);
    // Its elements are `name`s, so they truncate at 63 bytes like any other (ADR 0084).
    assert_eq!(
        node.rows("SELECT unnest('{a,b}'::name[])"),
        vec![vec!["a"], vec!["b"]]
    );
    assert_eq!(
        described(&mut node, "SELECT unnest('{a,b}'::name[]) AS v")[0].1,
        19
    );
}

/// **The catalog carries the row**, and its delimiter is a comma like every array but `_box`'s.
#[test]
fn pg_type_has_the_array_row() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT oid, typname, typlen, typtype, typcategory, typdelim, typelem, typarray, \
             typinput FROM pg_type WHERE oid IN (19, 1003) ORDER BY oid"
        ),
        vec![
            // **`typelem` is 18**, which says a `name` is 64 `"char"`s — and it says so because
            // that row is there (ADR 0095). It was a zero for exactly as long as the type was
            // missing, a pointer to nothing being worse than no pointer, which is the same call
            // `box`'s `typelem` makes and why `tests/array_delimiter.rs` tells an array from a
            // base type by `typinput` rather than by `typelem`.
            vec!["19", "name", "64", "b", "S", ",", "18", "1003", "namein"],
            vec!["1003", "_name", "-1", "b", "A", ",", "19", "0", "array_in"],
        ]
    );
}

/// A scalar `text` compares against a `name[]`'s elements; two whole arrays do not.
#[test]
fn the_comparisons_are_postgresqls() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 'x'::name = ANY('{x,y}'::name[]), 'x'::text = ANY('{x,y}'::name[])"),
        vec![vec!["t", "t"]]
    );
    let error = node
        .run("SELECT '{a,b}'::name[] = '{a,b}'::text[]")
        .unwrap_err();
    assert_eq!(error.sqlstate(), esker_sql::sqlstate::UNDEFINED_FUNCTION);
    assert_eq!(
        error.to_string(),
        "operator does not exist: name[] = text[]"
    );
}
