//! A chain of joins, against PostgreSQL 19beta1's own answers — the rung-3 blocker.
//!
//! `ActiveRecord`'s `indexes()` sends four tables and three joins, `pg_class` twice under two
//! aliases, all `ON`, mixing `INNER` and `LEFT`. Four scoreboard runs stopped at rung 3 on
//! `0A000 more than one JOIN is not supported`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
///
/// A `CROSS JOIN` entry stood here on the assumption that it was refused like a comma-separated
/// `FROM` list. It is not — it has always run — and the harness said so by failing on a divergence
/// that had started agreeing. Checking both directions is what makes a divergence list a
/// measurement rather than a note somebody wrote once.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT a.id, b.label, c.tag FROM ja a RIGHT JOIN jb b ON a.bid = b.id JOIN jc c ON \
             b.cid = c.id",
        "`RIGHT JOIN` is `0A000` naming itself and always has been — `plan::JoinKind` has \
             `Inner` and `Left` and no third. It is in this corpus because a chain is where it \
             would be most tempting to add one by flipping the operands, which works for a join \
             of two tables and does not for a chain: the flip changes which table drives every \
             step after it. It lands as its own unit, with its own capture, or not at all.",
    )],
};

#[test]
fn every_join_chain_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_join_chain.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 22,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The four ways two joins can combine, which is the test a fold either passes or is a guess.
///
/// `LEFT` then `INNER` returns **one** row, not four: the inner join is applied to the rows the
/// left join already NULL-extended and throws them back out. A chain planned as "each join
/// against the original left table" keeps all four; a chain that reordered its steps keeps two.
/// Any two of these four agreeing by accident is possible, all four is not.
#[test]
fn the_four_combinations_of_two_joins_differ_and_all_four_are_right() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE ja (id int8 PRIMARY KEY, name text, bid int8)",
        "CREATE TABLE jb (id int8 PRIMARY KEY, label text, cid int8)",
        "CREATE TABLE jc (id int8 PRIMARY KEY, tag text)",
        "INSERT INTO ja VALUES (1, 'a-one', 10), (2, 'a-two', 20), (3, 'a-three', NULL), \
         (4, 'a-four', 99)",
        "INSERT INTO jb VALUES (10, 'b-ten', 100), (20, 'b-twenty', NULL), (30, 'b-thirty', 300)",
        "INSERT INTO jc VALUES (100, 'c-hundred'), (300, 'c-threehundred')",
    ] {
        node.run(statement).unwrap();
    }

    for (first, second, expected) in [
        ("JOIN", "JOIN", 1),
        ("LEFT JOIN", "LEFT JOIN", 4),
        ("LEFT JOIN", "JOIN", 1),
        ("JOIN", "LEFT JOIN", 2),
    ] {
        let sql = format!(
            "SELECT count(*) FROM ja a {first} jb b ON a.bid = b.id {second} jc c ON b.cid = c.id"
        );
        assert_eq!(
            node.rows(&sql),
            vec![vec![expected.to_string()]],
            "{first} … {second}"
        );
    }
}

/// The shape rung 3 sends: four tables, three joins, and one table twice under two aliases.
///
/// An alias *replaces* a table's name, so `pg_class t` and `pg_class i` are two entries in one
/// scope that share a `TableDef` — a duplicate **name** is the error and a duplicate table is not.
#[test]
fn the_same_table_joins_itself_twice_under_two_aliases() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE t1 (id int8 PRIMARY KEY, ref int8, tag text)",
        "CREATE TABLE t2 (id int8 PRIMARY KEY, other int8)",
        "INSERT INTO t1 VALUES (1, 2, 'one'), (2, 1, 'two')",
        "INSERT INTO t2 VALUES (1, 2)",
    ] {
        node.run(statement).unwrap();
    }

    // `t1` twice, and an `ON` that reaches back past the table joined in between.
    assert_eq!(
        node.rows(
            "SELECT a.tag, b.other, c.tag FROM t1 a JOIN t2 b ON a.id = b.id JOIN t1 c ON \
             c.id = a.ref"
        ),
        vec![vec!["one", "2", "two"]]
    );

    // The same name twice is the error a real server gives, whatever the table.
    let error = node
        .run("SELECT a.id FROM t1 a JOIN t2 a ON a.id = a.id JOIN t1 c ON c.id = a.id")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "42712");
}

/// `USING` in a chain is refused by name rather than approximated.
///
/// `tests/corpus/pg19_join_using.txt` records what it would have to do: merge the column so
/// `SELECT *` returns it once, and answer `42702` for a bare reference once a later `ON` join
/// brings a third column of the same name. Both are in that capture; neither is guessed at here.
#[test]
fn using_in_a_chain_is_refused_by_name() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE u1 (id int8 PRIMARY KEY, a text)",
        "CREATE TABLE u2 (id int8 PRIMARY KEY, b text)",
        "CREATE TABLE u3 (id int8 PRIMARY KEY, c text)",
    ] {
        node.run(statement).unwrap();
    }

    // One join with `USING` still runs: only the chain is refused.
    assert_eq!(
        node.rows("SELECT id FROM u1 JOIN u2 USING (id)"),
        Vec::<Vec<String>>::new()
    );

    let error = node
        .run("SELECT id FROM u1 JOIN u2 USING (id) JOIN u3 USING (id)")
        .unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
    assert!(
        error.to_string().contains("USING"),
        "`{error}` does not name the construct"
    );
}
