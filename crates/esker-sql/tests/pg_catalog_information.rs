//! Contract C3 for `information_schema` — phase 13 unit 4.
//!
//! Five views over the same records `pg_catalog` describes, and the first relations on this node
//! whose **name carries a schema**: a bare `tables` is `42P01` on a real server and stays one here.
//!
//! `key_column_usage` is the one that earns its place beyond the standard: it gives a primary key's
//! columns one row each, which is the question `primary_keys()` asks `pg_index` and cannot get an
//! answer to here without `= ANY` over an `int2vector`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // A real server declares these columns as domains — `sql_identifier` for a name,
    // `cardinal_number` for a position, `yes_or_no` for a flag and `character_data` for a
    // description — which `\gdesc` reports as `name`, `integer` and `character varying`. This node
    // has no domains, so a name is `text` and a flag is `text`; **`ordinal_position` and the four
    // lengths are `int4` on both**. Every value is identical, and the strings `YES`/`NO` are what a
    // client compares against either way.
    types: &[
        "SELECT table_name, table_type, table_schema, table_catalog FROM information_schema.tables WHERE table_name IN ('sa','sb','sc') ORDER BY table_name",
        "SELECT column_name, ordinal_position, is_nullable, data_type FROM information_schema.columns WHERE table_name = 'sa' ORDER BY ordinal_position",
        "SELECT column_name, udt_name, column_default FROM information_schema.columns WHERE table_name = 'sa' ORDER BY ordinal_position",
        "SELECT column_name, ordinal_position, is_nullable, data_type, column_default FROM information_schema.columns WHERE table_name = 'sb' ORDER BY ordinal_position",
        "SELECT column_name, is_identity, identity_generation, is_generated FROM information_schema.columns WHERE table_name = 'sb' ORDER BY ordinal_position",
        "SELECT constraint_name, constraint_type, table_name, is_deferrable, initially_deferred FROM information_schema.table_constraints WHERE table_name IN ('sa','sb','sc') ORDER BY constraint_name",
        "SELECT constraint_name, table_name, column_name, ordinal_position, position_in_unique_constraint FROM information_schema.key_column_usage WHERE table_name IN ('sa','sb','sc') ORDER BY constraint_name, ordinal_position",
        "SELECT constraint_catalog, constraint_schema, table_catalog, table_schema FROM information_schema.key_column_usage WHERE table_name = 'sb'",
        "SELECT table_name FROM information_schema.tables WHERE table_schema = 'public' AND table_name IN ('sa','sb') ORDER BY table_name",
        "SELECT c.column_name, c.data_type FROM information_schema.columns c WHERE c.table_name = 'sb' AND c.column_name = 'x'",
        "SELECT constraint_name FROM information_schema.table_constraints WHERE table_name = 'sb' AND constraint_type = 'PRIMARY KEY'",
        "SELECT constraint_name FROM information_schema.table_constraints WHERE table_name = 'sb' AND constraint_type = 'CHECK' ORDER BY constraint_name",
        "SELECT constraint_name FROM information_schema.table_constraints WHERE table_name = 'sb' AND constraint_type = 'FOREIGN KEY'",
        "SELECT constraint_name FROM information_schema.table_constraints WHERE table_name = 'sb' AND constraint_type = 'UNIQUE'",
        "SELECT column_name FROM information_schema.columns WHERE table_name = 'sc' ORDER BY ordinal_position",
        "SELECT column_name, character_maximum_length, numeric_precision, numeric_scale, datetime_precision FROM information_schema.columns WHERE table_name = 'sa' ORDER BY ordinal_position",
    ],
    answers: &[
        (
            "SELECT table_name, table_type, table_schema, table_catalog FROM information_schema.tables WHERE table_name IN ('sa','sb','sc') ORDER BY table_name",
            "**`table_catalog` is refused by name** (`42703`), where a real server answers with \
             the database it is connected to. This node has no database concept at all — there is \
             no `current_database()` and the startup parameter never reaches the executor — so \
             there is no name to report and a constant would be a value nobody measured. The same \
             answer `pg_range` gives for `oid`, and it closes the day a unit gives this node \
             databases.",
        ),
        (
            "SELECT constraint_catalog, constraint_schema, table_catalog, table_schema FROM information_schema.key_column_usage WHERE table_name = 'sb'",
            "the same, twice over: `constraint_catalog` and `table_catalog` are both the database \
             name.",
        ),
        (
            "SELECT count(*) FROM information_schema.tables",
            "**265 rows there and this tenant's tables here.** A real server's \
             `information_schema` describes `information_schema` and `pg_catalog` as well as the \
             user's own schema; this node's catalog views are computed and are not themselves \
             relations in it. The same shape `pg_class` already declares — every other statement \
             in this corpus filters by name for exactly that reason, and this one is here to say \
             what the unfiltered number is.",
        ),
        (
            "DROP TABLE information_schema.tables",
            "**a write here is refused by permission, where a real server refuses it by kind.** \
             `information_schema` is built out of *views* there, so `DROP TABLE` is `42809` with \
             `HINT: Use DROP VIEW to remove a view.`, while the same statement against \
             `pg_catalog.pg_class` is `42501 permission denied` — two neighbouring schemas, two \
             refusals. Here every catalog relation is computed and every write to one is `42501`, \
             which is one rule rather than two and is the rule `tests/pg_catalog.rs` already \
             declares for `pg_type`. Both refuse, both name the relation, and neither lets the \
             write through — and `DROP TABLE pg_catalog.pg_class` agrees exactly.",
        ),
        (
            "ALTER TABLE information_schema.tables ADD COLUMN a bigint",
            "the same, for `ALTER`.",
        ),
        (
            "CREATE INDEX ixq ON information_schema.tables (table_name)",
            "the same, for `CREATE INDEX`.",
        ),
        (
            "SELECT table_name FROM information_schema.tables WHERE table_schema = 'information_schema' AND table_name IN ('tables','columns') ORDER BY table_name",
            "the same fact asked precisely: a real server lists `information_schema`'s own \
             relations in it and this node lists none, because they are computed rather than \
             stored.",
        ),
    ],
};

#[test]
fn every_information_schema_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_information.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The schema is part of the name, and a bare one is still `42P01`.
///
/// The whole reason these five views are named `information_schema.tables` rather than `tables`:
/// answering a bare `tables` would invent a relation a real server does not have. Measured — and
/// the mirror case is `pg_catalog.pg_class`, where the qualifier names a relation that *is* there
/// and is therefore stripped.
#[test]
fn a_bare_information_schema_name_is_not_a_relation() {
    let mut node = parity::Node::new(&["CREATE TABLE qn (id int8 PRIMARY KEY)"]);

    for bare in [
        "SELECT * FROM tables",
        "SELECT * FROM columns",
        "SELECT * FROM key_column_usage",
    ] {
        let error = node.run(bare).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE, "{bare}");
    }

    // And the qualified spellings that do work, both directions.
    assert_eq!(
        node.rows("SELECT table_name FROM information_schema.tables WHERE table_name = 'qn'"),
        vec![vec!["qn"]]
    );
    assert_eq!(
        node.rows("SELECT relname FROM pg_catalog.pg_class WHERE relname = 'qn'"),
        vec![vec!["qn"]]
    );

    // A schema this node does not have is `42P01` **with the schema inside the quotes**, which is
    // what a real server answers — measured in `pg19_schema.txt`. It was `0A000` until schemas
    // existed; now the relation really is looked for, in a namespace that is not there.
    let error = node.run("SELECT * FROM other.qn").unwrap_err();
    assert_eq!(error.sqlstate(), sqlstate::UNDEFINED_TABLE);
    assert_eq!(error.to_string(), "relation \"other.qn\" does not exist");
}

/// `key_column_usage` answers what `primary_keys()` asks and `pg_index` cannot.
///
/// `primary_keys()` writes `a.attnum = ANY(i.indkey)` over an `int2vector`, which needs an array
/// value this node has no type for. The same question has a row-shaped answer here, and it is the
/// one a client can use today: one row per key column, in key order.
#[test]
fn a_primary_key_s_columns_are_one_row_each() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE kk (k1 int8, k2 text, v int4, PRIMARY KEY (k1, k2))",
        "CREATE TABLE kn (a int4, b text)",
    ]);

    assert_eq!(
        node.rows(
            "SELECT column_name, ordinal_position FROM information_schema.key_column_usage \
             WHERE table_name = 'kk' ORDER BY ordinal_position"
        ),
        vec![vec!["k1", "1"], vec!["k2", "2"]]
    );
    // A table with no declared key has no rows here — its internal row id is not a primary key
    // and must not be reported as one.
    assert!(
        node.rows(
            "SELECT column_name FROM information_schema.key_column_usage WHERE table_name = 'kn'"
        )
        .is_empty()
    );
}
