//! `EXCLUDE` constraints — statement 777 of `postgresql_specific_schema.rb`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // A range is `daterange` on a real server and `text` here — the standing type trade, and the
    // one `pg_typeof` would prove. `&&` and `isempty` answer `boolean` on both.
    types: &[
        // `information_schema`'s own domains — `name` and two `character varying(3)`s — are `text`
        // here, with identical characters. The trade every catalog column makes.
        "SELECT 'r', constraint_name, constraint_type, is_deferrable, initially_deferred FROM \
         information_schema.table_constraints WHERE table_name = 'test_exclusion_constraints' \
         ORDER BY constraint_name",
        "SELECT 'r', daterange('2026-01-01','2026-02-01'), daterange('2026-01-01','2026-02-01') && \
         daterange('2026-02-01','2026-03-01'), daterange('2026-01-01','2026-02-01') && \
         daterange('2026-01-15','2026-03-01')",
        "SELECT 'r', daterange(NULL,'2026-02-01'), daterange('2026-01-01',NULL), \
         daterange(NULL,NULL) && daterange('2026-01-01','2026-02-01')",
    ],
    answers: &[
        // **A scalar key**, which a real server refuses for a reason this node cannot reach.
        (
            "CREATE TABLE tec_scalar (id int8, CONSTRAINT tec_scalar_x EXCLUDE USING gist (id \
             WITH =))",
            "Both refuse it. A real server names the *type*: `42704 data type bigint has no \
             default operator class for access method \"gist\"` — it has `gist` and no \
             `btree_gist`. This node names the *operator*, because `&&` over a range is the only \
             exclusion it enforces and the refusal is raised where the constraint is read, before \
             any column type is in reach. Same outcome, different half of the sentence.",
        ),
        // **`daterange` as a column type**, which is a type-surface unit and not this one.
        (
            "CREATE TABLE tec_btree (r daterange, CONSTRAINT tec_btree_x EXCLUDE USING btree (r \
             WITH &&))",
            "`0A000 the type daterange is not supported`: a range reaches this node as an \
             *expression* (`daterange(a, b)`), and a stored range column is a type with a codec, \
             an ordering and a columnar mapping of its own. The suite excludes on an expression \
             precisely because that is what works without `btree_gist`, so nothing in the schema \
             needs the column type — these three lines are the capture probing the edges.",
        ),
        (
            "CREATE TABLE tec_plain (r daterange, CONSTRAINT tec_plain_x EXCLUDE (r WITH &&))",
            "The same missing column type. The shape this line exists for — a bare `EXCLUDE` \
             defaulting to btree and being refused — is proved by `tec_btree` above and by \
             `an_exclude_without_using_gist_is_refused` below, which use no range column.",
        ),
        (
            "CREATE TABLE tec_gist (r daterange, CONSTRAINT tec_gist_x EXCLUDE USING gist (r WITH \
             &&))",
            "The same, and it is the table the three lines after it read — so those are the \
             cascade of this one rather than divergences of their own.",
        ),
    ],
};

#[test]
fn every_exclusion_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_exclusion_constraint.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **`daterange` is half-open**, which is what makes the suite's adjacent ranges legal.
#[test]
fn a_daterange_is_half_open_and_may_be_unbounded() {
    let mut node = parity::Node::new(&[]);
    // Adjacent ranges do **not** overlap: a closed reading would refuse a row PostgreSQL admits.
    assert_eq!(
        node.rows(
            "SELECT daterange('2026-01-01','2026-02-01') && daterange('2026-02-01','2026-03-01')"
        ),
        [["f"]]
    );
    assert_eq!(
        node.rows(
            "SELECT daterange('2026-01-01','2026-02-01') && daterange('2026-01-15','2026-02-15')"
        ),
        [["t"]]
    );
    // An **empty** range overlaps nothing, itself included.
    assert_eq!(
        node.rows(
            "SELECT isempty(daterange('2026-01-01','2026-01-01')), \
             daterange('2026-01-01','2026-01-01') && daterange('2026-01-01','2026-02-01')"
        ),
        [["t", "f"]]
    );
    // A NULL bound is **unbounded**, not NULL, and prints as an open end.
    assert_eq!(
        node.rows(
            "SELECT daterange(NULL,'2026-02-01'), daterange(NULL,NULL) && \
             daterange('2026-01-01','2026-02-01')"
        ),
        [["(,2026-02-01)", "t"]]
    );
}

/// The constraint refuses an overlapping row with **`23P01`**, not `23505`.
#[test]
fn an_overlapping_row_is_23p01() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ex (id bigserial primary key, start_date date, end_date date, CONSTRAINT \
         ex_overlap EXCLUDE USING gist (daterange(start_date, end_date) WITH &&) WHERE \
         (start_date IS NOT NULL AND end_date IS NOT NULL))",
    ]);
    node.run("INSERT INTO ex (start_date, end_date) VALUES ('2026-01-01','2026-02-01')")
        .unwrap();
    // Adjacent: accepted, because the range is half-open.
    node.run("INSERT INTO ex (start_date, end_date) VALUES ('2026-02-01','2026-03-01')")
        .unwrap();
    let error = node
        .run("INSERT INTO ex (start_date, end_date) VALUES ('2026-01-15','2026-02-15')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23P01");
    assert!(
        error.to_string().contains("ex_overlap"),
        "the constraint is named: {error}"
    );
}

/// **The partial `WHERE` makes NULLs legal — and duplicates too.**
///
/// A row the predicate rejects is not in the index at all, so two identical ones both insert. Four
/// rows here would be four conflicts without the clause.
#[test]
fn a_row_the_predicate_excludes_is_not_checked() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ex (id bigserial primary key, start_date date, end_date date, CONSTRAINT \
         ex_overlap EXCLUDE USING gist (daterange(start_date, end_date) WITH &&) WHERE \
         (start_date IS NOT NULL AND end_date IS NOT NULL))",
    ]);
    for values in [
        "(NULL,NULL)",
        "(NULL,NULL)",
        "('2026-01-10',NULL)",
        "('2026-01-10',NULL)",
    ] {
        node.run(&format!(
            "INSERT INTO ex (start_date, end_date) VALUES {values}"
        ))
        .unwrap();
    }
    assert_eq!(node.rows("SELECT count(*) FROM ex"), [["4"]]);
}

/// An `UPDATE` is checked, and only when the **new** range really overlaps.
#[test]
fn an_update_is_checked_against_the_new_range() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ex (id bigserial primary key, start_date date, end_date date, CONSTRAINT \
         ex_overlap EXCLUDE USING gist (daterange(start_date, end_date) WITH &&) WHERE \
         (start_date IS NOT NULL AND end_date IS NOT NULL))",
    ]);
    node.run("INSERT INTO ex (start_date, end_date) VALUES ('2026-01-01','2026-02-01')")
        .unwrap();
    node.run("INSERT INTO ex (start_date, end_date) VALUES ('2026-02-01','2026-03-01')")
        .unwrap();
    // Moving the end forward keeps them disjoint.
    node.run("UPDATE ex SET end_date = '2026-02-20' WHERE start_date = '2026-02-01'")
        .unwrap();
    // Moving the start back overlaps the first row.
    let error = node
        .run("UPDATE ex SET start_date = '2026-01-15' WHERE start_date = '2026-02-01'")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "23P01");
}

/// **`INITIALLY DEFERRED` holds the check to `COMMIT`**, and that is what makes a repair possible.
///
/// The point of deferring is not the delay: it is that a transaction may break the constraint in
/// the middle and put it right before the end. So the conflicting row inserts, `count(*)` sees it,
/// and what happens next depends on what the transaction does — `DELETE` the row it collided with
/// and the commit stands; leave it and the `23P01` arrives at `COMMIT`.
///
/// The **later** row is the one named `Key` and the earlier one is the existing key, because only
/// the insert that conflicted queued a recheck — the first row's insert conflicted with nothing and
/// left nothing to run.
#[test]
fn initially_deferred_holds_the_check_to_commit() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ex (a date, b date, CONSTRAINT ex_3 EXCLUDE USING gist (daterange(a, b) \
         WITH &&) DEFERRABLE INITIALLY DEFERRED)",
    ]);
    // Recorded as PostgreSQL records it: deferrable **and** deferred.
    assert_eq!(
        node.rows(
            "SELECT conname, contype, condeferrable, condeferred FROM pg_constraint WHERE \
             conrelid = 'ex'::regclass AND contype = 'x'"
        ),
        [["ex_3", "x", "t", "t"]]
    );
    // Broken in the middle and repaired before the end: this commits.
    for statement in [
        "BEGIN",
        "INSERT INTO ex (a, b) VALUES ('2026-01-01','2026-02-01')",
        "INSERT INTO ex (a, b) VALUES ('2026-01-15','2026-02-15')",
        "DELETE FROM ex WHERE a = '2026-01-01'",
        "COMMIT",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(node.rows("SELECT count(*) FROM ex"), [["1"]]);

    // Broken and left broken: the row inserts, and `COMMIT` is where it fails.
    node.run("DELETE FROM ex").unwrap();
    for statement in [
        "BEGIN",
        "INSERT INTO ex (a, b) VALUES ('2026-01-01','2026-02-01')",
        "INSERT INTO ex (a, b) VALUES ('2026-01-15','2026-02-15')",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT count(*) FROM ex"),
        [["2"]],
        "both are there"
    );
    let error = node.run("COMMIT").unwrap_err();
    assert_eq!(error.sqlstate(), "23P01");
    assert_eq!(
        error.detail().as_deref(),
        Some(
            "Key (daterange(a, b))=([2026-01-15,2026-02-15)) conflicts with existing key \
             (daterange(a, b))=([2026-01-01,2026-02-01))."
        ),
        "the later row is the Key; only its insert queued a recheck"
    );
    // The failed check rolled the transaction back, so neither row is there.
    assert_eq!(node.rows("SELECT count(*) FROM ex"), [["0"]]);
}

/// **`USING gist` is load-bearing, not decoration** — and the bare form is not a shortcut for it.
///
/// `EXCLUDE (… WITH &&)` with no `USING` defaults to **btree**, and `&&` is not in btree's
/// `range_ops` family; spelling `USING btree` gives the identical error, which is how a reader can
/// tell what the default was. The capture proves both against a `daterange` *column*, which this
/// node does not have — so both are asserted here over the expression key the suite actually uses.
///
/// Getting this wrong in the accepting direction is the dangerous one: reading a bare `EXCLUDE` as
/// `USING gist` would silently enforce a constraint a real server refused to create.
#[test]
fn an_exclude_without_using_gist_is_refused() {
    for written in [
        "CREATE TABLE tec (a date, b date, CONSTRAINT tec_x EXCLUDE (daterange(a, b) WITH &&))",
        "CREATE TABLE tec (a date, b date, CONSTRAINT tec_x EXCLUDE USING btree (daterange(a, b) \
         WITH &&))",
    ] {
        let mut node = parity::Node::new(&[]);
        let error = node.run(written).unwrap_err();
        assert_eq!(error.sqlstate(), "42809", "for {written}");
        assert_eq!(
            error.to_string(),
            "operator &&(anyrange,anyrange) is not a member of operator family \"range_ops\"",
            "for {written}"
        );
        assert_eq!(
            error.detail().as_deref(),
            Some(
                "The exclusion operator must be related to the index operator class for the \
                 constraint."
            )
        );
        // And nothing was created: the refusal is raised where the constraint is read.
        assert!(
            node.rows("SELECT relname FROM pg_class WHERE relname = 'tec'")
                .is_empty()
        );
    }
}

/// **Statement 777 itself**: all three constraints in one `CREATE TABLE`, as the suite writes it.
#[test]
fn statement_777_loads() {
    let mut node = parity::Node::new(&[]);
    node.run(
        "CREATE TABLE \"test_exclusion_constraints\" (\"id\" bigserial primary key, \
         \"start_date\" date, \"end_date\" date, \"valid_from\" date, \"valid_to\" date, \
         \"transaction_from\" date, \"transaction_to\" date, CONSTRAINT \
         \"test_exclusion_constraints_date_overlap\" EXCLUDE USING gist (daterange(start_date, \
         end_date) WITH &&) WHERE (start_date IS NOT NULL AND end_date IS NOT NULL), CONSTRAINT \
         \"test_exclusion_constraints_valid_overlap\" EXCLUDE USING gist (daterange(valid_from, \
         valid_to) WITH &&) WHERE (valid_from IS NOT NULL AND valid_to IS NOT NULL) DEFERRABLE \
         INITIALLY IMMEDIATE, CONSTRAINT \"test_exclusion_constraints_transaction_overlap\" \
         EXCLUDE USING gist (daterange(transaction_from, transaction_to) WITH &&) WHERE \
         (transaction_from IS NOT NULL AND transaction_to IS NOT NULL) DEFERRABLE INITIALLY \
         DEFERRED)",
    )
    .unwrap();
    // Three constraints, and **`DEFERRABLE INITIALLY IMMEDIATE` prints as bare `DEFERRABLE`** —
    // the same "keep only what differs from the default" rule a `UNIQUE` constraint follows.
    assert_eq!(
        node.rows(
            "SELECT conname, condeferrable, condeferred FROM pg_constraint WHERE conrelid = \
             'test_exclusion_constraints'::regclass AND contype = 'x' ORDER BY conname"
        ),
        vec![
            vec!["test_exclusion_constraints_date_overlap", "f", "f"],
            vec!["test_exclusion_constraints_transaction_overlap", "t", "t"],
            vec!["test_exclusion_constraints_valid_overlap", "t", "f"],
        ]
    );
}
