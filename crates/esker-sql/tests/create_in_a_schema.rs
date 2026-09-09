//! **Creating a relation, and renaming an index, must look for a collision in the schema the
//! object is going into — not in `public`.**
//!
//! `SchemaWithDotsTest`'s two tests, from the harness lane's side-by-side of the same fixture and
//! seed against PostgreSQL 19 (`triage/schemawithdots-divergence.md`). Both run with
//! `search_path` set to a schema called `my.schema`, and `articles` / `articles_pkey` are suite
//! fixtures in `public` that are always present:
//!
//! ```text
//! [836]  ALTER INDEX "posts_pkey" RENAME TO "articles_pkey"
//!          node -> ERROR relation "articles_pkey" already exists
//!          pg19 -> ALTER INDEX          (it becomes my.schema.articles_pkey)
//!
//! [844]  CREATE UNLOGGED TABLE "articles" ("id" bigserial primary key, "title" varchar)
//!          node -> ERROR relation "articles" already exists
//!          pg19 -> CREATE TABLE         (it becomes my.schema.articles)
//! ```
//!
//! The other four errors in that run are `25P02` consequences of these two.
//!
//! # This is the third face of one mechanism, and the first two are already closed
//!
//! `ALTER TABLE … RENAME TO` had it (`tests/rename_in_schema.rs`) and was fixed; that fix is why
//! the third statement of `test_rename_table` works and is *not* why these two tests still fail.
//! Object **creation** and **index rename** are separate resolution paths and kept the old
//! behaviour. The lesson is booked where it happened: one statement of a mechanism verified is not
//! the mechanism verified.
//!
//! # The dot is not the mechanism, and the tests below say so
//!
//! # Only one of the two statements reproduces, and that is the finding
//!
//! **[844] passes at this commit.** An unqualified `CREATE UNLOGGED TABLE g1sd_articles` under a
//! schema on the path, with `public.g1sd_articles` present, already goes into the right schema and
//! is not refused — `ddl::qualified_create` and `refuse_missing_schema` see to that. So the
//! creation half of the harness lane's mechanism is not reproduced by the minimal pair, and the
//! suite's `CREATE UNLOGGED TABLE "articles"` failure needs a different explanation: it is the
//! seed-dependent one (8 of 12, against 12 of 12 for the other test), which points at what the
//! previous test left behind rather than at the statement itself.
//!
//! **[836] does reproduce, and closing it was a decision rather than a lane's call.** An index's
//! name record was keyed on `IndexDef::name` alone where a table's is keyed on its qualified name,
//! so index names shared one namespace across every schema and the collision the suite meets was
//! real *in that model*. Schema-scoping those keys changes a **key layout** rather than a record's
//! contents, which is why it went to the user: schema-scoped keys, no fallback read and no
//! migration, existing databases disposable
//! ([ADR 0080](../../../docs/adr/0080-an-index-name-record-is-scoped-to-its-schema.md)). The test
//! that was written and held back now sits below.
//!
//! An older database is refused rather than misread, by a **layout marker** of its own rather than
//! by a record floor: raising the floor would have turned away version-14 sequence records this
//! change does not touch and inverted nine goldens that promise old table records still decode.
//! `catalog layout 1 is older than 2 …` is the sentence, and the ADR carries the mechanism.
//!
//! It is tempting to read `my.schema` as the cause — a name with a dot in it, a stored name that
//! is `schema ++ NUL ++ name`, a `search_path` parser that might split on the wrong character. It
//! is not: the same pair of statements collides under an ordinary schema name. The dot is only how
//! the suite happened to reach it. Each case below is asserted twice, once in `g1sd.dotted` and
//! once in `g1sd_plain`, so a future reader does not have to re-derive that.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// The suite's shape: fixtures in `public` that the working schema is about to collide with, and a
/// table whose primary-key index is the one that gets renamed.
fn node(schema: &str) -> parity::Node {
    let mut node = parity::Node::new(&[
        "CREATE TABLE public.g1sd_articles (id bigserial primary key, title character varying)",
    ]);
    node.run(&format!("CREATE SCHEMA \"{schema}\"")).unwrap();
    node.run(&format!("SET search_path TO \"{schema}\""))
        .unwrap();
    node.run("CREATE TABLE g1sd_posts (id bigserial primary key)")
        .unwrap();
    node
}

/// Every relation this file makes, as `schema|name|kind`, so an object created in the wrong schema
/// is visible rather than merely absent.
fn relations(node: &mut parity::Node) -> Vec<String> {
    node.rows(
        "SELECT n.nspname, c.relname, c.relkind FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname LIKE 'g1sd%' ORDER BY 1, 2",
    )
    .into_iter()
    .map(|row| row.join("|"))
    .collect()
}

/// **[844]**: an unqualified `CREATE TABLE` goes into the schema on the path, and the same name in
/// `public` is not a collision.
#[test]
fn a_create_collides_only_within_its_own_schema() {
    for schema in ["g1sd.dotted", "g1sd_plain"] {
        let mut node = node(schema);
        node.run("CREATE UNLOGGED TABLE g1sd_articles (id bigserial primary key, title character varying)")
            .unwrap_or_else(|error| panic!("in {schema}: {error}"));
        let seen = relations(&mut node);
        assert!(
            seen.contains(&format!("{schema}|g1sd_articles|r")),
            "in {schema}: {seen:?}"
        );
        assert!(
            seen.contains(&"public|g1sd_articles|r".to_owned()),
            "public's fixture is untouched, in {schema}: {seen:?}"
        );
    }
}

/// **[836]**: an index rename collides only within the index's own schema, and the renamed index
/// stays there.
#[test]
fn an_index_rename_collides_only_within_its_own_schema() {
    for schema in ["g1sd.dotted", "g1sd_plain"] {
        let mut node = node(schema);
        node.run("ALTER INDEX g1sd_posts_pkey RENAME TO g1sd_articles_pkey")
            .unwrap_or_else(|error| panic!("in {schema}: {error}"));
        let seen = relations(&mut node);
        assert!(
            seen.contains(&format!("{schema}|g1sd_articles_pkey|i")),
            "in {schema}: {seen:?}"
        );
        assert!(
            seen.contains(&"public|g1sd_articles_pkey|i".to_owned()),
            "public's index is untouched, in {schema}: {seen:?}"
        );
    }
}

/// **`SchemaWithDotsTest#test_rename_table`, all four statements**, which is the only assertion
/// that covers the three faces at once: the table's rename, the sequence's, and the index's each
/// asked `public` for a bare target, and each was closed on its own. A test per face cannot say
/// they compose, and this sequence is what the suite actually sends.
#[test]
fn the_whole_rename_sequence_runs_in_a_dotted_schema() {
    for schema in ["my.schema", "g1sd_plain"] {
        let mut node = parity::Node::new(&[
            "CREATE TABLE public.articles (id bigserial primary key, title character varying)",
        ]);
        node.run(&format!("CREATE SCHEMA \"{schema}\"")).unwrap();
        node.run(&format!("SET search_path TO \"{schema}\""))
            .unwrap();
        for sql in [
            "CREATE UNLOGGED TABLE \"posts\" (\"id\" bigserial primary key)".to_owned(),
            "ALTER TABLE \"posts\" RENAME TO \"articles\"".to_owned(),
            format!("ALTER TABLE \"{schema}\".\"posts_id_seq\" RENAME TO \"articles_id_seq\""),
            "ALTER INDEX \"posts_pkey\" RENAME TO \"articles_pkey\"".to_owned(),
        ] {
            node.run(&sql)
                .unwrap_or_else(|error| panic!("in {schema}: {sql}\n{error}"));
        }
        // The test's own assertion.
        assert_eq!(
            node.rows(
                "SELECT c.relname FROM pg_class c \
                 LEFT JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = ANY (current_schemas(false)) AND c.relkind IN ('r','p','f') \
                 ORDER BY 1"
            ),
            [["articles"]],
            "in {schema}"
        );
        // And `public`'s three are beside them, untouched — the collision that made each face
        // fail is still present and no longer fires.
        assert_eq!(
            node.rows(
                "SELECT n.nspname, c.relname FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname LIKE '%articles%' ORDER BY 1, 2"
            ),
            [
                vec![schema.to_owned(), "articles".to_owned()],
                vec![schema.to_owned(), "articles_id_seq".to_owned()],
                vec![schema.to_owned(), "articles_pkey".to_owned()],
                vec!["public".to_owned(), "articles".to_owned()],
                vec!["public".to_owned(), "articles_id_seq".to_owned()],
                vec!["public".to_owned(), "articles_pkey".to_owned()],
            ],
            "in {schema}"
        );
    }
}

/// **The collision check is still real** — the half a fix must not lose. Inside the *same* schema
/// both statements are refused, with PostgreSQL's own code and text.
#[test]
fn a_collision_inside_the_same_schema_is_still_refused() {
    for schema in ["g1sd.dotted", "g1sd_plain"] {
        let mut node = node(schema);
        node.run("CREATE TABLE g1sd_articles (id bigserial primary key)")
            .unwrap();
        assert_eq!(
            node.answer("CREATE TABLE g1sd_articles (id bigserial primary key)")
                .to_string(),
            "!42P07 relation \"g1sd_articles\" already exists",
            "in {schema}"
        );
        assert_eq!(
            node.answer("ALTER INDEX g1sd_posts_pkey RENAME TO g1sd_articles_pkey")
                .to_string(),
            "!42P07 relation \"g1sd_articles_pkey\" already exists",
            "in {schema}"
        );
    }
}

/// **`ActiveRecord#tables`**, which is what `test_rename_table` actually asserts: under a
/// `search_path` of one schema it lists that schema's tables and nothing else.
#[test]
fn tables_lists_only_the_schema_on_the_path() {
    for schema in ["g1sd.dotted", "g1sd_plain"] {
        let mut node = node(schema);
        node.run("ALTER TABLE g1sd_posts RENAME TO g1sd_articles")
            .unwrap_or_else(|error| panic!("in {schema}: {error}"));
        assert_eq!(
            node.rows(
                "SELECT c.relname FROM pg_class c \
                 LEFT JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = ANY (current_schemas(false)) AND c.relkind IN ('r','p','f') \
                 ORDER BY 1"
            ),
            [["g1sd_articles"]],
            "in {schema}"
        );
    }
}
