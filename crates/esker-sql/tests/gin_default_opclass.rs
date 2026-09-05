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
//!
//! # Provenance
//!
//! The access-method rows are `corpus/pg19_gin_tsvector.txt`, taken by the harness lane against
//! PostgreSQL 19beta1 in one `BEGIN … ROLLBACK` at this lane's request — because the brief for
//! this change assumed a `tsvector` under `USING btree` raised `42704`, and it does not. It is
//! **not replayed** here: three of its rows are statements PostgreSQL accepts and this node
//! refuses, and a refusal aborts the transaction and swallows the rest of the file. The rows are
//! asserted one at a time instead, which is what `storage_parameters.rs` does and for the same
//! reason.
//!
//! The capture also bounds the guard the brief wanted. The `42704` is real — it belongs to `hash`
//! and `brin`, where a `tsvector` genuinely has no default class — and `tsquery` turns out to have
//! a `btree` default as well, so it is not the counter-example either.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE things (id int8, name character varying(50), name_vector tsvector, tags text[])",
];

/// **A `tsvector` column is a `gin` index key**, which is the shape `schema_test.rb` builds and
/// the last statement `corpus/pg19_tsvector.txt` part 3 was stopping on.
///
/// Captured on PostgreSQL 19beta1 in one `BEGIN … ROLLBACK`, and the surprise is in the third row:
///
/// ```text
/// CREATE INDEX gtv_gin_col   ON gtv USING gin (tsv)     ok
/// CREATE INDEX gtv_gin_expr  ON gtv USING gin ((to_tsvector('english', coalesce(gtv.name, ''))))  ok
/// CREATE INDEX gtv_btree_col ON gtv USING btree (tsv)   ok      <- accepted there, refused here
/// ```
///
/// `pg_opclass` says why: `tsvector_ops` is the **default** class for `btree`, `gin` *and* `gist`,
/// so a real server takes all three. This node takes only `gin`, and that is a **declared
/// divergence rather than PostgreSQL's error**: under `btree` the order of the key is the index,
/// and this node's order for a `tsvector` is its bytes' rather than `tsvector_ops`'
/// ([ADR 0066](../../../docs/adr/0066-a-tsvector-is-its-canonical-text.md)). Under `gin` the order
/// is never read, which is what the amendment says and what makes the column safe there.
#[test]
fn a_tsvector_column_is_a_gin_key_and_only_a_gin_key() {
    let mut node = parity::Node::new(FIXTURE);
    // The two the capture accepts and this node now accepts: the column and the expression.
    for sql in [
        "CREATE INDEX i_col ON things USING gin (name_vector)",
        "CREATE INDEX i_expr ON things USING gin ((to_tsvector('english', coalesce(things.name, ''))))",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "(a command, no result set)",
            "{sql}"
        );
    }
    // And the row that writes, because an index refused at the first `INSERT` would be worse than
    // one refused at `CREATE`.
    node.run("INSERT INTO things (id, name, name_vector) VALUES (1, 'the fat cat', to_tsvector('english', 'the fat cat'))")
        .unwrap();
    assert_eq!(
        node.rows("SELECT name FROM things"),
        [["the fat cat".to_owned()]]
    );
}

/// **`btree` over a `tsvector` stays refused, and by our own sentence.**
///
/// PostgreSQL accepts it — `tsvector_ops` is `btree`'s default class too — so this is a divergence
/// and it says so with `0A000` naming the construct, contract C2. It must **not** be spelled
/// `42704 … has no default operator class`, which is the error a real server does not give: the
/// brief for this change assumed it did, and `pg_opclass` says otherwise.
///
/// **This is the one method where the order matters**, which is what keeps the `gin`/`gist`
/// widening from being "every method is fine": a btree index *is* its key order, and this node's
/// order for a `tsvector` is its bytes' rather than `tsvector_ops`'.
///
/// `gist` used to be refused here beside them and is not any more — see
/// [`a_tsvector_column_is_a_gin_key_and_only_a_gin_key`]'s sibling below.
///
/// **`hash` and `brin` are where the `42704` really lives**, measured: a `tsvector` has no default
/// class for either. This node refuses those methods outright, one statement earlier, so it never
/// reaches the type — a different sentence for a different reason, and both are honest.
#[test]
fn btree_over_a_tsvector_is_a_declared_divergence() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "CREATE INDEX i_bt ON things USING btree (name_vector)",
        "CREATE INDEX i_bare ON things (name_vector)",
    ] {
        let answer = node.answer(sql).to_string();
        assert_eq!(
            answer, "!0A000 an index on a column of type tsvector is not supported",
            "{sql}"
        );
        assert!(
            !answer.contains("operator class"),
            "PostgreSQL gives no operator-class error here: {answer}"
        );
    }
    // `hash` and `brin` are refused for the method before the type is ever looked at, which is
    // ADR 0070's rule and not this one's. PostgreSQL refuses them too, for the type — the same
    // outcome by a different route, and the capture holds both sentences.
    for sql in [
        "CREATE INDEX i_h ON things USING hash (name_vector)",
        "CREATE INDEX i_br ON things USING brin (name_vector)",
    ] {
        let answer = node.answer(sql).to_string();
        assert!(
            answer.starts_with("!0A000 an index USING"),
            "expected the access-method refusal, got {answer} for {sql}"
        );
    }
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

/// **`gist` over a `tsvector` is the same case as `gin`, and now answers the same.**
///
/// `tsvector_ops` is `gist`'s default class too — `corpus/pg19_gin_tsvector.txt` records
/// `CREATE INDEX gtv_gist_col ON gtv USING gist (tsv)` as accepted — and a `gist` index is not read
/// in key order any more than a GIN one is, so ADR 0066's amendment covers it word for word. It
/// was left refused at v32 only because nothing exercised it; the capture row was already there.
///
/// **`btree` stays refused**, which is what keeps this from being "every method is fine": there
/// the order of the key *is* the index.
#[test]
fn gist_over_a_tsvector_answers_as_gin_does() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("CREATE INDEX i_gist ON things USING gist (name_vector)")
            .to_string(),
        "(a command, no result set)"
    );
    // And it writes, the same assertion the gin column key carries.
    node.run(
        "INSERT INTO things (id, name, name_vector) VALUES (1, 'a', to_tsvector('english', 'a'))",
    )
    .unwrap();
    assert_eq!(node.rows("SELECT name FROM things"), [["a".to_owned()]]);
}
