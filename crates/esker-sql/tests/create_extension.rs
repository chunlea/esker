//! `CREATE EXTENSION` — statement 703, and rung 4's stopper.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is about the catalog.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `extname` and `name` are `name` on a real server and `text` here — the standing trade.
    types: &[],
    // **This list was one entry and is now none.** It held `uuid_generate_v4()`, on the argument
    // that `CREATE EXTENSION` records an install and does not bring the functions — and the user
    // ruled the other way: the allowlist means the extension *and* what it promises, because a
    // schema whose next line defaults a column to `uuid_generate_v4()` stops on that instead. The
    // functions landed in the commit before this one, the line agrees, and the entry is deleted
    // rather than kept (ADR 0031's rule 2).
    answers: &[],
};

#[test]
fn every_create_extension_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_create_extension.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 11,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `IF NOT EXISTS` covers **existence**, not availability.
///
/// The clause turns an already-installed extension from `42710` into a success, and does nothing
/// at all for a name the server does not have — that stays `0A000` with the same HINT either way.
/// An implementation that read the clause as "never fail" would swallow a typo'd extension name
/// and load a schema whose columns then do not work.
#[test]
fn if_not_exists_covers_existence_and_not_availability() {
    let mut node = parity::Node::new(&[]);
    node.run("CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"")
        .unwrap();
    // Existence: covered.
    node.run("CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"")
        .unwrap();
    let error = node.run("CREATE EXTENSION \"uuid-ossp\"").unwrap_err();
    assert_eq!(error.sqlstate(), "42710");
    // Availability: not covered, with or without the clause.
    for written in [
        "CREATE EXTENSION IF NOT EXISTS \"nosuchextension\"",
        "CREATE EXTENSION \"nosuchextension\"",
    ] {
        let error = node.run(written).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "for {written}");
        assert_eq!(
            error.to_string(),
            "extension \"nosuchextension\" is not available"
        );
        assert!(
            error
                .hint()
                .is_some_and(|hint| hint.contains("installed on the system")),
            "the HINT names what is missing: {error}"
        );
    }
}

/// The two extension views are **one fact read two ways**, and they have to agree.
///
/// `pg_available_extensions.installed_version` and the existence of a `pg_extension` row are the
/// same claim; before this unit they disagreed, because one was a constant that said `plpgsql` was
/// installed and the other was a view that held nothing at all.
#[test]
fn the_two_extension_views_agree() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT extname, extversion FROM pg_extension ORDER BY extname"),
        [["plpgsql", "1.0"]],
        "installed before anything runs, as on a real server"
    );
    node.run("CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT name, default_version, installed_version FROM pg_available_extensions ORDER \
             BY name"
        ),
        vec![
            // `citext` and `hstore` joined the available list when their types were built — an
            // entry here tells a client this server has something, so a type-bearing extension
            // goes on with the type and not before.
            vec!["citext", "1.8", "\\N"],
            vec!["hstore", "1.8", "\\N"],
            // `ltree` joined them with `ltree_test.rb`'s type, at the oracle's own 1.3 where the
            // two string extensions are at 1.8.
            vec!["ltree", "1.3", "\\N"],
            // `pg_trgm` joined them with the operator-class surface (ADR 0070), at the oracle's
            // own 1.6. Its *functions* are a named gap; what it brings here is `gin_trgm_ops` and
            // `gist_trgm_ops`, which is what `schema_test.rb` writes.
            vec!["pg_trgm", "1.6", "\\N"],
            vec!["pgcrypto", "1.4", "\\N"],
            vec!["plpgsql", "1.0", "1.0"],
            vec!["uuid-ossp", "1.1", "1.1"],
        ],
        "the version it installed at is the one the other view offered"
    );
    assert_eq!(node.rows("SELECT count(*) FROM pg_extension"), [["2"]]);
}

/// An installed extension **survives**, because a real server's does.
#[test]
fn an_installed_extension_is_a_catalog_write() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "BEGIN",
        "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"",
        "ROLLBACK",
    ] {
        node.run(statement).unwrap();
    }
    // It went back with the transaction, like any other catalog write.
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_extension WHERE extname = 'uuid-ossp'"),
        [["0"]]
    );
    node.run("CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"")
        .unwrap();
    assert_eq!(
        node.rows("SELECT count(*) FROM pg_extension WHERE extname = 'uuid-ossp'"),
        [["1"]]
    );
}
