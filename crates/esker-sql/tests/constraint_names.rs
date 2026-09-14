//! **The name a constraint is given, found and refused by** — debt #92, held to PostgreSQL 19's
//! answers.
//!
//! Four rules of one family, and none of them is about a schema: each is as wrong in `public` as
//! anywhere else, which is what separates this file from `constraint_name_in_a_schema.rs`.
//!
//! * A `CHECK` with no name is named for **the one column its expression reads**, and for none when
//!   it reads none or several; a derived name that is taken — a `CHECK`'s, a foreign key's, a
//!   `UNIQUE`'s or a primary key's — is **numbered** against every constraint in the schema, where
//!   a given one is refused.
//! * A table's constraints share **one name space**, of every kind.
//! * A `UNIQUE` or `PRIMARY KEY` meets **a relation** of its name first (`42P07`) and a constraint
//!   second (`42710`), because its name is its index's.
//! * `SET CONSTRAINTS <name>` finds its constraint **the way a relation is found**: a qualified name
//!   in its own schema, a bare one in the first schema on the search path that has one — and every
//!   constraint of that name there.
//!
//! `corpus/pg19_constraint_names.txt` is the capture and the replay is the test of record; the
//! tests after it pin one rule each, so a counterfactual can say which rule it broke.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_constraint_name_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_constraint_names.txt"),
        &[],
        &DIVERGENCES,
    );
    assert!(
        checked > 100,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **A `CHECK` is named for what it reads, not for where it is written**: the column constraint on
/// `b` reads `a` and `b` and is `ck_check`, the table constraint over `b` alone is `ck_b_check`, and
/// a name already chosen gets the next number on its label — in the order the constraints were
/// written, and again for the ones an `ALTER TABLE` adds.
#[test]
fn a_check_is_named_for_the_one_column_it_reads() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ck (a integer CHECK (a > 0), b integer CHECK (a < b), c integer CHECK (true), \
         CHECK (b > 0), CHECK (a <> b), CHECK (c > 0 AND c < 100), CHECK (a > 1))",
    ]);
    let checks = "SELECT conname FROM pg_constraint WHERE conrelid = 'ck'::regclass AND contype = 'c' \
                  ORDER BY conname";
    assert_eq!(
        node.rows(checks),
        [
            ["ck_a_check"],
            ["ck_a_check1"],
            ["ck_b_check"],
            ["ck_c_check"],
            ["ck_check"],
            ["ck_check1"],
            ["ck_check2"]
        ]
    );
    node.run("ALTER TABLE ck ADD CHECK (b < 1000)").unwrap();
    node.run("ALTER TABLE ck ADD CHECK (a + b < 5000)").unwrap();
    assert_eq!(
        node.rows(checks),
        [
            ["ck_a_check"],
            ["ck_a_check1"],
            ["ck_b_check"],
            ["ck_b_check1"],
            ["ck_c_check"],
            ["ck_check"],
            ["ck_check1"],
            ["ck_check2"],
            ["ck_check3"]
        ]
    );
}

/// **A derived name that is taken is numbered, against the whole schema; a given one is refused.**
/// `nsq_x_check1`, because another table's constraint holds `nsq_x_check`; the second foreign key
/// and the second `UNIQUE` on one column; a primary key beside a table that holds its name.
#[test]
fn a_taken_derived_name_is_numbered_and_a_taken_given_name_is_refused() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE other (x integer CONSTRAINT nsq_x_check CHECK (x > 0))",
        "CREATE TABLE nsq (x integer CHECK (x > 0))",
        "CREATE TABLE p (id integer PRIMARY KEY)",
        "CREATE TABLE t (a integer REFERENCES p, b integer)",
        "ALTER TABLE t ADD FOREIGN KEY (a) REFERENCES p",
        "ALTER TABLE t ADD UNIQUE (b)",
        "ALTER TABLE t ADD UNIQUE (b)",
        "CREATE TABLE q (id integer)",
        "CREATE TABLE q_pkey (x integer)",
        "ALTER TABLE q ADD PRIMARY KEY (id)",
        "CREATE TABLE k (a integer, CONSTRAINT k_a_fkey CHECK (a > 0))",
        "ALTER TABLE k ADD FOREIGN KEY (a) REFERENCES p",
    ]);
    assert_eq!(
        node.rows(
            "SELECT conname FROM pg_constraint WHERE conrelid = 'nsq'::regclass AND contype = 'c'"
        ),
        [["nsq_x_check1"]]
    );
    assert_eq!(
        node.rows(
            "SELECT conname FROM pg_constraint WHERE conrelid = 't'::regclass AND contype IN ('f', 'u') \
             ORDER BY conname"
        ),
        [["t_a_fkey"], ["t_a_fkey1"], ["t_b_key"], ["t_b_key1"]]
    );
    assert_eq!(
        node.rows(
            "SELECT conname FROM pg_constraint WHERE conrelid = 'q'::regclass AND contype = 'p'"
        ),
        [["q_pkey1"]]
    );
    assert_eq!(
        node.rows(
            "SELECT conname, contype FROM pg_constraint WHERE conrelid = 'k'::regclass AND contype IN \
             ('c', 'f') ORDER BY conname"
        ),
        [["k_a_fkey", "c"], ["k_a_fkey1", "f"]]
    );
    assert_eq!(
        node.answer("ALTER TABLE t ADD CONSTRAINT t_a_fkey FOREIGN KEY (a) REFERENCES p")
            .to_string(),
        "!42710 constraint \"t_a_fkey\" for relation \"t\" already exists"
    );
}

/// **A table's constraints share one name space, of every kind**: a name the primary key, a
/// `UNIQUE`, a `NOT NULL`, a foreign key or another `CHECK` holds is `42710` for a `CHECK`, and a
/// `CHECK`'s is `42710` for a foreign key or an `EXCLUDE`. Two given `CHECK`s of one name in one
/// `CREATE TABLE` are a sentence of their own, and another table may hold the same name.
#[test]
fn a_tables_constraints_share_one_name_space() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE p (id integer PRIMARY KEY)",
        "CREATE TABLE c (id integer PRIMARY KEY, p integer REFERENCES p, q integer NOT NULL, \
         u integer UNIQUE, CONSTRAINT c_positive CHECK (id > 0))",
        "CREATE TABLE ex (r daterange, CONSTRAINT ex_r CHECK (r IS NOT NULL))",
    ]);
    for taken in [
        "c_p_fkey",
        "c_pkey",
        "c_u_key",
        "c_q_not_null",
        "c_positive",
    ] {
        assert_eq!(
            node.answer(&format!(
                "ALTER TABLE c ADD CONSTRAINT {taken} CHECK (id > 1)"
            ))
            .to_string(),
            format!("!42710 constraint \"{taken}\" for relation \"c\" already exists"),
            "{taken}"
        );
    }
    assert_eq!(
        node.answer("ALTER TABLE c ADD CONSTRAINT c_positive FOREIGN KEY (p) REFERENCES p")
            .to_string(),
        "!42710 constraint \"c_positive\" for relation \"c\" already exists"
    );
    assert_eq!(
        node.answer("ALTER TABLE ex ADD CONSTRAINT ex_r EXCLUDE USING gist (r WITH &&)")
            .to_string(),
        "!42710 constraint \"ex_r\" for relation \"ex\" already exists"
    );
    assert_eq!(
        node.answer(
            "CREATE TABLE twice (a integer, CONSTRAINT dup CHECK (a > 0), CONSTRAINT dup CHECK (a < 10))"
        )
        .to_string(),
        "!42710 check constraint \"dup\" already exists"
    );
    assert_eq!(
        node.answer(
            "CREATE TABLE twice2 (a integer CONSTRAINT dup2 CHECK (a > 0), b integer CONSTRAINT dup2 \
             REFERENCES p)"
        )
        .to_string(),
        "!42710 constraint \"dup2\" for relation \"twice2\" already exists"
    );
    node.run("CREATE TABLE c2 (id integer, CONSTRAINT c_positive CHECK (id > 0))")
        .unwrap();
}

/// **A `UNIQUE` or `PRIMARY KEY` meets a relation of its name first**, because its name is its
/// index's: `42P07` for an index or a table of that name, `42710` for a constraint that is not one —
/// and a `CREATE UNIQUE INDEX` with a `CHECK`'s name builds, because an index is not a constraint.
#[test]
fn a_unique_or_primary_key_meets_a_relation_first() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE c (id integer PRIMARY KEY, q integer, u integer UNIQUE, \
         CONSTRAINT c_positive CHECK (id > 0))",
        "CREATE TABLE c2 (id integer, CONSTRAINT c_positive CHECK (id > 0))",
    ]);
    assert_eq!(
        node.answer("ALTER TABLE c ADD CONSTRAINT c_u_key UNIQUE (q)")
            .to_string(),
        "!42P07 relation \"c_u_key\" already exists"
    );
    assert_eq!(
        node.answer("ALTER TABLE c ADD CONSTRAINT c2 UNIQUE (q)")
            .to_string(),
        "!42P07 relation \"c2\" already exists"
    );
    assert_eq!(
        node.answer("ALTER TABLE c ADD CONSTRAINT c_positive UNIQUE (q)")
            .to_string(),
        "!42710 constraint \"c_positive\" for relation \"c\" already exists"
    );
    assert_eq!(
        node.answer("ALTER TABLE c2 ADD CONSTRAINT c_pkey PRIMARY KEY (id)")
            .to_string(),
        "!42P07 relation \"c_pkey\" already exists"
    );
    assert_eq!(
        node.answer("ALTER TABLE c2 ADD CONSTRAINT c_positive PRIMARY KEY (id)")
            .to_string(),
        "!42710 constraint \"c_positive\" for relation \"c2\" already exists"
    );
    node.run("CREATE UNIQUE INDEX c_positive ON c (q)").unwrap();
}

/// **`SET CONSTRAINTS` finds a name the way a relation is found**: a bare name no schema on the
/// path has is `42704`, a qualified one reaches its own schema's constraint and no other, a schema
/// that is not there is `3F000`, and a bare name is the first schema on the path that has one.
#[test]
fn set_constraints_finds_a_name_the_way_a_relation_is_found() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA s1",
        "CREATE SCHEMA s2",
        "CREATE TABLE s1.u (a integer, CONSTRAINT uq UNIQUE (a) DEFERRABLE INITIALLY IMMEDIATE)",
        "CREATE TABLE s2.u (a integer, CONSTRAINT uq UNIQUE (a) DEFERRABLE INITIALLY IMMEDIATE)",
        "INSERT INTO s1.u VALUES (1)",
        "INSERT INTO s2.u VALUES (1)",
    ]);
    let duplicate = "!23505 duplicate key value violates unique constraint \"uq\" DETAIL: Key (a)=(1) \
                     already exists.";
    assert_eq!(
        node.answer("SET CONSTRAINTS uq DEFERRED").to_string(),
        "!42704 constraint \"uq\" does not exist"
    );
    assert_eq!(
        node.answer("SET CONSTRAINTS nosuch.uq DEFERRED")
            .to_string(),
        "!3F000 schema \"nosuch\" does not exist"
    );
    node.run("BEGIN").unwrap();
    node.run("SET CONSTRAINTS s2.uq DEFERRED").unwrap();
    node.run("INSERT INTO s2.u VALUES (1)").unwrap();
    assert_eq!(
        node.answer("INSERT INTO s1.u VALUES (1)").to_string(),
        duplicate
    );
    node.run("ROLLBACK").unwrap();

    node.run("SET search_path = s1, s2, public").unwrap();
    node.run("BEGIN").unwrap();
    node.run("SET CONSTRAINTS uq DEFERRED").unwrap();
    node.run("INSERT INTO s1.u VALUES (1)").unwrap();
    assert_eq!(
        node.answer("INSERT INTO s2.u VALUES (1)").to_string(),
        duplicate
    );
    node.run("ROLLBACK").unwrap();
}

/// **Every constraint of the name in that schema, at once**: two tables' foreign keys called `kf`,
/// both deferred by one `SET CONSTRAINTS kf DEFERRED` and both owed at its `IMMEDIATE`. A guard
/// rather than a red test — it held before #92 — kept because keying the setting by schema is
/// exactly the change that could have narrowed it to one table.
#[test]
fn set_constraints_reaches_every_constraint_of_its_name_in_the_schema() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE parent (id integer PRIMARY KEY)",
        "CREATE TABLE k1 (p integer, CONSTRAINT kf FOREIGN KEY (p) REFERENCES parent DEFERRABLE)",
        "CREATE TABLE k2 (p integer, CONSTRAINT kf FOREIGN KEY (p) REFERENCES parent DEFERRABLE)",
    ]);
    node.run("BEGIN").unwrap();
    node.run("SET CONSTRAINTS kf DEFERRED").unwrap();
    node.run("INSERT INTO k1 VALUES (7)").unwrap();
    node.run("INSERT INTO k2 VALUES (7)").unwrap();
    assert_eq!(
        node.answer("SET CONSTRAINTS kf IMMEDIATE").to_string(),
        "!23503 insert or update on table \"k1\" violates foreign key constraint \"kf\" DETAIL: Key \
         (p)=(7) is not present in table \"parent\"."
    );
    node.run("ROLLBACK").unwrap();
}
