//! `DECLARE` / `FETCH` / `MOVE` / `CLOSE` — **a cursor is a position in a result, and the
//! position is the whole of the semantics.**
//!
//! `postgresql_adapter_prevent_writes_test.rb:83` opens a transaction and sends all four; this
//! node answered `0A000 DECLARE is not supported`.
//!
//! Measured on PostgreSQL 19 through the `pg` gem — `psql` prints a result set for `FETCH` and so
//! hides its command tag, and the tag is half of what a cursor answers. Four rows, `1 2 3 4`:
//!
//! ```text
//! DECLARE c CURSOR FOR SELECT id FROM cu ORDER BY id -> DECLARE CURSOR
//! FETCH c                                            -> FETCH 1    1
//! FETCH c                                            -> FETCH 1    2
//! MOVE c                                             -> MOVE 1
//! FETCH c                                            -> FETCH 1    4
//! FETCH BACKWARD 2 FROM c                            -> FETCH 2    3,2     (reverse order)
//! FETCH PRIOR FROM c                                 -> FETCH 1    1
//! FETCH FIRST FROM c                                 -> FETCH 1    1
//! FETCH LAST FROM c                                  -> FETCH 1    4
//! FETCH ABSOLUTE 2 FROM c                            -> FETCH 1    2
//! FETCH RELATIVE -1 FROM c                           -> FETCH 1    1
//! FETCH ALL FROM c                                   -> FETCH 3    2,3,4
//! FETCH ALL FROM c                                   -> FETCH 0
//! MOVE BACKWARD ALL IN c                             -> MOVE 4
//! MOVE 2 IN c                                        -> MOVE 2
//! FETCH c                                            -> FETCH 1    3
//! FETCH FORWARD 0 FROM c                             -> FETCH 1    3     <- zero re-reads the row
//! MOVE 0 IN c                                        -> MOVE 1           <- and counts it
//! CLOSE c                                            -> CLOSE CURSOR
//! CLOSE ALL                                          -> CLOSE CURSOR ALL
//! ```
//!
//! **`FORWARD 0` is the line worth keeping.** Zero does not mean "do nothing": it re-reads the row
//! the cursor is on and reports a count of one. A model that treated the count as a number of rows
//! to step over gets every other line right and that one wrong.
//!
//! And the errors, measured the same way:
//!
//! ```text
//! DECLARE outside a transaction block   25P01: DECLARE CURSOR can only be used in transaction blocks
//! DECLARE c ... twice                   42P03: cursor "c" already exists
//! FETCH after COMMIT                    34000: cursor "c2" does not exist
//! CLOSE nosuch                          34000: cursor "nosuch" does not exist
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::session::Outcome;

fn node() -> parity::Node {
    parity::Node::new(&[
        "CREATE TABLE cu (id int PRIMARY KEY)",
        "INSERT INTO cu VALUES (1),(2),(3),(4)",
        "CREATE TABLE ex (id int)",
    ])
}

/// `<tag>` for a command, `<tag>|<rows>` for one that returns them, `!<message>` for a refusal.
fn ask(node: &mut parity::Node, sql: &str) -> String {
    match node.run(sql) {
        Ok(Outcome::Done { tag }) => tag,
        Ok(Outcome::Rows { rows, tag, .. }) => {
            let values = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|cell| {
                            cell.as_ref().map_or_else(
                                || "\\N".to_owned(),
                                |bytes| String::from_utf8_lossy(bytes).into_owned(),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\t")
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{tag}|{values}")
        }
        Err(error) => format!("!{error}"),
    }
}

#[test]
fn the_four_statements_the_suite_sends() {
    let mut node = node();
    assert_eq!(ask(&mut node, "BEGIN"), "BEGIN");
    // `ex` is empty, which is what makes every one of these an empty answer in the suite.
    assert_eq!(
        ask(&mut node, "DECLARE cur_ex CURSOR FOR SELECT * FROM ex"),
        "DECLARE CURSOR"
    );
    assert_eq!(ask(&mut node, "FETCH cur_ex"), "FETCH 0|");
    assert_eq!(ask(&mut node, "MOVE cur_ex"), "MOVE 0");
    assert_eq!(ask(&mut node, "CLOSE cur_ex"), "CLOSE CURSOR");
    assert_eq!(ask(&mut node, "COMMIT"), "COMMIT");
}

#[test]
fn the_whole_measured_ladder() {
    let mut node = node();
    ask(&mut node, "BEGIN");
    let ladder = [
        (
            "DECLARE c CURSOR FOR SELECT id FROM cu ORDER BY id",
            "DECLARE CURSOR",
        ),
        ("FETCH c", "FETCH 1|1"),
        ("FETCH c", "FETCH 1|2"),
        ("MOVE c", "MOVE 1"),
        ("FETCH c", "FETCH 1|4"),
        ("FETCH BACKWARD 2 FROM c", "FETCH 2|3,2"),
        ("FETCH PRIOR FROM c", "FETCH 1|1"),
        ("FETCH FIRST FROM c", "FETCH 1|1"),
        ("FETCH LAST FROM c", "FETCH 1|4"),
        ("FETCH ABSOLUTE 2 FROM c", "FETCH 1|2"),
        ("FETCH RELATIVE -1 FROM c", "FETCH 1|1"),
        ("FETCH ALL FROM c", "FETCH 3|2,3,4"),
        ("FETCH ALL FROM c", "FETCH 0|"),
        ("MOVE BACKWARD ALL IN c", "MOVE 4"),
        ("MOVE 2 IN c", "MOVE 2"),
        ("FETCH c", "FETCH 1|3"),
        ("FETCH FORWARD 0 FROM c", "FETCH 1|3"),
        ("MOVE 0 IN c", "MOVE 1"),
        ("CLOSE c", "CLOSE CURSOR"),
        ("CLOSE ALL", "CLOSE CURSOR ALL"),
    ];
    for (sql, expected) in ladder {
        assert_eq!(ask(&mut node, sql), expected, "at {sql}");
    }
}

#[test]
fn a_cursor_needs_a_transaction_block_to_live_in() {
    let mut node = node();
    assert_eq!(
        ask(&mut node, "DECLARE c CURSOR FOR SELECT id FROM cu"),
        "!DECLARE CURSOR can only be used in transaction blocks"
    );
}

#[test]
fn two_cursors_of_one_name_is_an_error() {
    let mut node = node();
    ask(&mut node, "BEGIN");
    assert_eq!(
        ask(&mut node, "DECLARE c CURSOR FOR SELECT id FROM cu"),
        "DECLARE CURSOR"
    );
    assert_eq!(
        ask(&mut node, "DECLARE c CURSOR FOR SELECT 1"),
        "!cursor \"c\" already exists"
    );
}

#[test]
fn a_cursor_does_not_outlive_the_transaction_that_declared_it() {
    let mut node = node();
    ask(&mut node, "BEGIN");
    ask(&mut node, "DECLARE c2 CURSOR FOR SELECT id FROM cu");
    assert_eq!(ask(&mut node, "COMMIT"), "COMMIT");
    assert_eq!(ask(&mut node, "FETCH c2"), "!cursor \"c2\" does not exist");
}

#[test]
fn a_rollback_forgets_a_cursor_too() {
    let mut node = node();
    ask(&mut node, "BEGIN");
    ask(&mut node, "DECLARE c3 CURSOR FOR SELECT id FROM cu");
    assert_eq!(ask(&mut node, "ROLLBACK"), "ROLLBACK");
    ask(&mut node, "BEGIN");
    assert_eq!(ask(&mut node, "FETCH c3"), "!cursor \"c3\" does not exist");
}

#[test]
fn a_holdable_cursor_is_refused_by_name() {
    let mut node = node();
    ask(&mut node, "BEGIN");
    // **The one form that would outlive its transaction**, and the one thing keeping cursors on
    // the executor cannot express: it would mean keeping the snapshot alive past the commit.
    // Refused by name rather than answered with a cursor that quietly saw newer rows.
    assert_eq!(
        ask(
            &mut node,
            "DECLARE h CURSOR WITH HOLD FOR SELECT id FROM cu"
        ),
        "!DECLARE ... WITH HOLD is not supported"
    );
    // The refusal ended the block, as any error does; the next one needs a block of its own.
    ask(&mut node, "ROLLBACK");
    ask(&mut node, "BEGIN");
    // `WITHOUT HOLD` is the default spelled out, and it is taken.
    assert_eq!(
        ask(
            &mut node,
            "DECLARE h CURSOR WITHOUT HOLD FOR SELECT id FROM cu"
        ),
        "DECLARE CURSOR"
    );
}

#[test]
fn the_options_before_cursor_are_taken_and_ignored() {
    let mut node = node();
    ask(&mut node, "BEGIN");
    // Every result here is read into memory, so every cursor is scrollable and insensitive
    // whatever was asked for. `NO SCROLL` is the over-acceptance that leaves: a backward `FETCH`
    // works here and is an error on a real server, which is the direction contract C1 allows.
    assert_eq!(
        ask(
            &mut node,
            "DECLARE a BINARY INSENSITIVE NO SCROLL CURSOR FOR SELECT id FROM cu"
        ),
        "DECLARE CURSOR"
    );
    assert_eq!(ask(&mut node, "FETCH a"), "FETCH 1|1");
    assert_eq!(ask(&mut node, "FETCH BACKWARD 1 FROM a"), "FETCH 0|");
    assert_eq!(
        ask(&mut node, "DECLARE b SCROLL CURSOR FOR SELECT id FROM cu"),
        "DECLARE CURSOR"
    );
}

#[test]
fn a_quoted_name_is_its_own_cursor() {
    let mut node = node();
    ask(&mut node, "BEGIN");
    assert_eq!(
        ask(&mut node, "DECLARE \"Mixed\" CURSOR FOR SELECT id FROM cu"),
        "DECLARE CURSOR"
    );
    assert_eq!(ask(&mut node, "FETCH \"Mixed\""), "FETCH 1|1");
    // Unquoted folds to lower case, so it names a different cursor and finds nothing — asked last
    // because the refusal ends the block.
    assert_eq!(
        ask(&mut node, "FETCH Mixed"),
        "!cursor \"mixed\" does not exist"
    );
}

#[test]
fn a_declare_over_a_table_that_is_not_there_fails_at_the_declare() {
    let mut node = node();
    ask(&mut node, "BEGIN");
    // The rows are read now, so the mistake is reported against the statement the user is looking
    // at — which is where a real server reports it too.
    assert_eq!(
        ask(&mut node, "DECLARE c CURSOR FOR SELECT * FROM nosuchtable"),
        "!relation \"nosuchtable\" does not exist"
    );
}

#[test]
fn closing_a_cursor_that_is_not_there_names_it() {
    let mut node = node();
    ask(&mut node, "BEGIN");
    assert_eq!(
        ask(&mut node, "CLOSE nosuch"),
        "!cursor \"nosuch\" does not exist"
    );
}

/// **A count is an `int4`, as the grammar's `SignedIconst` is.** Past it a real server answers
/// `42601` at the number (measured: `FETCH FORWARD 2147483648` is a syntax error), and the node
/// used to admit the whole `i64` and overflow `self.at + step` on the next move.
#[test]
fn a_count_past_int4_is_a_syntax_error_and_the_limit_itself_is_a_count() {
    let mut node = node();
    // Each in its own block: a syntax error aborts the transaction it is in, there as here.
    for count in ["2147483648", "9223372036854775807"] {
        node.run("BEGIN").unwrap();
        node.run("DECLARE c CURSOR FOR SELECT id FROM cu ORDER BY id")
            .unwrap();
        let error = node
            .run(&format!("FETCH FORWARD {count} FROM c"))
            .unwrap_err();
        assert_eq!(error.sqlstate(), "42601", "{error}");
        assert_eq!(
            error.to_string(),
            format!("syntax error at or near \"{count}\"")
        );
        node.run("ROLLBACK").unwrap();
    }
    node.run("BEGIN").unwrap();
    node.run("DECLARE c CURSOR FOR SELECT id FROM cu ORDER BY id")
        .unwrap();
    node.run("FETCH c").unwrap();
    assert_eq!(
        node.rows("FETCH FORWARD 2147483647 FROM c"),
        vec![vec!["2"], vec!["3"], vec!["4"]]
    );
    node.run("ROLLBACK").unwrap();
}
