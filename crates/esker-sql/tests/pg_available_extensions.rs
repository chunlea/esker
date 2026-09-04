//! `pg_available_extensions`, against PostgreSQL 19beta1 — the relation every suite file stopped
//! on.
//!
//! Not a `schema.rb` statement, which is why the 699-statement schema probe reported zero
//! refusals while no file loaded: `test/cases/helper.rb` reaches it around the schema load,
//! through `ActiveRecord`'s `extension_available?` and `extension_enabled?`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a catalog read.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // `relkind` is a `"char"` and `relname` a `name` on a real server; both are `text` here,
        // with identical characters. The trade every `pg_catalog` column makes.
        "SELECT relkind, relname FROM pg_class WHERE relname = 'pg_available_extensions'",
        // `name` is of type `name` there and `text` here, so the first column's declared type
        // differs while every value agrees.
        "SELECT name, default_version, installed_version FROM pg_available_extensions WHERE name \
         = 'plpgsql'",
    ],
    answers: &[
        // **`hstore` was three entries here and is now one.** The `CREATE EXTENSION` unit had
        // taken it off the available list on the rule that an entry tells a client this server
        // has something, and hstore brings a type — so it went back on in the commit that built
        // the type, and two of the three started agreeing. The one left is the *version*: a real
        // server's hstore is 1.8 and so is this build's, but `default_version` and
        // `installed_version` are read together and the second is NULL here until an install.
        (
            "SELECT name, default_version, installed_version FROM pg_available_extensions WHERE \
             name = 'hstore'",
            "read directly, `installed_version` differs from the two probes above it",
        ),
        (
            "SELECT column_name, data_type FROM information_schema.columns WHERE table_name = \
             'pg_available_extensions' ORDER BY ordinal_position",
            "**`information_schema.columns` describes the tenant's tables, not the catalog's own \
             views**, so this is no rows where a real server lists the five. `pg_class` does now \
             carry a row for every catalog view — that one `ActiveRecord` asks for by name — and \
             extending the same treatment to the column view would still diverge on content: \
             every column here would report `text` where a real server reports `name` for the \
             first. Nothing reads it; the two methods this view exists for read the view itself.",
        ),
        (
            "SELECT extname, extversion FROM pg_extension ORDER BY extname",
            "**`pg_extension` is empty here and has no `extversion` column**, which is a standing \
             decision of its own unit: a row there would tell a client `CREATE FUNCTION … \
             LANGUAGE plpgsql` will work, and there is no procedural language on this node. It \
             sits uneasily beside this view, which reports `plpgsql` as *installed* because \
             `ActiveRecord`'s `extension_enabled?` gates the whole suite on it — the two catalogs \
             disagree about `plpgsql`, deliberately, and the disagreement is recorded here rather \
             than resolved by inventing a `pg_extension` row nothing else could honour.",
        ),
        (
            "SELECT 'g', pg_typeof(name), pg_typeof(default_version), pg_typeof(installed_version) \
             FROM pg_available_extensions WHERE name = 'plpgsql'",
            "`pg_typeof` is not implemented for any type, so this is `0A000` naming the function. \
             It is in the corpus because it is the statement that would *prove* the `name`-vs-\
             `text` divergence declared above.",
        ),
    ],
};

#[test]
fn every_pg_available_extensions_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_pg_available_extensions.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 12,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The two `ActiveRecord` methods that stopped all 367 suite files, and the three answers.
///
/// `extension_available?` and `extension_enabled?` are one line each in `postgresql_adapter.rb`,
/// and they send the same query shape. `query_value` maps "no rows" to `nil`, which is how one
/// shape carries three outcomes — so a view returning a row for every name, or none for an
/// uninstalled one, would look right in one case and be wrong in the other two.
#[test]
fn one_query_shape_carries_three_answers() {
    let mut node = parity::Node::new(&[]);

    // Installed: `installed_version IS NOT NULL` is `t`.
    assert_eq!(
        node.rows("SELECT installed_version IS NOT NULL FROM pg_available_extensions WHERE name = 'plpgsql'"),
        vec![vec!["t"]]
    );
    // Available and not installed: `f`, and a real `default_version` beside a NULL version.
    //
    // **`pgcrypto` rather than `hstore`.** The `CREATE EXTENSION` unit set this build's available
    // list to exactly what `postgresql_specific_schema.rb` needs in order to load — `uuid-ossp`
    // and `pgcrypto` — and took `hstore` off it, because an entry here tells a client this server
    // has something and `hstore` brings a type this node does not have. The three-answer shape
    // this test exists for is unchanged; only the name carrying the middle answer moved.
    assert_eq!(
        node.rows("SELECT installed_version IS NOT NULL FROM pg_available_extensions WHERE name = 'pgcrypto'"),
        vec![vec!["f"]]
    );
    assert_eq!(
        node.rows("SELECT default_version, installed_version FROM pg_available_extensions WHERE name = 'pgcrypto'"),
        vec![vec!["1.4", "\\N"]]
    );
    // Unknown: **no row at all** — not `f`, and not an error.
    assert!(
        node.rows("SELECT installed_version IS NOT NULL FROM pg_available_extensions WHERE name = 'nosuchextension'")
            .is_empty()
    );
    // `extension_available?`'s own shape, which is `true` for both of the two and nothing else.
    assert_eq!(
        node.rows("SELECT true FROM pg_available_extensions WHERE name = 'pgcrypto'"),
        vec![vec!["t"]]
    );
    assert!(
        node.rows("SELECT true FROM pg_available_extensions WHERE name = 'nosuchextension'")
            .is_empty()
    );

    // And it is a **view** in `pg_class`, which is what `relkind IN ('r','p')` table lists rely on
    // to leave it out: the catalog now describes itself, and describes itself correctly.
    assert_eq!(
        node.rows("SELECT relkind FROM pg_class WHERE relname = 'pg_available_extensions'"),
        vec![vec!["v"]]
    );
}
