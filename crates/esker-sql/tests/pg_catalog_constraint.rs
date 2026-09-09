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
    // `conname` is a `name` here (ADR 0084) and `contype` a `"char"` (ADR 0095). What is left is
    // `conrelid`, `conindid` and `confrelid`: they name a **relation**, and a relation's oid stays
    // a `bigint` here because a catalog view's id comes from `VIEW_ID_BASE` and a primary key's
    // index oid from `PRIMARY_KEY_OID_BASE`, neither of which fits four bytes (ADR 0097). Every
    // value is identical.
    types: &[
        "SELECT conname, conrelid = 'ka'::regclass, confrelid FROM pg_constraint WHERE conrelid = \
         'ka'::regclass AND contype = 'p'",
        "SELECT conname, confupdtype = ' ', confdeltype = ' ', confrelid, conindid = 0 FROM \
         pg_constraint WHERE conrelid = 'ka'::regclass ORDER BY conname",
    ],
    answers: &[(
        "SELECT pg_typeof(contype), pg_typeof(conname), pg_typeof(conrelid) FROM pg_constraint \
         WHERE conrelid = 'kd'::regclass AND contype = 'p'",
        "**`pg_typeof(conrelid)` is the type divergence above, read through a function** — and it \
         is a row difference rather than a declared-type one because `pg_typeof` answers a name. \
         `contype` and `conname` agree; the third names a relation, whose oid is a `bigint` here \
         (ADR 0097).",
        // `UNMEASURED` because there is no session capture under `tests/captures/` for this
        // corpus — the oracle's answer is recorded on the corpus row itself
        // (`corpus/pg19_catalog_constraint.txt:68`, `"char"|name|oid`), which is what the entry
        // this replaced also declared.
        "UNMEASURED",
    )],
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
