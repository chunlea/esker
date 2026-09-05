//! A **schema-qualified type name**, against PostgreSQL 19beta1.
//!
//! Run 81's residual. `schema_test.rb`'s `DefaultsUsingMultipleSchemasAndDomainTest` creates
//! `CREATE DOMAIN schema_1.text AS text` and then writes `'some text'::schema_1.text` in a
//! `DEFAULT`, so the name has to resolve in three places: a **column type**, an **expression
//! cast**, and a **`regtype`**. `value::split_type_name` made the parser one; this is the lookup.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus makes its own schema, domain and table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // The standing catalog trade, twice: `column_name`, `udt_name`, `udt_schema` and `typname`
        // are `name` on a real server and `typtype` a `"char"`, and `data_type` is
        // `information_schema`'s own domain — all `text` here, all comparing identically. **Every
        // value agrees**, and the values are what these ask: a column of `schema_9.text` reports
        // `udt_name` `text` and `udt_schema` `pg_catalog`, because both name the *base* type, and
        // the domain itself is `typtype` `d` in the schema it was declared in.
        "SELECT 'r', column_name, data_type, udt_name, udt_schema FROM information_schema.columns \
         WHERE table_name = 'd' ORDER BY ordinal_position",
        "SELECT 'r', typname, typtype FROM pg_type WHERE typname = 'text' AND typnamespace = \
         (SELECT oid FROM pg_namespace WHERE nspname = 'schema_9')",
    ],
    answers: &[
        // **A real server deparses the coercion it inserted, not the cast that was written.**
        // `'x'::schema_9.text` becomes `('x'::text)::schema_9.text` — the literal is `unknown`,
        // PostgreSQL coerces it to the domain's base type and *then* to the domain, and
        // `pg_get_expr` prints both steps with the parentheses that pairing needs. This node
        // prints what was written. The **value is right** — the row two statements down is `x` —
        // and what differs is a second cast that describes a coercion this node performs in one
        // step. Closing it means deparsing a coercion rather than an expression, which is a unit
        // of its own and is what `(0)::bigint` already declares one statement at a time.
        (
            "SELECT 'r', pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = '\"d\"'::regclass",
            "PostgreSQL deparses the coercion it inserted; this node prints the cast as written",
        ),
        // **A user type's oid is this node's own number**, which is the standing trade every
        // relation id makes: PostgreSQL puts everything a user creates above 16383 and this
        // node's ids start at 1. Nothing reads the number — `ActiveRecord` looks a type up by
        // `typname` — and `tests/regtype_user.rs` checks the thing that matters instead, that the
        // oid a `regtype` gives is the one `pg_type` reports for the same name.
        (
            "SELECT 'r', 'schema_9.text'::regtype::oid > 16383",
            "a user type's oid is a relation id here, and relation ids start at 1",
        ),
    ],
};

#[test]
fn every_qualified_type_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_qualified_type.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus did not load"
    );
}
