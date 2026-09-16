//! **A bulk `INSERT` whose prewrite fits a frame is not refused by an estimate of it** — debt #98,
//! against three real stores, because `MemoryBackend` sends no frames and cannot see this at all.
//!
//! Run 127 attempt 7's one new red was `associations/eager_test.rb`, whose `citations` fixture is
//! ERB — `<% 65536.times %>` — and whose table is three `t.references`, so every row writes a row
//! key and three index entries: **four mutations a row, every one of them stamped** with the
//! statement's read timestamp (ADR 0057 §4). The fixture load was refused inside `esker-sql`,
//! before a frame went out:
//!
//! ```text
//! XX000  internal error: request of about 17334584 bytes exceeds the 16777216-byte frame limit
//! ```
//!
//! The request had not grown. `txn_payload_size` charges `PER_FIELD` — six bytes — for the read
//! timestamp a stamped mutation carries, which the encoder writes as a varint beside fields the
//! estimate already counted loosely, so the estimate stands six bytes per stamped mutation above
//! the frame: 262,144 mutations, 1,572,864 bytes, and a prewrite of about 15.8 MB refused as 17.3.
//!
//! This test is the same shape with the rows turned down until it straddles the real limit: the
//! estimate is over it and the frame is comfortably under, so the insert goes through — or is
//! refused with that message, which is the bug.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

/// Rows, chosen from the two numbers this shape measures at: **473 bytes a row estimated** and
/// **24 bytes a row of that estimate that no byte of the request answers to** — six per stamped
/// mutation, four mutations a row. At 36,000 rows the estimate is about 17.0 MB, over the
/// 16,777,216-byte limit, and the frame is about 16.2 MB, under it. So the insert is refused today
/// and sent once the size is the encoding's own.
const ROWS: usize = 36_000;

/// Padding per row, so the limit is reached at a row count a test can afford. A real `citations`
/// row is narrower and needs 65,536 of them.
const PAD_BYTES: usize = 200;

#[test]
fn a_bulk_insert_whose_frame_fits_is_not_refused_by_an_estimate_of_it() {
    let cluster = cluster::Cluster::start();
    let mut node = cluster.session();

    node.run(
        "CREATE TABLE citations (\
           id bigint primary key, \
           book1_id bigint, \
           book2_id bigint, \
           citation_id bigint, \
           note text)",
    )
    .expect("the table is created");
    for column in ["book1_id", "book2_id", "citation_id"] {
        node.run(&format!(
            "CREATE INDEX index_citations_on_{column} ON citations ({column})"
        ))
        .expect("the index is created");
    }

    let padding = "x".repeat(PAD_BYTES);
    node.run(&format!(
        "INSERT INTO citations SELECT i, i, i, i, '{padding}' FROM generate_series(1, {ROWS}) AS i"
    ))
    .expect("a prewrite whose frame fits the limit is sent");

    assert_eq!(
        node.rows("SELECT count(*) FROM citations"),
        vec![vec![Some(ROWS.to_string())]],
        "every row committed"
    );
}
