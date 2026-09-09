//! Contract C3 for `pg_attribute` and `pg_attrdef` — phase 13 unit 1b.
//!
//! The rows behind `format_type`: `columns()` asks these two what a table's columns are, and the
//! answer is computed from the column definitions rather than stored, like every other relation in
//! `pg_catalog` here.
//!
//! The corpus builds the fixture in `docs/plans/phase-13-catalog.md` §5 — fourteen types, a
//! sequence-backed key, both identity kinds, a unique index, a multi-column index, and a table with
//! no key, no `NOT NULL` and no default — and puts the same statements to both servers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::sqlstate;

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // A real server's `attname` is a `name`, `attidentity` and `attgenerated` are `"char"`, and
    // `attrelid`, `atttypid` and `attcollation` are `oid`s. This node has none of those three
    // types, so they are `text` and `bigint` — the trade `pg_class` and `pg_type` already make
    // (`tests/pg_catalog.rs`). **`attnum` is an `int2` and `atttypmod` an `int4` on both**, which
    // is two fewer than `pg_class` needed.
    types: &[
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
        "SELECT attname, attcollation FROM pg_attribute WHERE attrelid = 'cb'::regclass AND attnum IN (1, 3) ORDER BY attnum",
        "SELECT typname, typcollation FROM pg_type WHERE typname IN ('int8', 'text') ORDER BY typname",
        "SELECT attname, attnum, attnotnull, atthasdef, atttypmod, attisdropped, attidentity, attgenerated FROM pg_attribute WHERE attrelid = 'ca'::regclass AND attnum > 0 ORDER BY attnum",
        "SELECT attname, atttypid, format_type(atttypid, atttypmod) FROM pg_attribute WHERE attrelid = 'ca'::regclass AND attnum > 0 ORDER BY attnum",
        "SELECT attname, attnum, attnotnull, atthasdef, atttypmod, attidentity FROM pg_attribute WHERE attrelid = 'cb'::regclass AND attnum > 0 ORDER BY attnum",
        "SELECT attname, attnum, attnotnull, atthasdef, attidentity, attgenerated FROM pg_attribute WHERE attrelid = 'cd'::regclass AND attnum > 0 ORDER BY attnum",
        "SELECT attname, attnum, atttypid, atttypmod FROM pg_attribute WHERE attrelid = 'cb_x_idx'::regclass AND attnum > 0 ORDER BY attnum",
        "SELECT attname, attnum, atttypid, atttypmod FROM pg_attribute WHERE attrelid = 'cb_yz_idx'::regclass AND attnum > 0 ORDER BY attnum",
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), pg_get_expr(d.adbin, d.adrelid), a.attnotnull, a.atttypid, a.atttypmod FROM pg_attribute a LEFT JOIN pg_attrdef d ON a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE a.attrelid = '\"cb\"'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        "SELECT a.attname, format_type(a.atttypid, a.atttypmod), pg_get_expr(d.adbin, d.adrelid), a.attnotnull, attidentity, attgenerated FROM pg_attribute a LEFT JOIN pg_attrdef d ON a.attrelid = d.adrelid AND a.attnum = d.adnum WHERE a.attrelid = '\"cd\"'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        "SELECT attname, attcollation FROM pg_attribute WHERE attrelid = 'cb'::regclass AND attnum IN (1, 3) ORDER BY attnum",
        "SELECT typname, typcollation FROM pg_type WHERE typname IN ('int8', 'text') ORDER BY typname",
    ],
    answers: &[(
        "SELECT count(*) FROM pg_collation",
        "**`pg_collation` is empty**, where a real server has 880 rows. The argument \
             `pg_range` makes: a collation is a feature this node does not have, so listing \
             PostgreSQL's would tell a client it could ask for one. What makes the emptiness safe \
             is above — the join that reads it never matches on a real server either.",
        "UNMEASURED",
    )],
};

#[test]
fn every_attribute_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_attribute.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 35,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Every relation of a table has its own oid, and every view agrees about it.**
///
/// The assertion this phase exists for, and it is not a value assertion: `9de9519` gave a primary
/// key and a sequence the *table's* id, so `t`, `t_pkey` and `t_id_seq` were three rows of
/// `pg_class` with one oid between them. Nothing read the column before this phase, which is why it
/// survived — and every statement `ActiveRecord`'s schema dump writes joins on it.
#[test]
fn a_table_its_key_and_its_sequence_have_three_different_oids() {
    let mut node = parity::Node::new(&["CREATE TABLE oi (id bigserial PRIMARY KEY, v text)"]);

    let oids = node.rows(
        "SELECT relname, oid FROM pg_class WHERE relname IN ('oi', 'oi_pkey', 'oi_id_seq') \
         ORDER BY relname",
    );
    assert_eq!(oids.len(), 3, "three relations: {oids:?}");
    let mut distinct: Vec<&String> = oids.iter().map(|row| &row[1]).collect();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 3, "three oids, one each: {oids:?}");

    // And the oid a name resolves to is the oid the *other* views use for it, which is what makes
    // `a.attrelid = c.oid` a join rather than a coincidence.
    assert_eq!(
        node.rows(
            "SELECT a.attname FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             WHERE c.relname = 'oi_pkey' ORDER BY a.attnum"
        ),
        vec![vec!["id"]]
    );
    assert_eq!(
        node.rows(
            "SELECT a.attname FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             WHERE c.relname = 'oi' AND a.attnum > 0 ORDER BY a.attnum"
        ),
        vec![vec!["id"], vec!["v"]]
    );
}

/// A table with **no** primary key has no hidden column in `pg_attribute`.
///
/// `TableDef` puts an internal row id in column 0 of a keyless table and `user_columns` hides it
/// (`catalog::INTERNAL_ROW_ID_NAME`). A `pg_attribute` built from `columns` rather than from
/// `user_columns` would report it — as a column with an empty name, at `attnum` 1, shifting every
/// real column by one. Measured on 19beta1: `cc (a int4, b text)` is `a` at 1 and `b` at 2.
#[test]
fn a_keyless_table_does_not_report_its_internal_row_id() {
    let mut node = parity::Node::new(&["CREATE TABLE ki (a int4, b text)"]);
    assert_eq!(
        node.rows(
            "SELECT attname, attnum FROM pg_attribute WHERE attrelid = 'ki'::regclass \
             AND attnum > 0 ORDER BY attnum"
        ),
        vec![vec!["a", "1"], vec!["b", "2"]]
    );
}

/// A **volatile** default is a row of `pg_attrdef` and an `atthasdef`, like any other.
///
/// Catalog record v5 records `DEFAULT CURRENT_TIMESTAMP` as a flag rather than a value, because a
/// constant cannot express it — so a `pg_attrdef` built from `ColumnDef::default` alone reports no
/// default at all for it, which is what this node did until the record grew the flag. Measured on
/// 19beta1: `atthasdef` is `t` and the expression prints **unparenthesised**, unlike a computed
/// default such as `DEFAULT 1 + 1`, which a real server prints as `(1 + 1)`.
///
/// **This node prints the canonical spelling for both of them.** PostgreSQL keeps the one the user
/// wrote — `DEFAULT now()` prints `now()` — and catalog record v5 stores a bool, so the two cannot
/// be told apart here. The value a row gets is identical either way, which is why one flag is the
/// right record; the divergence is in the text alone and is asserted nowhere else, so it is stated
/// here.
#[test]
fn a_volatile_default_is_reported_like_any_other() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE cv (id int8 PRIMARY KEY, made_at timestamp DEFAULT CURRENT_TIMESTAMP, \
         plain text DEFAULT 'x', none int4)",
    ]);

    assert_eq!(
        node.rows(
            "SELECT adnum, pg_get_expr(adbin, adrelid) FROM pg_attrdef \
             WHERE adrelid = 'cv'::regclass ORDER BY adnum"
        ),
        vec![vec!["2", "CURRENT_TIMESTAMP"], vec!["3", "'x'::text"]]
    );
    assert_eq!(
        node.rows(
            "SELECT attname, atthasdef FROM pg_attribute WHERE attrelid = 'cv'::regclass \
             AND attnum > 0 ORDER BY attnum"
        ),
        vec![
            vec!["id", "f"],
            vec!["made_at", "t"],
            vec!["plain", "t"],
            vec!["none", "f"],
        ]
    );
    // And `information_schema` reads the same expression, because it reads the same function.
    assert_eq!(
        node.rows(
            "SELECT column_name, column_default FROM information_schema.columns \
             WHERE table_name = 'cv' ORDER BY ordinal_position"
        ),
        vec![
            vec!["id", "\\N"],
            vec!["made_at", "CURRENT_TIMESTAMP"],
            vec!["plain", "'x'::text"],
            vec!["none", "\\N"],
        ]
    );
}

/// Every view in the registry refuses every write with `42501`, including the ones added here.
///
/// Written over `CatalogView::ALL` rather than over a list of names, so a view added to the
/// registry is refused by being added and cannot be forgotten.
#[test]
fn every_catalog_view_is_read_only() {
    let mut node = parity::Node::new(&[]);
    for view in esker_sql::catalog::pg_catalog::CatalogView::ALL {
        let name = view.name();
        // The `information_schema` views carry their schema in their name, and a real server
        // refuses a write to one as a **view** rather than as a permission — `42809 "tables" is
        // not a table`, measured, because `information_schema` really is built out of views where
        // `pg_catalog` is built out of tables. Here the DDL path refuses a schema-qualified name
        // before it reaches this guard at all, which is a `0A000` naming the qualified name.
        // Declared in `tests/pg_catalog_information.rs`; this loop is about the other rule.
        if name.contains('.') {
            continue;
        }
        for statement in [
            format!("DROP TABLE {name}"),
            format!("ALTER TABLE {name} ADD COLUMN a bigint"),
            format!("CREATE INDEX ix_{name} ON {name} (oid)"),
        ] {
            let error = node.run(&statement).unwrap_err();
            assert_eq!(
                error.sqlstate(),
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "{statement}"
            );
            assert!(
                error.to_string().ends_with("is a system catalog"),
                "{statement}"
            );
        }
    }
}
