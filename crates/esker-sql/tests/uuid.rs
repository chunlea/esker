//! `uuid`, against PostgreSQL 19beta1 — nine of the twenty refusals in
//! `postgresql_specific_schema.rb`, and every `id: :uuid` primary key `ActiveRecord` writes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
/// One of `DIVERGENCES`' reasons: a function this node has for no type.
const FUNCTIONS: &str = "A function this node does not implement for any type, named under \
     contract C2 rather than answered. `uuid_cmp` is the comparison this type's order already \
     is; `gen_random_uuid`, `uuidv4` and `uuidv7` are generators, built in on a real server and \
     needing no extension. None is reachable from a storage type's unit — a generator has to be \
     a volatile function the planner knows not to fold, which is its own piece of machinery.";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // `typname` is a `name` and `typcategory` a `"char"` on a real server; both are `text`
        // here with identical characters. `typlen` agrees exactly, as a `smallint`.
        "SELECT oid, typname, typlen, typinput, typcategory FROM pg_type WHERE typname = 'uuid'",
        // A cast **to** `varchar` reports `text` here: the two are one representation told apart
        // by OID, and a cast with no length has nothing to carry the distinction. The value is
        // identical, and `::text` on the line above agrees exactly.
    ],
    answers: &[
        (
            "SELECT pg_typeof('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid), format_type(2950, -1)",
            "`format_type(2950, -1)` agrees — it is `uuid` on both — and `pg_typeof` is not \
             implemented for any type, so the statement is `0A000` naming the function.",
            "pg19_uuid.txt:42",
        ),
        (
            "SELECT 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid::char(8)",
            "**An explicit cast to `character(n)` truncates on a real server and raises `22001` \
             here.** Nothing to do with `uuid` — `::text` and `::varchar` both agree — and the \
             same divergence `tests/time.rs` records for the same reason: it belongs to \
             `bpchar`'s cast path, where an explicit cast and an assignment are the same code.",
            "pg19_uuid.txt:79",
        ),
        (
            "SELECT 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid(10)",
            "Both refuse with `42601`; the text differs. PostgreSQL's *type-name* grammar reads \
             `uuid(10)` and rejects the modifier by name — `type modifier is not allowed for \
             type \"uuid\"` — where `sqlparser` will not parse a modifier on `uuid` at all and \
             stops at the parenthesis. **The `regtype` spelling of the same question already \
             agrees**: `'uuid(10)'::regtype` gives PostgreSQL's own message, because that path \
             resolves the name through `value::type_by_name`. This is the *cast* path, which is \
             `sqlparser`'s grammar and not this node's.",
            "pg19_uuid.txt:84",
        ),
        (
            "SELECT uuid_cmp('a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, \
             '00000000-0000-0000-0000-000000000000'::uuid)",
            FUNCTIONS,
            "pg19_uuid.txt:87",
        ),
        // **This line used to be here** and now agrees, deleted under ADR 0031 rule 2. What kept
        // it diverging was `length`'s **declared type** — `text` where a real server says
        // `integer` — and not the uuid functions, which have answered for a while. The sibling
        // lines below still carry `FUNCTIONS` because they name functions this node does not have.
        (
            "SELECT uuidv4() IS NOT NULL, uuidv7() IS NOT NULL",
            FUNCTIONS,
            "pg19_uuid.txt:91",
        ),
    ],
};

#[test]
fn every_uuid_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_uuid.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `'uuid'::regtype` follows from `ColumnType::ALL`, with nothing written for it.
///
/// The `::regtype` name table used to be a hand-written list of strings, and it drifted: `numeric`
/// and `date` were in `pg_type` — which derives itself — and missing there, so
/// `'decimal(3,2)'::regtype` was `42704` for a type this node would happily create a column of,
/// and every Rails suite file stopped on it. `5dd7b6f` made the two resolve from one array.
///
/// This unit is the first test of that: **not one line of `value::type_by_name` mentions `uuid`**,
/// and every spelling below answers because the type exists. The typmod refusal comes from the
/// same place, with PostgreSQL's own message for a name its grammar has no keyword for.
#[test]
fn the_regtype_probe_follows_the_type_with_nothing_written_for_it() {
    let mut node = parity::Node::new(&[]);

    assert_eq!(node.rows("SELECT 'uuid'::regtype::oid"), vec![vec!["2950"]]);
    assert_eq!(node.rows("SELECT 'UUID'::regtype::oid"), vec![vec!["2950"]]);
    assert_eq!(
        node.rows("SELECT '  uuid  '::regtype::oid"),
        vec![vec!["2950"]]
    );
    assert_eq!(node.rows("SELECT 'uuid'::regtype"), vec![vec!["uuid"]]);
    assert_eq!(
        node.rows("SELECT format_type(2950, -1)"),
        vec![vec!["uuid"]]
    );

    // A uuid takes no typmod, and the refusal is PostgreSQL's own — `uuid` is not one of the
    // spellings its grammar has a keyword for, so it is the "not allowed" message and not the
    // syntax error `integer(4)` and `json(10)` get.
    let error = node.run("SELECT 'uuid(10)'::regtype::oid").unwrap_err();
    assert_eq!(error.sqlstate(), "42601");
    assert_eq!(
        error.to_string(),
        "type modifier is not allowed for type \"uuid\""
    );

    // And the catalog it derives from carries the row it derives from.
    assert_eq!(
        node.rows("SELECT oid, typname, typlen, typcategory FROM pg_type WHERE typname = 'uuid'"),
        vec![vec!["2950", "uuid", "16", "U"]]
    );
}
