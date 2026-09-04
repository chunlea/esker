//! `DROP EXTENSION` — **the suite's teardown**, 61 tests over 6 files in run 52's ranking.
//!
//! `disable_extension` sends `DROP EXTENSION IF EXISTS "name"` and appends ` CASCADE` for
//! `force: :cascade` (`postgresql_adapter.rb:503`), so this is not a feature any test writes but
//! the statement that runs after the ones that do.
//!
//! The pairing worth knowing: **the verb decides the class.** `CREATE EXTENSION nosuch` is `0A000`
//! — the *server* does not have it — and `DROP EXTENSION nosuch` is `42704`, because this
//! *database* has not installed it. Same name, two classes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is about the catalog.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_available_extensions.name` is a `name` there and `text` here — the standing catalog
    // trade, and the same one `tests/create_extension.rs` records for its own lines.
    types: &[
        "SELECT 'r', name, default_version, installed_version FROM pg_available_extensions WHERE name IN ('hstore','citext','ltree','postgres_fdw') ORDER BY name",
        "SELECT 'r', extname, extversion FROM pg_extension WHERE extname = 'hstore'",
    ],
    answers: &[
        // **The allowlist is deliberate and is the user's call**, not an oversight: this node
        // offers `citext`, `hstore`, `pgcrypto`, `plpgsql` and `uuid-ossp`, and nothing else
        // without a decision — because `CREATE EXTENSION` here means the extension *and* what it
        // promises, so an entry nothing implements would be a name that installs and then fails at
        // the first value. `ltree` and `postgres_fdw` are the two the capture reaches.
        (
            "SELECT 'r', name, default_version, installed_version FROM pg_available_extensions WHERE name IN ('hstore','citext','ltree','postgres_fdw') ORDER BY name",
            "Two rows here rather than four: `ltree` and `postgres_fdw` are not on the allowlist, \
             so they are not available and the view does not claim they are.",
        ),
        (
            "CREATE EXTENSION IF NOT EXISTS \"ltree\"",
            "`0A000 extension \"ltree\" is not available`, with PostgreSQL's own HINT — the answer \
             a real server gives for an extension its *system* does not have, which is exactly \
             this node's position.",
        ),
        // A C1 gap rather than a refusal: `sqlparser` 0.62.0 cannot read the clause at all, so
        // this is `42601` where a real server runs the statement. The lowering already refuses
        // `CREATE EXTENSION ... SCHEMA` by name and never gets the chance.
        (
            "CREATE EXTENSION IF NOT EXISTS \"pgcrypto\" SCHEMA extschema",
            "`42601` from `sqlparser`, which stops at `SCHEMA`. This node has one schema per \
             tenant, so the clause is refused by name in `parse::lower` once a parser can reach \
             it — a C1 gap in the parser rather than a C2 refusal of ours.",
        ),
    ],
};

#[test]
fn every_extension_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_extensions.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **What the capture does not reach: an extension a column still depends on.**
///
/// The capture drops `hstore` while nothing uses it, so it never asks the question this node most
/// needs answered — `hstore` and `citext` are *column types* here, and dropping the extension out
/// from under one would leave a column whose type nothing declares. Measured on the oracle rather
/// than reasoned: `2BP01 cannot drop extension citext because other objects depend on it` with
/// `DETAIL: column c of table ce depends on type citext`, and `CASCADE` takes the column with a
/// `NOTICE`.
///
/// The suite reaches it because `disable_extension(name, force: :cascade)` is what
/// `postgresql_adapter.rb:503` sends for the cascading form.
#[test]
fn an_extension_a_column_uses_needs_cascade() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE EXTENSION IF NOT EXISTS citext").unwrap();
    node.run("CREATE TABLE ce (id bigint PRIMARY KEY, c citext)")
        .unwrap();
    node.run("INSERT INTO ce VALUES (1, 'Abc')").unwrap();

    let refused = node.answer("DROP EXTENSION citext").to_string();
    assert!(
        refused.starts_with("!2BP01"),
        "a column of the type must hold the extension: {refused}"
    );
    assert!(
        refused.contains("column c of table ce depends on type citext"),
        "PostgreSQL's own DETAIL: {refused}"
    );
    // Refused means refused: the extension is still installed and the column still reads.
    assert_eq!(node.rows("SELECT c FROM ce"), [["Abc"]]);

    // `CASCADE` takes the column and leaves the table. The column is tombstoned rather than
    // removed (ADR 0051), so the row that was written before it is still readable — which is what
    // a real server does too, and why `id` still answers.
    node.run("DROP EXTENSION citext CASCADE").unwrap();
    assert_eq!(node.rows("SELECT * FROM ce"), [["1"]]);
    assert!(
        node.answer("SELECT c FROM ce")
            .to_string()
            .starts_with("!42703"),
        "the cascaded column is gone"
    );
    // And the extension is: installing it again is not a duplicate.
    node.run("CREATE EXTENSION citext").unwrap();
}

/// The two spellings for an extension that is not installed, which differ by **verb**.
#[test]
fn dropping_an_extension_that_is_not_there() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.answer("DROP EXTENSION nosuchextension").to_string(),
        "!42704 extension \"nosuchextension\" does not exist"
    );
    // `IF EXISTS` succeeds — the form the suite's teardown always sends.
    node.run("DROP EXTENSION IF EXISTS nosuchextension")
        .unwrap();
    // And an extension the *server* does not have is the other class entirely, from the other verb.
    assert!(
        node.answer("CREATE EXTENSION nosuchextension")
            .to_string()
            .starts_with("!0A000"),
        "the create side is 0A000, not 42704"
    );
}
