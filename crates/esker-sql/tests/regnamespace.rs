//! `regnamespace` — an oid that prints as a schema, the second of ADR 0098's kind — held to
//! PostgreSQL 19's answers ([ADR 0115]).
//!
//! `corpus/pg19_regnamespace.txt` is the capture, in three sessions: the value — input, output, the
//! catalog columns that hold a namespace oid, and the census's own `UPDATE` shape — the type — its
//! `pg_type` and `pg_cast` rows, digits, aggregates and an assignment — and a value stored in a
//! table, read back after its schema is renamed and after it is dropped. The replay is the test of
//! record; the tests after it pin the rule each group of rows is an instance of.
//!
//! **Names, not bootstrap oids**, in every test below: this node numbers `public` 11 and every other
//! schema by its record id, where PostgreSQL's `public` is 2200 — so a test that asserted an oid would
//! be asserting a numbering, and the replay declares the rows that print one.
//!
//! [ADR 0115]: ../../../docs/adr/0115-regnamespace-is-an-oid-that-prints-as-a-schema.md

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// PostgreSQL's bootstrap oids against this node's own numbering — the reason most rows below differ.
const NUMBERING: &str = "numbering, not the type: PostgreSQL's `pg_catalog` is oid 11 and its \
                        `public` 2200, and this node's `public` is 11 and `pg_catalog` 12 — the \
                        numbers `pg_namespace.oid`, `relnamespace` and `connamespace` carry here, \
                        which `::regnamespace` agrees with (ADR 0115). Every row that compares or \
                        prints a name agrees";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 'r', 'pg_catalog'::regnamespace::oid, 'public'::regnamespace::oid",
            NUMBERING,
            "pg19_regnamespace.txt:45",
        ),
        (
            "SELECT 'r', 'pg_catalog'::regnamespace < 'public'::regnamespace",
            NUMBERING,
            "pg19_regnamespace.txt:71",
        ),
        (
            "SELECT 'r', oid, typname, typlen, typbyval, typcategory, typinput::text, \
             typoutput::text, typreceive::text, typsend::text, typelem, typarray FROM pg_type \
             WHERE typname IN ('regnamespace', '_regnamespace') ORDER BY oid",
            "this node's `pg_type` has no `typbyval`, `typoutput`, `typreceive` or `typsend` \
             column, so the widest query is `42703`; its savepoint keeps the rest, and the \
             narrower query after it (line 96) agrees on both rows",
            "pg19_regnamespace.txt:94",
        ),
        (
            "SELECT 'r', oid, proname FROM pg_proc WHERE proname IN ('regnamespacein', \
             'regnamespaceout', 'regnamespacerecv', 'regnamespacesend', 'to_regnamespace') ORDER \
             BY oid",
            "this node's `pg_proc` has no row for the type's four I/O functions or for \
             `to_regnamespace`: `typinput` prints `regnamespacein` because `regproc`'s table knows \
             the name (line 96 agrees), and `to_regnamespace` is called by name \
             (`a_missing_schema_and_a_malformed_name_are_two_refusals`) without being listed as a \
             function",
            "pg19_regnamespace.txt:97",
        ),
        (
            "SELECT 'r', castsource::regtype::text, casttarget::regtype::text, castfunc, \
             castcontext, castmethod FROM pg_cast WHERE castsource = 'regnamespace'::regtype OR \
             casttarget = 'regnamespace'::regtype ORDER BY 2, 3",
            "this node's `pg_cast` has no `castfunc` column, so the query is `42703` in its \
             savepoint; the seven rows' sources, targets, contexts and methods are PostgreSQL's \
             (`pg_cast_has_the_regnamespace_rows`)",
            "pg19_regnamespace.txt:99",
        ),
        (
            "SELECT 'r', '11'::regnamespace::text, '2200'::regnamespace::text, \
             '99999'::regnamespace::text",
            NUMBERING,
            "pg19_regnamespace.txt:102",
        ),
        (
            "SELECT 'r', 11::regnamespace::text, 2200::oid::regnamespace::text",
            NUMBERING,
            "pg19_regnamespace.txt:104",
        ),
        (
            "SELECT 'r', pg_typeof(min(ns)), pg_typeof(max(ns)), min(ns)::text = \
             'pg_catalog'::regnamespace::oid::text FROM s2ns2.holder",
            NUMBERING,
            "pg19_regnamespace.txt:117",
        ),
        (
            "SELECT 'r', ns FROM s2ns2.holder WHERE ns = 11",
            NUMBERING,
            "pg19_regnamespace.txt:121",
        ),
    ],
};

#[test]
fn every_regnamespace_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_regnamespace.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 60,
        "only {checked} statements ran; the corpus did not load"
    );
}

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE SCHEMA s2ns",
        "CREATE SCHEMA \"S2 Mixed\"",
        "CREATE TABLE s2ns.holder (ns regnamespace)",
        "INSERT INTO s2ns.holder VALUES ('s2ns'), ('public'), ('pg_catalog')",
    ])
}

/// **A name in, a name out, folded like an identifier**: `'PUBLIC'` is `public`, a quoted
/// mixed-case name prints back quoted, and the type says what it is.
#[test]
fn a_name_prints_back_and_folds_like_an_identifier() {
    let mut node = node();
    assert_eq!(
        node.rows("SELECT 'public'::regnamespace::text, pg_typeof('public'::regnamespace)::text"),
        [["public", "regnamespace"]]
    );
    assert_eq!(
        node.rows("SELECT 'PUBLIC'::regnamespace::text, '\"S2 Mixed\"'::regnamespace::text"),
        [["public", "\"S2 Mixed\""]]
    );
}

/// **A name no schema has is `3F000`, and a string that is not one name is `42602`** — two
/// refusals for two mistakes — and `to_regnamespace` answers `NULL` for the first.
#[test]
fn a_missing_schema_and_a_malformed_name_are_two_refusals() {
    let mut node = node();
    assert_eq!(
        node.answer("SELECT 'nosuch'::regnamespace").to_string(),
        "!3F000 schema \"nosuch\" does not exist"
    );
    for malformed in ["'S2 Mixed'", "'a.b'", "''"] {
        assert_eq!(
            node.answer(&format!("SELECT {malformed}::regnamespace"))
                .to_string(),
            "!42602 invalid name syntax",
            "{malformed}"
        );
    }
    assert_eq!(
        node.rows("SELECT to_regnamespace('nosuch') IS NULL, to_regnamespace('public')::text"),
        [["t", "public"]]
    );
}

/// **An oid prints as the schema that has it**, an oid no schema has as its digits, and `0` as `-`.
#[test]
fn an_oid_prints_as_the_schema_that_has_it() {
    let mut node = node();
    assert_eq!(
        node.rows(
            "SELECT (SELECT oid FROM pg_namespace WHERE nspname = 's2ns')::regnamespace::text"
        ),
        [["s2ns"]]
    );
    assert_eq!(
        node.rows("SELECT 99999::regnamespace::text, 0::regnamespace::text"),
        [["99999", "-"]]
    );
}

/// **An untyped literal beside one is read as an `oid`**, because the comparison is `oid`'s:
/// `ns = 'public'` is `22P02`, and `ns = 'public'::regnamespace` is the spelling that works.
#[test]
fn an_untyped_literal_beside_one_is_read_as_an_oid() {
    let mut node = node();
    assert_eq!(
        node.answer("SELECT ns FROM s2ns.holder WHERE ns = 'public'")
            .to_string(),
        "!22P02 invalid input syntax for type oid: \"public\""
    );
    assert_eq!(
        node.rows("SELECT ns::text FROM s2ns.holder WHERE ns = 'public'::regnamespace"),
        [["public"]]
    );
}

/// **An assignment reads a bare literal as a name**, where a comparison reads it as an oid —
/// ADR 0098's rule, measured again for this type.
#[test]
fn an_assignment_reads_a_bare_literal_as_a_name() {
    let mut node = node();
    node.run("UPDATE s2ns.holder SET ns = 's2ns' WHERE ns = 'public'::regnamespace")
        .unwrap();
    assert_eq!(
        node.rows("SELECT ns::text FROM s2ns.holder ORDER BY 1"),
        [["pg_catalog"], ["s2ns"], ["s2ns"]]
    );
}

/// **`min` and `max` decay to `oid`**, and an array of them keeps the type.
#[test]
fn the_aggregates_decay_to_an_oid_and_the_array_keeps_the_type() {
    let mut node = node();
    assert_eq!(
        node.rows("SELECT pg_typeof(min(ns))::text, pg_typeof(max(ns))::text FROM s2ns.holder"),
        [["oid", "oid"]]
    );
    assert_eq!(
        node.rows("SELECT pg_typeof(ARRAY[ns])::text FROM s2ns.holder LIMIT 1"),
        [["regnamespace[]"]]
    );
}

/// **`pg_type` has the two rows**, under PostgreSQL's own oids — 4089, and its array 4090.
#[test]
fn pg_type_has_the_regnamespace_rows() {
    let mut node = node();
    assert_eq!(
        node.rows(
            "SELECT oid, typname, typlen, typcategory, typelem, typarray FROM pg_type WHERE typname \
             IN ('regnamespace', '_regnamespace') ORDER BY oid"
        ),
        [
            ["4089", "regnamespace", "4", "N", "0", "4090"],
            ["4090", "_regnamespace", "-1", "A", "4089", "0"]
        ]
    );
}

/// **The census's `UPDATE`, with its schema predicate in**: the constraint is selected by
/// `connamespace::regnamespace`, and `VALIDATE CONSTRAINT` then validates what it marked.
#[test]
fn the_census_update_selects_its_constraint_by_schema() {
    let mut node = node();
    node.run("CREATE TABLE s2ns.p (id integer PRIMARY KEY)")
        .unwrap();
    node.run("CREATE TABLE s2ns.c (id integer PRIMARY KEY, p integer REFERENCES s2ns.p)")
        .unwrap();
    node.run(
        "UPDATE pg_catalog.pg_constraint SET convalidated=false WHERE conname = 'c_p_fkey' AND \
         connamespace::regnamespace = 's2ns'::regnamespace",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT convalidated FROM pg_constraint WHERE conname = 'c_p_fkey'"),
        [["f"]]
    );
    node.run("ALTER TABLE s2ns.c VALIDATE CONSTRAINT c_p_fkey")
        .unwrap();
    assert_eq!(
        node.rows("SELECT convalidated FROM pg_constraint WHERE conname = 'c_p_fkey'"),
        [["t"]]
    );
}

/// `check_all_foreign_keys_valid!`'s block, **verbatim as Rails sends it** (`referential_integrity.rb:41`,
/// `docs/plans/plpgsql-subset.md` §2.1 D3) — the schema predicate included, which is the half B6 had
/// to leave out.
const CENSUS_BLOCK: &str = "do $$ declare r record; BEGIN FOR r IN (SELECT FORMAT('UPDATE \
                            pg_catalog.pg_constraint SET convalidated=false WHERE conname = ''%1$I'' \
                            AND connamespace::regnamespace = ''%2$I''::regnamespace; ALTER TABLE \
                            %2$I.%3$I VALIDATE CONSTRAINT %1$I;', constraint_name, table_schema, \
                            table_name) AS constraint_check FROM information_schema.table_constraints \
                            WHERE constraint_type = 'FOREIGN KEY') LOOP EXECUTE (r.constraint_check); \
                            END LOOP; END; $$;";

/// **`fixtures_test.rb:900` and `:926`**: clean, the block answers `DO`; with a row that slipped in
/// under `DISABLE TRIGGER ALL`, PostgreSQL's `23503` from inside the `EXECUTE` — measured
/// (`esker-coord/s2-fk3.out`).
#[test]
fn the_census_block_runs_in_the_shape_rails_sends() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE p (id integer PRIMARY KEY)",
        "CREATE TABLE c (id integer PRIMARY KEY, p integer REFERENCES p)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1, 1)",
    ]);
    assert_eq!(
        node.answer(CENSUS_BLOCK).to_string(),
        "(a command, no result set)"
    );
    node.run("ALTER TABLE c DISABLE TRIGGER ALL").unwrap();
    node.run("INSERT INTO c VALUES (2, 99)").unwrap();
    node.run("ALTER TABLE c ENABLE TRIGGER ALL").unwrap();
    assert_eq!(
        node.answer(CENSUS_BLOCK).to_string(),
        "!23503 insert or update on table \"c\" violates foreign key constraint \"c_p_fkey\" \
         DETAIL: Key (p)=(99) is not present in table \"p\"."
    );
}

/// **`referential_integrity_test.rb:106`**: foreign keys in two schemas, each selected by its own
/// schema, and the block answers `DO` — measured (`esker-coord/s2-fk3.out`).
#[test]
fn the_census_block_reaches_foreign_keys_in_two_schemas() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA s2ns",
        "CREATE TABLE p (id integer PRIMARY KEY)",
        "CREATE TABLE c (id integer PRIMARY KEY, p integer REFERENCES p)",
        "CREATE TABLE s2ns.p (id integer PRIMARY KEY)",
        "CREATE TABLE s2ns.c (id integer PRIMARY KEY, p integer REFERENCES s2ns.p)",
        "INSERT INTO p VALUES (1)",
        "INSERT INTO c VALUES (1, 1)",
        "INSERT INTO s2ns.p VALUES (1)",
        "INSERT INTO s2ns.c VALUES (1, 1)",
    ]);
    assert_eq!(
        node.answer(CENSUS_BLOCK).to_string(),
        "(a command, no result set)"
    );
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_constraint WHERE contype = 'f' AND convalidated"),
        [["2"]]
    );
}

/// **`pg_cast` has the seven rows**, with PostgreSQL's contexts and methods — measured by the
/// corpus's `pg_cast` query, whose `castfunc` column this node's view does not have.
#[test]
fn pg_cast_has_the_regnamespace_rows() {
    let mut node = node();
    assert_eq!(
        node.rows(
            "SELECT castsource::regtype::text, casttarget::regtype::text, castcontext, castmethod \
             FROM pg_cast WHERE castsource = 'regnamespace'::regtype OR casttarget = \
             'regnamespace'::regtype ORDER BY 1, 2"
        ),
        [
            ["bigint", "regnamespace", "i", "f"],
            ["integer", "regnamespace", "i", "b"],
            ["oid", "regnamespace", "i", "b"],
            ["regnamespace", "bigint", "a", "f"],
            ["regnamespace", "integer", "a", "b"],
            ["regnamespace", "oid", "i", "b"],
            ["smallint", "regnamespace", "i", "f"]
        ]
    );
}

/// **A stored value is its number, not the name it was written with**: after `ALTER SCHEMA …
/// RENAME TO` it prints the schema's new name, and once the schema is dropped its digits — the
/// corpus's third session, and the reason a row holds four bytes and no name (ADR 0115).
#[test]
fn a_stored_value_follows_its_schema_through_a_rename_and_a_drop() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA s2ns4",
        "CREATE TABLE s2ns4_holder (ns regnamespace)",
        "INSERT INTO s2ns4_holder VALUES ('s2ns4')",
    ]);
    node.run("ALTER SCHEMA s2ns4 RENAME TO s2ns4b").unwrap();
    assert_eq!(node.rows("SELECT ns::text FROM s2ns4_holder"), [["s2ns4b"]]);
    node.run("DROP SCHEMA s2ns4b").unwrap();
    assert_eq!(
        node.rows("SELECT ns::text = ns::oid::text FROM s2ns4_holder"),
        [["t"]]
    );
}

/// **Not an index key**, like `regproc` and `regtype`: PostgreSQL 19 builds a primary key and an
/// index over one (`esker-coord/s2-d92c.out`), and this node refuses both by name rather than build
/// a key its row codec cannot encode. ADR 0115 declares it.
#[test]
fn a_regnamespace_column_is_not_an_index_key() {
    let mut node = parity::Node::new(&["CREATE TABLE ix (ns regnamespace)"]);
    assert_eq!(
        node.answer("CREATE INDEX ix_ns ON ix (ns)").to_string(),
        "!0A000 an index on a column of type regnamespace is not supported"
    );
    assert_eq!(
        node.answer("CREATE TABLE pk (ns regnamespace PRIMARY KEY)")
            .to_string(),
        "!0A000 a primary key on a column of type regnamespace is not supported"
    );
    assert_eq!(
        node.answer("CREATE TABLE uq (ns regnamespace UNIQUE)")
            .to_string(),
        "!0A000 a unique constraint on a column of type regnamespace is not supported"
    );
    assert_eq!(
        node.answer("ALTER TABLE ix ADD PRIMARY KEY (ns)")
            .to_string(),
        "!0A000 a primary key on a column of type regnamespace is not supported"
    );
}
