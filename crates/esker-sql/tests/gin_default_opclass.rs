//! **A type with a default `gin` operator class needs no class written.**
//!
//! [ADR 0070](../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md)
//! brought `USING gin` in with the rule that only `btree` has a default, so every `gin` index with
//! no class named was `42704 … has no default operator class`. That is right for the type the ADR
//! was written against — `character varying`, which really has none — and wrong for the three
//! types that do.
//!
//! Measured on 19beta1, `pg_opclass` joined to `pg_am` and `pg_type` where `opcdefault`:
//!
//! ```text
//! gin   anyarray   array_ops
//! gin   jsonb      jsonb_ops
//! gin   tsvector   tsvector_ops
//! ```
//!
//! `schema_test.rb` writes both spellings this closes — `USING gin (name_vector)` over a
//! `tsvector` column and `USING gin ((to_tsvector('english', coalesce(things.name, ''))))` over an
//! expression of that type — and they are the last two rows of `corpus/pg19_tsvector.txt` part 3.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE things (id int8, name character varying(50), name_vector tsvector, tags text[])",
];

/// **A `tsvector` key gets past the operator class and is stopped one layer down**, which is the
/// distinction this test exists to pin.
///
/// `tsvector` has a `gin` default and this node now resolves it — so the refusal is no longer
/// `42704 … has no default operator class`. What refuses it is
/// [`esker_keys::row::is_index_key`], because a `tsvector`'s byte order is not its printed order
/// ([ADR 0066](../../../docs/adr/0066-a-tsvector-is-its-canonical-text.md)), and an index key here
/// is an ordered one whatever access method was recorded
/// ([ADR 0070](../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md)).
///
/// **Both of `corpus/pg19_tsvector.txt` part 3's indexes are this shape** — one over the column
/// and one over an expression of the same type — so part 3 cannot replay until the two ADRs are
/// reconciled. That is a decision, not an omission, and it is not one to take inside a test.
#[test]
fn a_tsvector_key_is_refused_by_the_key_rule_and_not_the_class_rule() {
    let mut node = parity::Node::new(FIXTURE);
    let answer = node
        .answer("CREATE INDEX i_col ON things USING gin (name_vector)")
        .to_string();
    assert!(
        answer.contains("is not supported") && !answer.contains("operator class"),
        "expected the key-type refusal, got {answer}"
    );

    // **The expression of that same type is accepted**, which is what a real server does — and is
    // the asymmetry worth staring at: `index_expression` has no `is_index_key` gate, so a
    // `tsvector` reaches the key encoding here where a column of it cannot. Whether that is the
    // column rule being too strict or this path missing a gate is the question in the handover;
    // what this asserts is that the row actually writes, because an index that refuses the first
    // `INSERT` would be worse than one refused at `CREATE`.
    node.run("CREATE INDEX i_expr ON things USING gin ((to_tsvector('english', coalesce(things.name, ''))))")
        .unwrap();
    node.run("INSERT INTO things (id, name) VALUES (1, 'the fat cat')")
        .unwrap();
    assert_eq!(
        node.rows("SELECT name FROM things"),
        [["the fat cat".to_owned()]]
    );
}

/// An array is the second of the three, and `gist` keeps its own answer: it has **no** default for
/// an array, so the same key is refused there — measured, and the pair is what says the default is
/// per method and not per type.
#[test]
fn an_array_has_a_gin_default_and_no_gist_one() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("CREATE INDEX i_arr ON things USING gin (tags)")
            .to_string(),
        "(a command, no result set)"
    );
    assert_eq!(
        node.answer("CREATE INDEX i_arr_g ON things USING gist (tags)")
            .to_string(),
        "!42704 data type text[] has no default operator class for access method \"gist\" \
         HINT: You must specify an operator class for the index or define a default operator \
         class for the data type."
    );
}

/// **The type that really has none keeps its refusal**, which is the rule this must not widen into
/// "every gin index is fine". `character varying` is what ADR 0070 measured.
#[test]
fn a_varchar_still_has_no_gin_default() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("CREATE INDEX i_v ON things USING gin (name)")
            .to_string(),
        "!42704 data type character varying has no default operator class for access method \
         \"gin\" HINT: You must specify an operator class for the index or define a default \
         operator class for the data type."
    );
    // And naming a class it does accept still works, which is ADR 0070's own case.
    assert_eq!(
        node.answer("CREATE INDEX i_v2 ON things USING gin (name gin_trgm_ops)")
            .to_string(),
        "(a command, no result set)"
    );
}
