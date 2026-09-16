//! **What cutting a batch into frames costs in round trips** — #99's number, in the idiom of
//! `lock_cost.rs`: a real cluster, a printed table, and no assertion on a clock.
//!
//! The statement is the one #98 and #99 were written for: one `INSERT` whose prewrite does not fit
//! a 16 MiB frame. Before #99 it was refused outright (`XX000 … exceeds the 16777216-byte frame
//! limit`); now it goes out as several frames, and what that costs is what this prints — beside the
//! same shape narrow enough for one frame, because a number with nothing to compare it to is not a
//! measurement.
//!
//! **The counters are per thread and this shape stays on one.** `esker_client::stmt_stats` counts
//! what the calling thread did, the rows of one table live in one region, and a single region's
//! group is answered on the calling thread (`router::fan_out`), so every chunk of the cut is sent in
//! turn and counted here. A shape that spread over three regions would fan out and lose the other
//! two threads' counts — which is why this measures the cut and not the spread.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::cast_precision_loss
)]

mod cluster;

/// Rows for the wide statement: at four stamped mutations a row and about 473 bytes of request a
/// row, 45,000 rows is a little over 20 MB — more than one frame and less than two full ones.
const WIDE_ROWS: usize = 45_000;

/// Rows for the narrow one: the same shape, comfortably inside a single frame.
const NARROW_ROWS: usize = 4_000;

/// Padding per row, so the limit is reached at a row count a measurement can afford.
const PAD_BYTES: usize = 200;

fn insert(node: &mut cluster::Session, padding: &str, from: usize, rows: usize) {
    node.run(&format!(
        "INSERT INTO citations SELECT i, i, i, i, '{padding}' \
         FROM generate_series({from}, {}) AS i",
        from + rows - 1
    ))
    .expect("a batch larger than a frame is cut, not refused");
}

#[test]
#[ignore = "a measurement, not an assertion; starts a real cluster and prints a table"]
fn what_cutting_a_batch_into_frames_costs() {
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
    .unwrap();
    for column in ["book1_id", "book2_id", "citation_id"] {
        node.run(&format!(
            "CREATE INDEX index_citations_on_{column} ON citations ({column})"
        ))
        .unwrap();
    }

    esker_client::stmt_stats::force_on();
    let padding = "x".repeat(PAD_BYTES);

    esker_client::stmt_stats::reset();
    insert(&mut node, &padding, 1, WIDE_ROWS);
    let wide = esker_client::stmt_stats::taken();

    esker_client::stmt_stats::reset();
    insert(&mut node, &padding, WIDE_ROWS + 1, NARROW_ROWS);
    let narrow = esker_client::stmt_stats::taken();

    println!(
        "rows     round trips   prewrites   commits   mutations   per 1,000 rows\n\
         {:<9}{:<14}{:<12}{:<10}{:<12}{:.2}\n\
         {:<9}{:<14}{:<12}{:<10}{:<12}{:.2}",
        WIDE_ROWS,
        wide.round_trips,
        wide.prewrites,
        wide.commits,
        wide.keys,
        wide.round_trips as f64 * 1000.0 / WIDE_ROWS as f64,
        NARROW_ROWS,
        narrow.round_trips,
        narrow.prewrites,
        narrow.commits,
        narrow.keys,
        narrow.round_trips as f64 * 1000.0 / NARROW_ROWS as f64,
    );

    // A measurement that read zero measured nothing: the counters are off, or the layer reset them
    // after the statement rather than before it.
    assert!(
        wide.round_trips > 0 && narrow.round_trips > 0,
        "the statement counters read zero, so this measured nothing"
    );
    assert!(
        wide.prewrites > narrow.prewrites,
        "the wide statement did not need more frames than the narrow one: {} against {}",
        wide.prewrites,
        narrow.prewrites
    );
}
