//! **P0.** A catalog name record that points at a table record which is not there.
//!
//! Run 128 (attempt 4, `091f07bb`, 64 MiB, `--retention-ms 60000`, the #70 sweeper on) stopped
//! after twenty-eight files with five consecutive files failing the same way:
//!
//! ```text
//! PG::DataCorrupted: ERROR: corrupt data: a name points at table 34928, which is not there
//! SELECT c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace
//!   WHERE n.nspname = ANY (current_schemas(false)) AND c.relname = 'books' AND c.relkind IN ('r','p')
//! ```
//!
//! Two different table ids across two stops, so it is not one table. **The node was restarted and
//! the error continued**, which is the fact that shapes this test: a cache cleared by a restart
//! cannot explain it, so the two keys really do disagree **on disk** — the name record is there and
//! the table record it names is not.
//!
//! The two are written by one transaction and deleted by one transaction, so a durable disagreement
//! between them is either a write that landed by halves or a collection that took one and left the
//! other. The suite that produced it creates and drops the same table name repeatedly, with a
//! sixty-second retention window and a collection on every safepoint rise — so every version of
//! both keys is collectable within a minute of being written.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use cluster::{Cluster, Settings};

/// Rounds of the shape the suite runs: the same table name, created and dropped.
const ROUNDS: usize = 200;

/// What `ActiveRecord` sends before it can describe a table — the statement that failed.
const LOOKUP: &str = "SELECT c.relname FROM pg_class c \
     LEFT JOIN pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = ANY (current_schemas(false)) AND c.relname = 'books' \
     AND c.relkind IN ('r','p')";

/// A safepoint above everything this round has written and below nothing it is about to.
///
/// One physical millisecond per round on the oracle's scale, which is far above the counting
/// timestamps the workload is using — so everything already committed is collectable and the
/// collector is doing the deciding.
fn round_safepoint(round: usize) -> u64 {
    (round as u64) << esker_store::TSO_LOGICAL_BITS
}

#[test]
fn a_name_record_never_outlives_the_table_it_names() {
    let cluster = Cluster::start_with(Settings {
        collecting: true,
        ..Settings::default()
    });
    let mut session = cluster.session();

    for round in 1..=ROUNDS {
        session
            .run("CREATE TABLE books (id bigserial primary key, title text)")
            .unwrap_or_else(|error| panic!("round {round}: CREATE: {error}"));
        // The lookup while it exists, and again after it is gone: the failing statement ran in
        // both states and the catalog has to answer in both.
        session.run(LOOKUP).unwrap_or_else(|error| {
            panic!(
                "round {round}: the lookup found a broken catalog \
                 while `books` existed: {error}"
            )
        });
        session
            .run("DROP TABLE books")
            .unwrap_or_else(|error| panic!("round {round}: DROP: {error}"));

        // **A safepoint that actually bites.** `publish_safepoint` publishes the oracle's `now`,
        // and this harness's oracle counts from a thousand — a retention window in milliseconds
        // underflows against it and the collector keeps everything, so a probe using it would
        // watch the engine's own rules and call them the collector's. This reaches the past by
        // token: every version written above is below it, and only the newest of each key may
        // survive.
        cluster.publish_safepoint_at(round_safepoint(round));

        session
            .run(LOOKUP)
            .unwrap_or_else(|error| panic!("round {round}: {error}"));
    }

    let (entries, ssts) = cluster.standing();
    println!(
        "  {ROUNDS} rounds · {entries} entries in {ssts} tables · safepoint {}",
        cluster.safepoint()
    );
}
