//! **`ALTER TABLE … ADD CONSTRAINT … UNIQUE USING INDEX`** — a constraint attached to an index
//! that already exists, rather than one that builds its own.
//!
//! `UniqueConstraintTest#test_add_unique_constraint_with_name_and_using_index` and
//! `#..._with_only_using_index` send it, after `add_index … unique: true`:
//!
//! ```text
//! CREATE UNIQUE INDEX "unique_index" ON "sections" ("position")
//! ALTER TABLE "sections" ADD CONSTRAINT "unique_constraint" UNIQUE USING INDEX "unique_index"
//!     DEFERRABLE INITIALLY IMMEDIATE
//! ```
//!
//! # Everything below was measured on 19beta1 before a line was written
//!
//! The unit was sized on a worry that turned out to be backwards. `tests/alter_index.rs` records
//! that in this node an index and its constraint are **one field** rather than two catalog rows,
//! which read like "a bare unique index may already answer as a constraint here, and then the
//! count is wrong before the `ALTER` is even reached". It does not:
//!
//! ```text
//! a bare CREATE UNIQUE INDEX, Rails' own contype='u' query
//!     PostgreSQL   0 rows          this node   0 rows
//! ```
//!
//! That note is about a *constraint's* index following a rename, not about an index inventing a
//! constraint. The plain `ADD CONSTRAINT … UNIQUE (position)` already worked here; only the
//! `USING INDEX` spelling was refused.
//!
//! **The three answers the capture gave**, which are what this file pins:
//!
//! ```text
//! 1  the constraint is attached to the NAMED index; no second index is built
//! 2  the index is RENAMED to the constraint's name, and PostgreSQL says so:
//!      NOTICE:  ALTER TABLE / ADD CONSTRAINT USING INDEX will rename index
//!               "unique_index" to "unique_constraint"
//!    — and when the two names are already equal there is no notice and no rename
//! 3  DEFERRABLE INITIALLY IMMEDIATE lands on the constraint:
//!      condeferrable t, condeferred f, def  UNIQUE ("position") DEFERRABLE
//!    with no clause at all it is  f, f  and  UNIQUE ("position")
//! ```
//!
//! And the two refusals, with their SQLSTATEs read off the server rather than guessed:
//!
//! **One difference in the printed definition, and it is an older declared divergence.**
//! PostgreSQL writes `UNIQUE ("position")` and this node writes `UNIQUE (position)`: `position` is
//! one of PostgreSQL's *reserved* words and it quotes those, which
//! `catalog::pg_index::quote_identifier` records as deliberately not approximated — `sqlparser`'s
//! keyword lists are a different set and would quote `name` and `value`, which a real server
//! leaves bare. It reaches nothing here: `ActiveRecord` reads `constraintdef` only to test
//! `start_with?("UNIQUE NULLS NOT DISTINCT")`, so both Rails tests are unaffected. The assertions
//! below carry **this node's** text, so that closing the divergence reddens them (ADR 0031 rule 2)
//! rather than leaving a wrong expectation green.
//!
//! ```text
//! USING INDEX nosuchindex   42704  index "nosuchindex" does not exist
//! USING INDEX plain_idx     42809  "plain_idx" is not a unique index
//!                                  DETAIL: Cannot create a primary key or unique
//!                                          constraint using such an index.
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE g1_sections (id bigserial primary key, position integer)",
    "CREATE UNIQUE INDEX unique_index ON g1_sections (position)",
];

fn constraints(node: &mut parity::Node) -> Vec<Vec<String>> {
    node.rows(
        "SELECT c.conname, c.contype, c.condeferrable, c.condeferred, \
         pg_get_constraintdef(c.oid) FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
         WHERE c.contype = 'u' AND t.relname = 'g1_sections'",
    )
}

fn indexes(node: &mut parity::Node) -> Vec<Vec<String>> {
    node.rows(
        "SELECT indexrelid::regclass::text FROM pg_index \
         WHERE indrelid = 'g1_sections'::regclass ORDER BY 1",
    )
}

/// **The control, and the correction.** A bare unique index is not a unique constraint on either
/// server — which is what makes the count in the Rails test reachable at all.
#[test]
fn a_bare_unique_index_is_not_a_constraint() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(constraints(&mut node), Vec::<Vec<String>>::new());
    assert_eq!(
        indexes(&mut node),
        [["g1_sections_pkey".to_owned()], ["unique_index".to_owned()]]
    );
}

/// The statement the suite sends, and the row it then reads.
#[test]
fn the_constraint_is_attached_to_the_named_index() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(
        "ALTER TABLE g1_sections ADD CONSTRAINT unique_constraint UNIQUE \
         USING INDEX unique_index DEFERRABLE INITIALLY IMMEDIATE",
    )
    .unwrap();
    assert_eq!(
        constraints(&mut node),
        [[
            "unique_constraint".to_owned(),
            "u".to_owned(),
            "t".to_owned(),
            "f".to_owned(),
            "UNIQUE (position) DEFERRABLE".to_owned(),
        ]],
        "exactly one, deferrable and not deferred"
    );
}

/// **The index is renamed, and no second one is built.** Two indexes before, two after, and the
/// unique one now carries the constraint's name.
#[test]
fn the_index_takes_the_constraints_name() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(
        "ALTER TABLE g1_sections ADD CONSTRAINT unique_constraint UNIQUE \
         USING INDEX unique_index DEFERRABLE INITIALLY IMMEDIATE",
    )
    .unwrap();
    assert_eq!(
        indexes(&mut node),
        [
            ["g1_sections_pkey".to_owned()],
            ["unique_constraint".to_owned()],
        ],
        "renamed, not duplicated"
    );
}

/// With no `DEFERRABLE` clause: the constraint is not deferrable and the definition says nothing.
#[test]
fn without_the_clause_it_is_not_deferrable() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1_sections ADD CONSTRAINT uc2 UNIQUE USING INDEX unique_index")
        .unwrap();
    assert_eq!(
        constraints(&mut node),
        [[
            "uc2".to_owned(),
            "u".to_owned(),
            "f".to_owned(),
            "f".to_owned(),
            "UNIQUE (position)".to_owned(),
        ]]
    );
}

/// **A constraint named the same as its index needs no rename** — the case that would loop or
/// collide in an implementation that renamed unconditionally.
#[test]
fn the_same_name_is_not_a_rename() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("ALTER TABLE g1_sections ADD CONSTRAINT unique_index UNIQUE USING INDEX unique_index")
        .unwrap();
    assert_eq!(
        constraints(&mut node),
        [[
            "unique_index".to_owned(),
            "u".to_owned(),
            "f".to_owned(),
            "f".to_owned(),
            "UNIQUE (position)".to_owned(),
        ]]
    );
    assert_eq!(
        indexes(&mut node),
        [["g1_sections_pkey".to_owned()], ["unique_index".to_owned()]]
    );
}

/// An index that is not there, and one that is not unique. Both SQLSTATEs were read off the
/// server; the second carries a `DETAIL` naming what such an index cannot be used for.
#[test]
fn the_two_refusals() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("ALTER TABLE g1_sections ADD CONSTRAINT uc UNIQUE USING INDEX nosuchindex")
            .to_string(),
        "!42704 index \"nosuchindex\" does not exist"
    );

    let mut node = parity::Node::new(&[
        "CREATE TABLE g1_sections (id bigserial primary key, position integer)",
        "CREATE INDEX plain_idx ON g1_sections (position)",
    ]);
    assert_eq!(
        node.answer("ALTER TABLE g1_sections ADD CONSTRAINT uc UNIQUE USING INDEX plain_idx")
            .to_string(),
        "!42809 \"plain_idx\" is not a unique index DETAIL: Cannot create a primary key or \
         unique constraint using such an index."
    );
}

/// `ActiveRecord`'s `unique_constraints("sections")` query, verbatim — the one the two tests read,
/// including the `conkey_names` array they turn into `constraint.column`.
#[test]
fn the_query_activerecord_actually_sends() {
    let mut node = parity::Node::new(FIXTURE);
    node.run(
        "ALTER TABLE g1_sections ADD CONSTRAINT unique_constraint UNIQUE \
         USING INDEX unique_index DEFERRABLE INITIALLY IMMEDIATE",
    )
    .unwrap();
    assert_eq!(
        node.rows(
            "SELECT c.conname, c.condeferrable, c.condeferred, pg_get_constraintdef(c.oid), \
             ( SELECT array_agg(a.attname ORDER BY idx) FROM ( SELECT idx, c.conkey[idx] AS \
             conkey_elem FROM generate_subscripts(c.conkey, 1) AS idx ) indexed_conkeys \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = indexed_conkeys.conkey_elem \
             ) AS conkey_names \
             FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
             JOIN pg_namespace n ON n.oid = c.connamespace \
             WHERE c.contype = 'u' AND t.relname = 'g1_sections' AND n.nspname = 'public'"
        ),
        [[
            "unique_constraint".to_owned(),
            "t".to_owned(),
            "f".to_owned(),
            "UNIQUE (position) DEFERRABLE".to_owned(),
            "{position}".to_owned(),
        ]]
    );
}
