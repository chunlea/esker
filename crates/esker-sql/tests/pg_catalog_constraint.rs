//! Contract C3 for `pg_constraint` and `pg_get_constraintdef` — phase 13 unit 3.
//!
//! Four of `ActiveRecord`'s schema-dump methods read this relation and this node has none of what
//! three of them are looking for: `FOREIGN KEY`, `CHECK` and `EXCLUDE` are all `0A000` in the DDL,
//! so no rows is a *correct* answer about this catalog. The fourth, `unique_constraints()`, is a
//! real gap and is declared below.
//!
//! What it does have is the row nothing asked about: **PostgreSQL 19 keeps a `pg_constraint` row
//! for every `NOT NULL` column**, and an emulation written from an older major would not have one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `conname` is a `name`, `contype` is a `"char"` and `conrelid`, `conindid` and `confrelid` are
    // `oid`s on a real server; this node has none of those three types, so they are `text` and
    // `bigint`. Every value is identical.
    types: &[
        "SELECT conname, contype, condeferrable, condeferred, convalidated FROM pg_constraint WHERE conrelid = 'ka'::regclass ORDER BY conname",
        "SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'ka'::regclass ORDER BY conname",
        "SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'kc'::regclass ORDER BY conname",
        "SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'kb'::regclass ORDER BY conname",
        "SELECT conname, conindid = 'ka_pkey'::regclass FROM pg_constraint WHERE conrelid = 'ka'::regclass AND contype = 'p'",
        "SELECT conname, conrelid = 'ka'::regclass, confrelid FROM pg_constraint WHERE conrelid = 'ka'::regclass AND contype = 'p'",
        "SELECT conname, confupdtype = ' ', confdeltype = ' ', confrelid, conindid = 0 FROM pg_constraint WHERE conrelid = 'ka'::regclass ORDER BY conname",
        "SELECT c.conname FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid JOIN pg_namespace n ON n.oid = c.connamespace WHERE c.contype = 'p' AND t.relname = 'ka' AND n.nspname = 'public'",
        "SELECT conname, pg_get_constraintdef(oid, true) FROM pg_constraint WHERE conrelid = 'ka'::regclass AND contype = 'p'",
        "SELECT conname FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid JOIN pg_namespace n ON n.oid = c.connamespace WHERE c.contype = 'c' AND t.relname = 'ka' AND n.nspname = 'public'",
        "SELECT conname FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid JOIN pg_namespace n ON n.oid = c.connamespace WHERE c.contype = 'f' AND t.relname = 'ka' AND n.nspname = 'public'",
        "SELECT conname FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid JOIN pg_namespace n ON n.oid = c.connamespace WHERE c.contype = 'u' AND t.relname = 'ka' AND n.nspname = 'public'",
        "SELECT conname FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid JOIN pg_namespace n ON n.oid = c.connamespace WHERE c.contype = 'x' AND t.relname = 'ka' AND n.nspname = 'public'",
        "SELECT i.relname, x.indisunique, x.indisprimary, x.indkey FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = 'kd'::regclass ORDER BY i.relname",
    ],
    answers: &[
        (
            "SELECT conname, contype, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'kd'::regclass ORDER BY conname",
            "**a `UNIQUE` constraint and a `CREATE UNIQUE INDEX` are one record here.** A real \
             server keeps `kd_u_key` and `kd_w_v_key` in both `pg_constraint` (contype `u`) and \
             `pg_index`; this node stores an `IndexDef` with `unique` set and nothing that says \
             which statement wrote it, so it reports the shape it can prove — the unique index, \
             which agrees exactly — and claims no constraint. A `u` row for every unique index \
             would tell `unique_constraints()` about a constraint the user never declared, which \
             is a wrong answer rather than a missing one. Closing it is one bool on `IndexDef`, \
             which is a catalog **record format** change and a question for a human \
             (`CLAUDE.md`, \"Ask before doing\"). Until then `schema_dumper` writes \
             `t.index …, unique: true` where a real server writes `t.unique_constraint …`, and \
             the schema that round-trips is the same schema.",
            "UNMEASURED",
        ),
        (
            "SELECT c.conname, c.conindid = i.oid FROM pg_constraint c JOIN pg_class i ON i.oid = c.conindid WHERE c.conrelid = 'kd'::regclass AND c.contype = 'u' ORDER BY c.conname",
            "the same fact through the join `unique_constraints()` writes: two rows there, none \
             here.",
            "UNMEASURED",
        ),
        (
            "SELECT conname, contype, conkey FROM pg_constraint WHERE conrelid = 'kd'::regclass ORDER BY conname",
            "**`conkey` is refused by name** (`42703`), where a real server answers `{1}` and \
             `{3,4}`. It is a `smallint[]`, and every use `ActiveRecord` makes of it is a real \
             array subscript — `c.conkey[idx]` under `generate_subscripts` — so a `text` column \
             spelled `{1}` would answer `conkey[1]` with a brace rather than a column number. \
             That is the opposite of `pg_index.indkey`, which is provided as text *because* the \
             client's use of it is `split(\" \")`. The rule is the client's use, not the type. \
             `information_schema.key_column_usage` answers the same question in a shape this node \
             has, and unit 4 provides it.",
            "UNMEASURED",
        ),
        (
            "SELECT pg_typeof(contype), pg_typeof(conname), pg_typeof(conrelid) FROM pg_constraint WHERE conrelid = 'kd'::regclass AND contype = 'p'",
            "`pg_typeof` is a function this node does not have. What it would have said is the \
             type divergence declared above.",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_constraint_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_catalog_constraint.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// An oid read out of `pg_constraint` reads back through `pg_get_constraintdef`.
///
/// The round trip `check_constraints()` and `unique_constraints()` both make — `SELECT conname,
/// pg_get_constraintdef(c.oid) FROM pg_constraint c` — and the one thing a derived oid has to get
/// right. A `NOT NULL` constraint has no record at all here: its oid is built from the table and
/// the column and taken apart again on the way back, so this is what says the two halves agree.
#[test]
fn a_constraint_oid_reads_back_as_its_own_definition() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE cr (id int8 PRIMARY KEY, a int4 NOT NULL, b text)",
        "CREATE TABLE cs (k1 int8, k2 text, v int4, PRIMARY KEY (k1, k2))",
    ]);

    assert_eq!(
        node.rows(
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'cr'::regclass ORDER BY conname"
        ),
        vec![
            vec!["cr_a_not_null", "NOT NULL a"],
            vec!["cr_id_not_null", "NOT NULL id"],
            vec!["cr_pkey", "PRIMARY KEY (id)"],
        ]
    );
    // A two-column key: both columns are NOT NULL without saying so, and the definition lists
    // them in key order.
    assert_eq!(
        node.rows(
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'cs'::regclass ORDER BY conname"
        ),
        vec![
            vec!["cs_k1_not_null", "NOT NULL k1"],
            vec!["cs_k2_not_null", "NOT NULL k2"],
            vec!["cs_pkey", "PRIMARY KEY (k1, k2)"],
        ]
    );
    // And two tables' constraints do not share an oid, which is the failure a derived one invites.
    let oids = node.rows("SELECT oid FROM pg_constraint ORDER BY oid");
    let mut distinct = oids.clone();
    distinct.dedup();
    assert_eq!(distinct.len(), oids.len(), "an oid is used twice: {oids:?}");
}

/// An oid that names no constraint is NULL, not an error — including a table's own.
#[test]
fn an_oid_that_is_not_a_constraint_is_null() {
    let mut node = parity::Node::new(&["CREATE TABLE nc (id int8 PRIMARY KEY, a int4)"]);
    assert_eq!(
        node.rows("SELECT pg_get_constraintdef('nc'::regclass), pg_get_constraintdef(999999)"),
        vec![vec!["\\N", "\\N"]]
    );
}
