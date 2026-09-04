//! `ALTER TABLE … ALTER COLUMN … TYPE` — which conversions PostgreSQL 19 takes without help.
//!
//! **Run 59: 13 tests over 2 files.** The files are `adapters/postgresql/change_schema_test.rb`
//! and `array_test.rb` — and there are **two** files named `change_schema_test.rb`, so the
//! ranking's basename does not say which; the timestamp row is the `adapters/postgresql` one, and
//! running `migration/change_schema_test.rb` instead measures a different set of statements.
//!
//! Twelve distinct `ALTER COLUMN … TYPE` statements were logged running those files against
//! PostgreSQL 19. Every timestamp conversion carries an explicit `USING CAST(…)`, because the
//! column is `character varying` and that pair has no assignment cast.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    // **A refusal by name, and the boundary this unit draws on purpose.**
    //
    // `USING CAST(c AS t)` and `USING c::t` say "convert this column to that type", which is the
    // conversion the statement already names — so they license it and nothing has to be evaluated.
    // `USING string_to_array(c, ',')` asks for a **computation** over each row, and this crate has
    // no per-row expression evaluator to run one with (`crate::parse::lower_cast`: "a cast of a
    // column has to happen per row and this node has no expression-level cast to do it with").
    //
    // Running the type change and ignoring the expression would be the wrong-answer shape: the
    // column would end up `text[]` with every row holding a one-element array of the whole string
    // rather than the split one. So it is `0A000` naming the expression, which is contract C2, and
    // it costs one test in `array_test.rb`.
    answers: &[(
        "ALTER TABLE \"pg_arrays\" ALTER COLUMN \"snippets\" TYPE text[] USING \
             string_to_array(\"snippets\", \',\'), ALTER COLUMN \"snippets\" SET DEFAULT \'{}\';",
        "`0A000` naming the expression: a `USING` that computes rather than converts needs a \
             per-row evaluator this node does not have, and ignoring it would silently store a \
             different value in every row.",
    )],
};

#[test]
fn every_alter_column_type_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_alter_column_type.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 80,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **`CREATE UNLOGGED TABLE` works, and the row that said otherwise named the wrong feature.**
///
/// Run 59 ranked `CREATE UNLOGGED is not supported` at 10 tests over 2 files. The keyword is cut
/// out of the source and the statement parses (`crate::parse::strip_unlogged`), `relpersistence`
/// answers `u`, and none of that changed in this unit — what those ten tests actually hit is a
/// **virtual generated column**, which this parser has no grammar for. The refusal table matched
/// `CREATE UNLOGGED` on the original words and reported that instead, sending the reader to a
/// feature that works.
///
/// The rule counts rather than searches, because one `CREATE TABLE` declares both kinds: the
/// suite's `virtual_columns` has three `STORED` columns and two virtual ones.
#[test]
fn an_unlogged_table_works_and_a_virtual_column_names_itself() {
    let mut node = parity::Node::new(&[]);
    // Unlogged, with a stored generated column: this has worked all along.
    node.run(
        "CREATE UNLOGGED TABLE vc (id bigint PRIMARY KEY, c1 integer, c2 integer GENERATED ALWAYS \
         AS (c1 + 1) STORED)",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT relpersistence FROM pg_class WHERE relname = 'vc'"),
        [["u"]],
        "the flag is stored and reported; nothing here is actually unlogged"
    );
    node.run("CREATE TABLE lg (id bigint PRIMARY KEY)").unwrap();
    assert_eq!(
        node.rows("SELECT relpersistence FROM pg_class WHERE relname = 'lg'"),
        [["p"]]
    );

    // A virtual generated column names **itself**, in both spellings and on both persistences —
    // and in a table that also has a stored one, which is the case the first rule missed.
    for written in [
        "CREATE TABLE v1 (id bigint PRIMARY KEY, c1 integer, c3 integer GENERATED ALWAYS AS (c1 + \
         2))",
        "CREATE TABLE v2 (id bigint PRIMARY KEY, c1 integer, c3 integer GENERATED ALWAYS AS (c1 + \
         2) VIRTUAL)",
        "CREATE UNLOGGED TABLE v3 (id bigint PRIMARY KEY, c1 integer, c2 integer GENERATED ALWAYS \
         AS (c1 + 1) STORED, c3 integer GENERATED ALWAYS AS (c1 + 2) VIRTUAL)",
    ] {
        let error = node.run(written).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "for {written}");
        assert!(
            error.to_string().contains("virtual generated column"),
            "the refusal names the real gap rather than UNLOGGED: {error}"
        );
    }
}
