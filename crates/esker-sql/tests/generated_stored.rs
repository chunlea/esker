//! `GENERATED ALWAYS AS (expr) STORED` — statement 741.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing `pg_catalog` trade: `attname` and `column_name` are `name` on a real server,
    // `attgenerated` is a `"char"` and the `information_schema` columns are `character varying`;
    // all of them are `text` here. **Every row agrees**, `s` and `ALWAYS` included.
    types: &[
        "SELECT attname, attgenerated, attnotnull FROM pg_attribute WHERE attrelid = 'gen'::regclass AND attnum > 0 ORDER BY attnum",
    ],
    answers: &[],
};

#[test]
fn every_generated_stored_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_generated_stored.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// **Two sentences, one SQLSTATE** — and an implementation that reused one is wrong half the time.
#[test]
fn insert_and_update_refuse_it_differently() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE gen (id int8 PRIMARY KEY, name text, upper_name text GENERATED ALWAYS AS \
         (upper(name)) STORED)",
        "INSERT INTO gen (id, name) VALUES (1, 'ada')",
    ]);
    let error = node
        .run("INSERT INTO gen (id, name, upper_name) VALUES (2, 'bob', 'BOB')")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "428C9");
    assert_eq!(
        error.to_string(),
        "cannot insert a non-DEFAULT value into column \"upper_name\""
    );
    let update = node
        .run("UPDATE gen SET upper_name = 'X' WHERE id = 1")
        .unwrap_err();
    assert_eq!(update.sqlstate(), "428C9");
    assert_eq!(
        update.to_string(),
        "column \"upper_name\" can only be updated to DEFAULT"
    );
    // The same DETAIL under both, which is what says *why*.
    for error in [&error, &update] {
        assert_eq!(
            error.detail().as_deref(),
            Some("Column \"upper_name\" is a generated column.")
        );
    }
    // And `DEFAULT` is accepted where a value is not, in both.
    node.run("INSERT INTO gen (id, name, upper_name) VALUES (2, 'bob', DEFAULT)")
        .unwrap();
    node.run("UPDATE gen SET upper_name = DEFAULT WHERE id = 1")
        .unwrap();
    assert_eq!(
        node.rows("SELECT id, upper_name FROM gen ORDER BY id"),
        vec![vec!["1", "ADA"], vec!["2", "BOB"]],
        "DEFAULT recomputes rather than storing NULL"
    );
}

/// **Every shape the deparser now stores, computed** — because the corpus only reads the catalog.
///
/// `tests/generated_parens.rs` asserts what `pg_get_expr` prints, and printing is not the whole
/// contract: the stored string is parsed again to compute the column's value on every write
/// ([ADR 0088](../../../docs/adr/0088-a-stored-expression-is-deparsed-by-the-statement-that-writes-it.md)),
/// so a printed form that reads back as something else is a wrong *value* and not a wrong string.
/// `exec::ddl::reads_back` is the guard on the write path; this is the assertion from the other
/// side, and it is here rather than there because a corpus row cannot insert.
///
/// The shapes are the ones that unit newly routed through the deparser — an operator at depth, a
/// concatenation, a comparison, a `LIKE`, an `IN`, a cast and a nested call — one column each and
/// one row through all of them at once.
#[test]
fn every_deparsed_shape_still_computes_its_value() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE gsv (id int8 PRIMARY KEY, c1 integer, c2 integer, t text)",
        "ALTER TABLE gsv ADD COLUMN g_depth integer GENERATED ALWAYS AS (c1 * 2 + 3) STORED",
        "ALTER TABLE gsv ADD COLUMN g_cat text GENERATED ALWAYS AS (t || 'x') STORED",
        "ALTER TABLE gsv ADD COLUMN g_cmp boolean GENERATED ALWAYS AS (c1 > 0 AND c2 > 0) STORED",
        "ALTER TABLE gsv ADD COLUMN g_like boolean GENERATED ALWAYS AS (t LIKE 'a%') STORED",
        "ALTER TABLE gsv ADD COLUMN g_in boolean GENERATED ALWAYS AS (c1 IN (1, 2)) STORED",
        "ALTER TABLE gsv ADD COLUMN g_cast bigint GENERATED ALWAYS AS ((c1 + c2)::bigint) STORED",
        "ALTER TABLE gsv ADD COLUMN g_call integer GENERATED ALWAYS AS (length(t || 'x')) STORED",
        "INSERT INTO gsv (id, c1, c2, t) VALUES (1, 2, 5, 'ab')",
    ]);
    assert_eq!(
        node.rows(
            "SELECT g_depth, g_cat, g_cmp, g_like, g_in, g_cast, g_call FROM gsv WHERE id = 1"
        ),
        vec![vec!["7", "abx", "t", "t", "t", "7", "3"]],
        "a shape whose printed form does not read back computes the wrong value, or none"
    );
    // And it recomputes, which is the half an `INSERT` alone cannot show: the expression is read
    // out of the catalog again for the `UPDATE`, so a text that only parses once would pass above
    // and fail here.
    node.run("UPDATE gsv SET c1 = 9, t = 'zz' WHERE id = 1")
        .unwrap();
    assert_eq!(
        node.rows(
            "SELECT g_depth, g_cat, g_cmp, g_like, g_in, g_cast, g_call FROM gsv WHERE id = 1"
        ),
        vec![vec!["21", "zzx", "t", "f", "f", "14", "3"]],
    );
}

/// It is a function of the **row**, re-evaluated on every write — not a value computed once.
#[test]
fn it_recomputes_when_its_source_changes() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE gen (id int8 PRIMARY KEY, name text, upper_name text GENERATED ALWAYS AS \
         (upper(name)) STORED)",
        "INSERT INTO gen (id, name) VALUES (1, 'ada')",
    ]);
    node.run("UPDATE gen SET name = 'zoe' WHERE id = 1")
        .unwrap();
    assert_eq!(
        node.rows("SELECT name, upper_name FROM gen"),
        [["zoe", "ZOE"]]
    );
    // A NULL source gives a NULL value, and the column is nullable — nothing forces the
    // expression to produce one.
    node.run("INSERT INTO gen (id, name) VALUES (2, NULL)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT upper_name IS NULL FROM gen WHERE id = 2"),
        [["t"]]
    );
}

/// The expression lives where a default does, and the column has **no** default.
#[test]
fn the_expression_is_in_pg_attrdef_and_not_in_column_default() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE gen (id int8 PRIMARY KEY, name text, upper_name text GENERATED ALWAYS AS \
         (upper(name)) STORED)",
    ]);
    assert_eq!(
        node.rows(
            "SELECT attname, attgenerated FROM pg_attribute WHERE attrelid = 'gen'::regclass AND \
             attnum > 0 ORDER BY attnum"
        ),
        vec![vec!["id", ""], vec!["name", ""], vec!["upper_name", "s"],]
    );
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d WHERE d.adrelid = \
             'gen'::regclass"
        ),
        [["upper(name)"]]
    );
    assert_eq!(
        node.rows(
            "SELECT column_default, is_generated, generation_expression FROM \
             information_schema.columns WHERE table_name = 'gen' AND column_name = 'upper_name'"
        ),
        [["\\N", "ALWAYS", "upper(name)"]],
        "no default, and the expression is in the other column"
    );
}
