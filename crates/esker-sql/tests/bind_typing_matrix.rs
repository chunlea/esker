//! **What actually decides whether a bind gets typed** — r1's matrix, run here.
//!
//! Ten cells, `triage/bind-typing-depth-matrix.txt`. Every one is the same bind, the same `bigint`
//! column and the same `= $1`; only the construct the column is reached through varies. PostgreSQL
//! 19 answers `{bigint}` to all ten, verified against the oracle with these exact statements
//! before they were written down here.
//!
//! **The matrix exists because a pattern was wrong.** Four captures had the failing bind inside
//! something nested, and nesting depth was offered as the map. It is not: depth 2 with the bind
//! innermost passes and depth 1 with the bind outside fails, so adding a level changes nothing.
//! Nor is it joins — a plain `INNER JOIN` with the bind in the `WHERE` passes. What the failing
//! cells share is that the bind is compared against a column **projected out of** a derived table
//! or a CTE, whose type has to be carried through the projection, rather than one still attached
//! to a base relation.
//!
//! Keeping the whole matrix — the six that always passed included — is the point of it. A fix that
//! types the projected columns by breaking the base-relation cells would be a fix that moved the
//! failure, and only the cells nobody was worried about would say so.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE bm_t (id bigint, n integer, tag varchar)",
    "CREATE TABLE bm_j (id bigint, t_id bigint)",
    "INSERT INTO bm_t VALUES (1, 10, 'x')",
    "INSERT INTO bm_j VALUES (1, 1)",
    "CREATE VIEW bm_v AS SELECT id, n, tag FROM bm_t WHERE tag = 'x'",
];

/// Every cell: a name, the statement, and what PostgreSQL 19 resolves `$1` to — which is `bigint`
/// in all ten, measured.
const MATRIX: &[(&str, &str)] = &[
    ("plain WHERE", "SELECT n FROM bm_t WHERE bm_t.id = $1"),
    (
        "derived table, bind outside",
        "SELECT d.n FROM (SELECT * FROM bm_t) d WHERE d.id = $1",
    ),
    (
        "derived table, bind inside",
        "SELECT d.n FROM (SELECT * FROM bm_t WHERE bm_t.id = $1) d",
    ),
    (
        "derived in derived, bind outside",
        "SELECT d.n FROM (SELECT * FROM (SELECT * FROM bm_t) e) d WHERE d.id = $1",
    ),
    (
        "derived in derived, bind innermost",
        "SELECT d.n FROM (SELECT * FROM (SELECT * FROM bm_t WHERE bm_t.id = $1) e) d",
    ),
    (
        "CTE, bind outside",
        "WITH cte AS (SELECT * FROM bm_t) SELECT cte.n FROM cte WHERE cte.id = $1",
    ),
    (
        "CTE, bind inside the body",
        "WITH cte AS (SELECT * FROM bm_t WHERE bm_t.id = $1) SELECT cte.n FROM cte",
    ),
    (
        "view, bind in the outer WHERE",
        "SELECT bm_v.n FROM bm_v WHERE bm_v.id = $1",
    ),
    (
        "join, bind in the WHERE",
        "SELECT bm_t.n FROM bm_t INNER JOIN bm_j ON bm_j.t_id = bm_t.id WHERE bm_t.id = $1",
    ),
    (
        "joined UPDATE ... FROM with an alias",
        "UPDATE bm_t \"a\" SET n = 99 FROM bm_t INNER JOIN bm_j ON bm_j.t_id = bm_t.id \
         WHERE bm_t.id = $1 AND bm_t.id = \"a\".id",
    ),
];

#[test]
fn every_cell_of_the_matrix_types_its_bind_as_the_column_it_meets() {
    let mut node = parity::Node::new(FIXTURE);
    let mut wrong = Vec::new();
    for (at, (what, sql)) in MATRIX.iter().enumerate() {
        let name = format!("m{at}");
        match node.run(&format!("PREPARE {name} AS {sql}")) {
            Ok(_) => {
                let types = node
                    .rows(&format!(
                        "SELECT parameter_types FROM pg_prepared_statements WHERE name = '{name}'"
                    ))
                    .first()
                    .and_then(|row| row.first())
                    .cloned()
                    .unwrap_or_default();
                if types != "{bigint}" {
                    wrong.push(format!("  {what}: {types}"));
                }
            }
            // A refusal here is the failure the suite sees: the bind resolved to `text` and the
            // comparison had no operator.
            Err(error) => wrong.push(format!("  {what}: !{error}")),
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} cells resolve `$1` to something other than PostgreSQL's `bigint`:\n{}",
        wrong.len(),
        MATRIX.len(),
        wrong.join("\n")
    );
}
