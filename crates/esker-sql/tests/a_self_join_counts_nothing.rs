//! **#87 — a self-join aggregate counts nothing, and the plan never scans the join side.**
//!
//! Run 127 attempt 5 lost four tests in `counter_cache_test.rb`, all of them `reset_counters`, all
//! with the same assertion: a counter recomputed from a `COUNT` landed on **0 where 1 was owed**.
//! r1's binary comparison then took the regression story away — `29e66e60` (with #78 and #79) and
//! `091f07bb` (without either) fail **identically**, 4 of 56 both ways, on a fresh cluster in 95 s.
//! It is not new code; it is newly *reached* code, and `counter_cache_test.rb` is file 234 where no
//! earlier attempt got past 44.
//!
//! What names the mechanism is the tap's own read counters, in the same file, on the same cluster:
//!
//! ```text
//! pr=0 rs=2   SELECT COUNT(*) FROM "bulbs" WHERE "bulbs"."car_id" = $1          passes
//! pr=1 rs=0   SELECT COUNT(*) AS "count_all", "topics"."id" …
//!               FROM "topics" INNER JOIN "topics" "replies_topics"
//!               ON "replies_topics"."parent_id" = "topics"."id"                 fails
//! ```
//!
//! **Counting children requires scanning by `parent_id`; one point read can find one row.** A plain
//! child count scans twice and is right. The **self-join** — one table joined to itself under an
//! alias — does one point read, no scan at all, and answers a number the `UPDATE` on the next line
//! writes into the counter column.
//!
//! So the row was never lost. The plan never looked for it. These tests are the shape, against
//! three real stores.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use cluster::Cluster;

/// The rows as text, with `NULL` spelled out — the harness answers `Option<String>` a column.
fn rows(session: &mut cluster::Session, sql: &str) -> Vec<Vec<String>> {
    session
        .rows(sql)
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.unwrap_or_else(|| "NULL".to_string()))
                .collect()
        })
        .collect()
}

/// Two tables, the way the fixture does: a self-referential `topics` and a plain child `bulbs`.
fn schema(session: &mut cluster::Session) {
    session
        .run(
            "CREATE TABLE topics (id bigserial primary key, parent_id bigint, \
             title text, replies_count int DEFAULT 0)",
        )
        .unwrap();
    session
        .run("CREATE TABLE cars (id bigserial primary key, name text)")
        .unwrap();
    session
        .run("CREATE TABLE bulbs (id bigserial primary key, car_id bigint, name text)")
        .unwrap();
    // One parent and one reply, which is the whole of what the failing assertion counts.
    session
        .run("INSERT INTO topics (id, parent_id, title) VALUES (1, NULL, 'parent')")
        .unwrap();
    session
        .run("INSERT INTO topics (id, parent_id, title) VALUES (2, 1, 'reply')")
        .unwrap();
    session
        .run("INSERT INTO cars (id, name) VALUES (1, 'c')")
        .unwrap();
    session
        .run("INSERT INTO bulbs (id, car_id, name) VALUES (1, 1, 'b')")
        .unwrap();
}

/// **The control, and it is in the same file for the same reason it was in the same Rails file**:
/// a plain child count over a foreign key is right, so whatever is wrong below is not "counting".
#[test]
fn a_plain_child_count_is_right() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);

    assert_eq!(
        rows(
            &mut session,
            "SELECT COUNT(*) FROM bulbs WHERE bulbs.car_id = 1"
        ),
        [["1"]],
        "a plain child count over a foreign key"
    );
}

/// **The failing shape: one table joined to itself under an alias.**
///
/// `reset_counters` recomputes the counter with exactly this, and the `UPDATE` on the next line
/// writes whatever it answers into `replies_count`.
#[test]
#[ignore = "#87: red — the reproduction, un-ignored by the fix that makes predicate placement \
            alias-aware. Kept in the tree because a bug fix lands with its regression test and \
            this one is already written."]
fn a_self_join_counts_the_rows_that_are_there() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);

    // The join alone, before any aggregate: one reply joins to one parent.
    assert_eq!(
        rows(
            &mut session,
            "SELECT replies_topics.id FROM topics \
             INNER JOIN topics replies_topics ON replies_topics.parent_id = topics.id \
             WHERE topics.id = 1"
        ),
        [["2"]],
        "the self-join itself does not find the reply, so the count below cannot"
    );

    assert_eq!(
        rows(
            &mut session,
            "SELECT COUNT(*) FROM topics \
             INNER JOIN topics replies_topics ON replies_topics.parent_id = topics.id \
             WHERE topics.id = 1"
        ),
        [["1"]],
        "the self-join aggregate answered a different number from the join under it"
    );

    // And the statement as `reset_counters` writes it, with the alias column and the grouping.
    assert_eq!(
        rows(
            &mut session,
            "SELECT COUNT(*) AS count_all, topics.id AS topics_id FROM topics \
             INNER JOIN topics replies_topics ON replies_topics.parent_id = topics.id \
             WHERE topics.id = 1 GROUP BY topics.id"
        ),
        [["1", "1"]],
        "`reset_counters`' own statement, which is what wrote 0 into replies_count"
    );
}

/// **The same join with the two sides the other way round**, so a red above can be read as
/// "the aliased side is the one that is not scanned" rather than "self-joins do not work".
#[test]
fn a_self_join_driven_from_the_child_side_counts_the_same() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);

    assert_eq!(
        rows(
            &mut session,
            "SELECT COUNT(*) FROM topics replies_topics \
             INNER JOIN topics parents ON replies_topics.parent_id = parents.id \
             WHERE parents.id = 1"
        ),
        [["1"]],
        "the same join written from the child side"
    );
}

/// **A join of two different tables**, so a red above cannot be read as "joins do not count".
#[test]
fn a_join_of_two_tables_counts_the_same_way() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);

    assert_eq!(
        rows(
            &mut session,
            "SELECT COUNT(*) FROM cars INNER JOIN bulbs ON bulbs.car_id = cars.id \
             WHERE cars.id = 1"
        ),
        [["1"]],
        "a join of two different tables"
    );
}

/// **The plans, printed side by side**, because the difference between the two self-joins is the
/// whole question: one of them is answered without scanning the aliased side at all.
#[test]
fn the_plan_of_each_shape() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);

    for (name, sql) in [
        (
            "FAILS  alias on the inner side, filter on the driving side",
            "SELECT replies_topics.id FROM topics \
             INNER JOIN topics replies_topics ON replies_topics.parent_id = topics.id \
             WHERE topics.id = 1",
        ),
        (
            "passes alias on the driving side",
            "SELECT replies_topics.id FROM topics replies_topics \
             INNER JOIN topics parents ON replies_topics.parent_id = parents.id \
             WHERE parents.id = 1",
        ),
        (
            "passes two different tables",
            "SELECT bulbs.id FROM cars INNER JOIN bulbs ON bulbs.car_id = cars.id \
             WHERE cars.id = 1",
        ),
        (
            "the same join with no filter at all",
            "SELECT replies_topics.id FROM topics \
             INNER JOIN topics replies_topics ON replies_topics.parent_id = topics.id",
        ),
    ] {
        let plan = rows(&mut session, &format!("EXPLAIN {sql}"));
        let answer = rows(&mut session, sql);
        println!("\n=== {name}\n    answer {answer:?}");
        for line in plan {
            println!("    {}", line.join(" | "));
        }
    }
}
