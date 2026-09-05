//! **A foreign key whose target is in another schema, and the three ways `ActiveRecord` asks.**
//!
//! `SchemaForeignKeyTest` is three tests over one shape: `wagons.train_id` references
//! `my_schema.trains`, with the *referencing* table in `public`, in `my_schema`, and in a third
//! schema. Two want `foreign_key_exists?`; the first wants the dumper to print
//! `add_foreign_key "wagons", "my_schema.trains"`.
//!
//! Both answers come out of one query — `foreign_keys(table)` in
//! `postgresql/schema_statements.rb` — and it discriminates on **`c.connamespace`**:
//!
//! ```text
//! WHERE c.contype = 'f' AND t1.relname = 'wagons' AND n.nspname = <the schema asked for>
//! ```
//!
//! Which is why the fixture below puts a table called `wagons` in all three schemas. Nothing
//! about the *name* separates them; only the constraint's namespace does. A `connamespace` that
//! answers a constant would pass a one-schema test and return the wrong constraint here.
//!
//! Measured on 19beta1, under the default `search_path` of `"$user", public`: all three rows
//! report `to_table` as `my_schema.trains`, qualified, because `my_schema` is off the path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE SCHEMA my_schema",
    "CREATE SCHEMA my_other_schema",
    "CREATE TABLE my_schema.trains (id bigserial primary key, name character varying)",
    "CREATE TABLE wagons (id bigserial primary key, train_id integer)",
    "CREATE TABLE my_schema.wagons (id bigserial primary key, train_id integer)",
    "CREATE TABLE my_other_schema.wagons (id bigserial primary key, train_id integer)",
    // `add_foreign_key` in the three shapes the suite writes, quoted the way it quotes them.
    "ALTER TABLE \"wagons\" ADD CONSTRAINT \"fk_rails_a1\" FOREIGN KEY (\"train_id\") \
     REFERENCES \"my_schema\".\"trains\" (\"id\")",
    "ALTER TABLE \"my_schema\".\"wagons\" ADD CONSTRAINT \"fk_rails_b2\" FOREIGN KEY (\"train_id\") \
     REFERENCES \"my_schema\".\"trains\" (\"id\")",
    "ALTER TABLE \"my_other_schema\".\"wagons\" ADD CONSTRAINT \"fk_rails_c3\" \
     FOREIGN KEY (\"train_id\") REFERENCES \"my_schema\".\"trains\" (\"id\")",
];

/// `ActiveRecord`'s `foreign_keys` query, verbatim, with the schema left to the caller.
fn foreign_keys(schema: &str) -> String {
    format!(
        "SELECT t2.oid::regclass::text AS to_table, c.conname AS name, c.confupdtype AS on_update, \
         c.confdeltype AS on_delete, c.convalidated AS valid, c.condeferrable AS deferrable, \
         c.condeferred AS deferred, \
         ( SELECT array_agg(a.attname ORDER BY idx) FROM ( SELECT idx, c.conkey[idx] AS conkey_elem \
         FROM generate_subscripts(c.conkey, 1) AS idx ) indexed_conkeys \
         JOIN pg_attribute a ON a.attrelid = t1.oid AND a.attnum = indexed_conkeys.conkey_elem ) \
         AS conkey_names, \
         ( SELECT array_agg(a.attname ORDER BY idx) FROM ( SELECT idx, c.confkey[idx] AS confkey_elem \
         FROM generate_subscripts(c.confkey, 1) AS idx ) indexed_confkeys \
         JOIN pg_attribute a ON a.attrelid = t2.oid AND a.attnum = indexed_confkeys.confkey_elem ) \
         AS confkey_names \
         FROM pg_constraint c JOIN pg_class t1 ON c.conrelid = t1.oid \
         JOIN pg_class t2 ON c.confrelid = t2.oid JOIN pg_namespace n ON c.connamespace = n.oid \
         WHERE c.contype = 'f' AND t1.relname = 'wagons' AND n.nspname = '{schema}' \
         ORDER BY c.conname"
    )
}

/// `test_dump_foreign_key_targeting_different_schema` — the row the dumper prints
/// `add_foreign_key "wagons", "my_schema.trains"` from.
#[test]
fn a_public_table_reports_its_target_qualified() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.rows(&foreign_keys("public")),
        [[
            "my_schema.trains".to_owned(),
            "fk_rails_a1".to_owned(),
            "a".to_owned(),
            "a".to_owned(),
            "t".to_owned(),
            "f".to_owned(),
            "f".to_owned(),
            "{train_id}".to_owned(),
            "{id}".to_owned(),
        ]]
    );
}

/// `test_create_foreign_key_same_schema` — referencing and referenced in one schema, and the
/// target still prints qualified, because the qualifier is about the `search_path` and not
/// about whether the two agree.
#[test]
fn a_target_in_the_same_off_path_schema_still_prints_qualified() {
    let mut node = parity::Node::new(FIXTURE);
    let rows = node.rows(&foreign_keys("my_schema"));
    assert_eq!(rows.len(), 1, "one constraint, not three: {rows:?}");
    assert_eq!(
        (rows[0][0].as_str(), rows[0][1].as_str()),
        ("my_schema.trains", "fk_rails_b2")
    );
}

/// `test_create_foreign_key_different_schemas` — and the third `wagons`, which exists only to
/// prove the query is answering about a namespace rather than about a name.
#[test]
fn a_third_schema_gets_its_own_constraint_and_not_anothers() {
    let mut node = parity::Node::new(FIXTURE);
    let rows = node.rows(&foreign_keys("my_other_schema"));
    assert_eq!(rows.len(), 1, "one constraint, not three: {rows:?}");
    assert_eq!(
        (rows[0][0].as_str(), rows[0][1].as_str()),
        ("my_schema.trains", "fk_rails_c3")
    );
}

/// The constraint has to *work*, not merely be described: a cross-schema reference is checked on
/// insert and on delete like any other.
///
/// The messages are the point. **Every relation a `23503` names is printed bare**, whatever
/// schema it lives in — measured on 19beta1 with the child in one schema and the parent in
/// another, on both messages, for a plain insert, a delete, a deferred check at `COMMIT`, and the
/// scan `ADD CONSTRAINT` runs. That is not the rule everywhere: `42P01` quotes the qualifier back.
/// Printing the stored name instead put its NUL separator inside the quotes.
#[test]
fn a_cross_schema_reference_is_enforced() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("INSERT INTO my_schema.trains (id, name) VALUES (1, 'flying scotsman')")
        .unwrap();
    node.run("INSERT INTO wagons (id, train_id) VALUES (1, 1)")
        .unwrap();
    assert_eq!(
        node.answer("INSERT INTO wagons (id, train_id) VALUES (2, 99)")
            .to_string(),
        "!23503 insert or update on table \"wagons\" violates foreign key constraint \
         \"fk_rails_a1\" DETAIL: Key (train_id)=(99) is not present in table \"trains\".",
        "a target in another schema is still a target, and is named without the schema"
    );
    assert_eq!(
        node.answer("DELETE FROM my_schema.trains WHERE id = 1")
            .to_string(),
        "!23503 update or delete on table \"trains\" violates foreign key constraint \
         \"fk_rails_a1\" on table \"wagons\" \
         DETAIL: Key (id)=(1) is still referenced from table \"wagons\"."
    );
}

/// **The half the suite's own shape cannot reach**: with the *referencing* table in `public`,
/// the name in the leading clause is bare whether or not anyone strips a schema from it. So this
/// puts both tables outside `public`, which is where a stored name would show through.
#[test]
fn both_sides_outside_public_are_still_named_bare() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA sa",
        "CREATE SCHEMA sb",
        "CREATE TABLE sa.trains (id bigint primary key)",
        "CREATE TABLE sb.wagons (id bigint primary key, train_id bigint)",
        "ALTER TABLE sb.wagons ADD CONSTRAINT fk_x FOREIGN KEY (train_id) \
         REFERENCES sa.trains (id)",
        "INSERT INTO sa.trains VALUES (1)",
        "INSERT INTO sb.wagons VALUES (1, 1)",
    ]);
    assert_eq!(
        node.answer("INSERT INTO sb.wagons VALUES (2, 99)")
            .to_string(),
        "!23503 insert or update on table \"wagons\" violates foreign key constraint \"fk_x\" \
         DETAIL: Key (train_id)=(99) is not present in table \"trains\"."
    );
    assert_eq!(
        node.answer("DELETE FROM sa.trains WHERE id = 1")
            .to_string(),
        "!23503 update or delete on table \"trains\" violates foreign key constraint \"fk_x\" \
         on table \"wagons\" \
         DETAIL: Key (id)=(1) is still referenced from table \"wagons\"."
    );
}

/// The other two paths that build the same message — the scan `ADD CONSTRAINT` runs over rows
/// that are already there, and a `DEFERRABLE INITIALLY DEFERRED` check asked at `COMMIT`.
#[test]
fn the_validate_and_deferred_paths_name_it_bare_too() {
    let mut node = parity::Node::new(&[
        "CREATE SCHEMA sa",
        "CREATE TABLE sa.trains (id bigint primary key)",
        "CREATE TABLE sa.wagons (id bigint, train_id bigint)",
        "INSERT INTO sa.wagons VALUES (1, 55)",
    ]);
    assert_eq!(
        node.answer(
            "ALTER TABLE sa.wagons ADD CONSTRAINT fk_z FOREIGN KEY (train_id) \
             REFERENCES sa.trains (id)"
        )
        .to_string(),
        "!23503 insert or update on table \"wagons\" violates foreign key constraint \"fk_z\" \
         DETAIL: Key (train_id)=(55) is not present in table \"trains\"."
    );

    let mut node = parity::Node::new(&[
        "CREATE SCHEMA sa",
        "CREATE TABLE sa.trains (id bigint primary key)",
        "CREATE TABLE sa.wagons (id bigint, train_id bigint, CONSTRAINT fk_y FOREIGN KEY \
         (train_id) REFERENCES sa.trains (id) DEFERRABLE INITIALLY DEFERRED)",
    ]);
    node.run("BEGIN").unwrap();
    node.run("INSERT INTO sa.wagons VALUES (3, 77)").unwrap();
    assert_eq!(
        node.answer("COMMIT").to_string(),
        "!23503 insert or update on table \"wagons\" violates foreign key constraint \"fk_y\" \
         DETAIL: Key (train_id)=(77) is not present in table \"trains\".",
        "the check deferred to COMMIT builds its message the same way"
    );
}
