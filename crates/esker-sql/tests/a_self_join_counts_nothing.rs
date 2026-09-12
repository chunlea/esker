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

/// **The plans, printed side by side**, because the difference between the two self-joins was the
/// whole question: `#87`'s shape was answered without scanning the aliased side at all.
///
/// Before the fix the first of these read
/// `Nested Loop / Inner: Point Get on topics / Point Get on topics` and answered nothing; it now
/// reads `… / Seq Scan on topics` like the three that were always right. Printed rather than
/// asserted, because the plan's text is not a contract — the answers above are.
#[test]
fn the_plan_of_each_shape() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);

    for (name, sql) in [
        (
            "#87   alias on the inner side, filter on the driving side",
            "SELECT replies_topics.id FROM topics \
             INNER JOIN topics replies_topics ON replies_topics.parent_id = topics.id \
             WHERE topics.id = 1",
        ),
        (
            "      alias on the driving side",
            "SELECT replies_topics.id FROM topics replies_topics \
             INNER JOIN topics parents ON replies_topics.parent_id = parents.id \
             WHERE parents.id = 1",
        ),
        (
            "      two different tables",
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

/// **The other shape run 128 lost, and it is the same question asked of one relation**: a point
/// read on the primary key with a **second** equality beside it.
///
/// `has_many_associations_test.rb` answered *"Couldn't find `Namespaced::Firm` with 'id'=49"* for a
/// row the stores held, and the statement family is
/// `SELECT … FROM "companies" WHERE "companies"."type" = $1 AND "id" = 49`. r1 records that the tap
/// truncates and the exact failing statement is not in the log, so this is **not** confirmed to be
/// the same defect — it is the nearest shape, written so the answer is on the record either way.
///
/// Three forms, because the fix is about telling relation instances apart: no alias at all, the
/// same table under two aliases, and two different tables.
#[test]
fn a_point_read_with_a_second_equality_finds_the_row() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);
    session
        .run(
            "CREATE TABLE companies (id bigserial primary key, firm_id bigint, \
             type text, name text)",
        )
        .unwrap();
    session
        .run(
            "INSERT INTO companies (id, firm_id, type, name) VALUES \
             (49, NULL, 'Namespaced::Firm', 'the firm'), \
             (50, 49, 'Namespaced::Client', 'a client')",
        )
        .unwrap();

    assert_eq!(
        rows(
            &mut session,
            "SELECT name FROM companies WHERE companies.type = 'Namespaced::Firm' \
             AND companies.id = 49"
        ),
        [["the firm"]],
        "no alias: a point read with a second equality beside it"
    );

    assert_eq!(
        rows(
            &mut session,
            "SELECT firms.name FROM companies firms \
             INNER JOIN companies clients ON clients.firm_id = firms.id \
             WHERE firms.type = 'Namespaced::Firm' AND firms.id = 49"
        ),
        [["the firm"]],
        "the same table under two aliases, which is #87's shape with a second equality"
    );

    assert_eq!(
        rows(
            &mut session,
            "SELECT companies.name FROM companies \
             INNER JOIN bulbs ON bulbs.car_id = companies.id \
             WHERE companies.type = 'Namespaced::Firm' AND companies.id = 1"
        ),
        Vec::<Vec<String>>::new(),
        "two different tables: no company with id 1, so no rows and no wrong row either"
    );
}

/// **The aliased side's own equality still pins its key**, which is the half a fix that simply
/// stopped trusting qualifiers would have broken.
#[test]
fn an_equality_written_on_the_alias_still_pins_that_alias() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);

    // `replies_topics.id = 2` names the *aliased* side, and it is the reply.
    assert_eq!(
        rows(
            &mut session,
            "SELECT replies_topics.title FROM topics \
             INNER JOIN topics replies_topics ON replies_topics.parent_id = topics.id \
             WHERE replies_topics.id = 2"
        ),
        [["reply"]],
        "an equality on the alias must still reach the alias"
    );
    // And a bare qualifier-less one still applies, as it always did.
    assert_eq!(
        rows(
            &mut session,
            "SELECT COUNT(*) FROM topics \
             INNER JOIN topics replies_topics ON replies_topics.parent_id = topics.id"
        ),
        [["1"]],
        "no qualifier anywhere: the join is unchanged"
    );
}

/// **Which engine answers these two**, because `#86` is a columnar-copy defect in the same week and
/// a fragment answering one of them would put it in that row rather than this one.
///
/// h1's reading of `#86` is that a columnar copy cannot see an unresolved secondary lock, so a
/// fragment comes back **exactly one row short with no error** — and `esker_columnar`'s
/// `Aggregate::CountStar` means a `COUNT(*)` *may* be answered by a fragment. So the two statements
/// run 128 lost are asked here with `EXPLAIN ANALYZE` and the line that would say so is printed.
///
/// This harness has no columnar learner, so `Fragments:` is expected to be absent and the point of
/// the test is the record rather than the assertion: it says which engine answered **here**, which
/// is what stops a green row path from standing in for an uncovered fragment path.
#[test]
fn which_engine_answers_the_two_statements_run_128_lost() {
    let cluster = Cluster::start();
    let mut session = cluster.session();
    schema(&mut session);
    session
        .run("CREATE TABLE companies (id bigserial primary key, firm_id bigint, type text, name text)")
        .unwrap();
    session
        .run(
            "INSERT INTO companies (id, firm_id, type, name) VALUES \
             (49, NULL, 'Namespaced::Firm', 'the firm')",
        )
        .unwrap();

    for (what, sql) in [
        (
            "counter_cache's COUNT",
            "SELECT COUNT(*) FROM topics \
             INNER JOIN topics replies_topics ON replies_topics.parent_id = topics.id \
             WHERE topics.id = 1",
        ),
        (
            "firm49's point read",
            "SELECT name FROM companies WHERE companies.type = 'Namespaced::Firm' \
             AND companies.id = 49",
        ),
    ] {
        let plan = rows(&mut session, &format!("EXPLAIN ANALYZE {sql}"));
        let text: Vec<String> = plan.iter().map(|row| row.join(" | ")).collect();
        let fragments: Vec<&String> = text.iter().filter(|l| l.contains("Fragment")).collect();
        println!("\n=== {what}\n    answer {:?}", rows(&mut session, sql));
        for line in &text {
            println!("    {line}");
        }
        println!("    fragments mentioned: {fragments:?}");
    }
}
