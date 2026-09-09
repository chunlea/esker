//! **A sequence's `RENAME TO` stays in the sequence's own schema** — the third face of one
//! mechanism, and the one that was still open.
//!
//! `SchemaWithDotsTest#test_rename_table` renames a table inside a schema called `my.schema`, and
//! `ActiveRecord` follows the table with its sequence:
//!
//! ```text
//! ALTER TABLE "my.schema"."posts_id_seq" RENAME TO "articles_id_seq"
//!   relation "articles_id_seq" already exists
//! ```
//!
//! `public.articles_id_seq` is a suite fixture's sequence and is always there, so the collision
//! check — which asked for the **bare** target name — fired on a name in a schema nobody had
//! mentioned. A table's `RENAME TO` had the same bug and an index's `RENAME TO` had it too; both
//! were closed, and this is the third.
//!
//! **The dot is not the mechanism.** It is how the suite reaches it, and every case below is
//! asserted twice, in a dotted schema and an ordinary one.
//!
//! # Measured on 19beta1
//!
//! ```text
//! CREATE TABLE public.articles (id bigserial primary key);
//! CREATE TABLE "my.schema".posts (id bigserial primary key);
//! ALTER TABLE "my.schema"."posts_id_seq" RENAME TO "articles_id_seq";   -- accepted
//!
//!   my.schema | articles_id_seq        <- the renamed one, still in its schema
//!   public    | articles_id_seq        <- untouched
//!
//! ALTER TABLE g1q_s.b_seq RENAME TO a_seq   (a_seq already in g1q_s)
//!   ERROR: relation "a_seq" already exists  -- the check is real, it asked the wrong schema
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node(schema: &str) -> parity::Node {
    let mut node = parity::Node::new(&["CREATE TABLE public.articles (id bigserial primary key)"]);
    node.run(&format!("CREATE SCHEMA \"{schema}\"")).unwrap();
    node.run(&format!(
        "CREATE TABLE \"{schema}\".posts (id bigserial primary key)"
    ))
    .unwrap();
    node
}

/// Every sequence, as `schema|name`.
fn sequences(node: &mut parity::Node) -> Vec<String> {
    node.rows(
        "SELECT n.nspname, c.relname FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind = 'S' ORDER BY 1, 2",
    )
    .into_iter()
    .map(|row| row.join("|"))
    .collect()
}

/// **The statement the suite sends.**
#[test]
fn a_sequence_rename_stays_in_its_own_schema() {
    for schema in ["my.schema", "g1q_plain"] {
        let mut node = node(schema);
        node.run(&format!(
            "ALTER TABLE \"{schema}\".\"posts_id_seq\" RENAME TO \"articles_id_seq\""
        ))
        .unwrap_or_else(|error| panic!("in {schema}: {error}"));
        assert_eq!(
            sequences(&mut node),
            [
                format!("{schema}|articles_id_seq"),
                "public|articles_id_seq".to_owned()
            ],
            "in {schema}"
        );
    }
}

/// **The collision check is still real**, which is the half a fix must not lose: inside one schema
/// the same rename is refused, with PostgreSQL's code and its bare name.
#[test]
fn a_collision_inside_the_same_schema_is_still_refused() {
    for schema in ["my.schema", "g1q_plain"] {
        let mut node = node(schema);
        node.run(&format!(
            "CREATE TABLE \"{schema}\".other (id bigserial primary key)"
        ))
        .unwrap();
        node.run(&format!(
            "ALTER TABLE \"{schema}\".\"posts_id_seq\" RENAME TO \"articles_id_seq\""
        ))
        .unwrap();
        assert_eq!(
            node.answer(&format!(
                "ALTER TABLE \"{schema}\".\"other_id_seq\" RENAME TO \"articles_id_seq\""
            ))
            .to_string(),
            "!42P07 relation \"articles_id_seq\" already exists",
            "in {schema}"
        );
    }
}

/// And the sequence answers to its new name afterwards, which is what says the name record moved
/// rather than merely the record's text.
#[test]
fn the_renamed_sequence_answers_to_its_new_name() {
    let mut node = node("my.schema");
    node.run("ALTER TABLE \"my.schema\".\"posts_id_seq\" RENAME TO \"articles_id_seq\"")
        .unwrap();
    assert_eq!(
        node.rows("SELECT nextval('\"my.schema\".articles_id_seq')"),
        [["1"]]
    );
}
