//! A catalog name record that points at a table record which is not there — **and what that turned
//! out to be**.
//!
//! Run 127 attempt 4 (`091f07bb`, 64 MiB, `--retention-ms 60000`, the #70 sweeper on) stopped after
//! twenty-eight files with five consecutive files failing the same way:
//!
//! ```text
//! PG::DataCorrupted: ERROR: corrupt data: a name points at table 34928, which is not there
//! SELECT c.relname FROM pg_class c LEFT JOIN pg_namespace n ON n.oid = c.relnamespace
//!   WHERE n.nspname = ANY (current_schemas(false)) AND c.relname = 'books' AND c.relkind IN ('r','p')
//! ```
//!
//! It was the first pass in which collection had ever run, so this file was written to ask the
//! collector about it: the same table name created and dropped, with a safepoint published between
//! the drop and the next lookup, so every version of both keys is collectable before it is read.
//!
//! **The answer is that the collector was not it**, and the preserved data directory says so: the
//! table records of all five are present and readable. The failure is
//! `crates/esker-store/tests/a_scan_answers_with_a_subset.rs` — a scan whose range holds more
//! distinct keys than the store's ceiling answers with a prefix of it and no sign that it did. This
//! loop cannot reach that: it needs eight thousand table records in one tenant, where this has one
//! at a time.
//!
//! **Two things it does still pin**, and both are worth the fourteen seconds:
//!
//! * The lookup answers in all three states — while `books` exists, after it is dropped, and after
//!   a collection has been through — over two hundred rounds.
//! * The collector really is deciding, which is the trap this file fell into first. The control arm
//!   publishes a safepoint that **rises every round and bites nothing**: the sweeper does the same
//!   flush and the same `compact_range`, so what separates the two arms is the collector's rule and
//!   not the engine's. Without it, a fall in the entry count is the engine's own compaction and
//!   reads as a collection.
//!
//! A `Kind::Lock` record is **not** reachable on a catalog key, whatever the isolation level:
//! `backend::store`'s `record_key` excludes the `'m'` namespace from a SERIALIZABLE read set on
//! purpose — every statement reads the catalog, so validating it would make every concurrent
//! `CREATE TABLE` a serialization failure — so no `Op::Check` is ever staged for one. #78 is real
//! and is pinned at the store, where it is reachable.

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

/// Runs the rounds, publishing whatever safepoint `safepoint` says between the drop and the lookup,
/// and answers with the entries standing and the safepoint reached.
///
/// The lookups are the assertion; the two numbers are the denominator.
fn rounds(arm: &str, safepoint: impl Fn(&Cluster, usize)) -> (u64, u64) {
    let cluster = Cluster::start_with(Settings {
        collecting: true,
        ..Settings::default()
    });
    let mut session = cluster.session();

    for round in 1..=ROUNDS {
        session
            .run("CREATE TABLE books (id bigserial primary key, title text)")
            .unwrap_or_else(|error| panic!("{arm} round {round}: CREATE: {error}"));
        // The lookup while it exists, and again after it is gone: the failing statement ran in
        // both states and the catalog has to answer in both.
        session.run(LOOKUP).unwrap_or_else(|error| {
            panic!(
                "{arm} round {round}: the lookup found a broken catalog \
                 while `books` existed: {error}"
            )
        });
        session
            .run("DROP TABLE books")
            .unwrap_or_else(|error| panic!("{arm} round {round}: DROP: {error}"));

        safepoint(&cluster, round);

        session
            .run(LOOKUP)
            .unwrap_or_else(|error| panic!("{arm} round {round}: {error}"));
    }

    let (entries, ssts) = cluster.standing();
    let reached = cluster.safepoint();
    println!(
        "  {arm:<12} {ROUNDS} rounds · {entries} entries in {ssts} tables · safepoint {reached}"
    );
    (entries, reached)
}

#[test]
fn a_name_record_never_outlives_the_table_it_names() {
    // **The control, and it runs first so its numbers are on the screen when the other fails.** A
    // safepoint that rises every round — so `raise_safepoint` asks the sweeper for a collection
    // every round, exactly as the other arm does — and that is far below every version's
    // `commit_ts`, which start above a thousand on this harness's oracle. The collector is
    // therefore reached, asked, and unable to drop anything.
    let (kept, control_safepoint) = rounds("control", |cluster, round| {
        cluster.publish_safepoint_at(round as u64);
    });

    // **A safepoint that bites, at the oracle's own `now`.** The retention window is subtracted by
    // *PD*, not here: `MvccCollector::effective_safepoint` adjusts a published safepoint by a
    // table's override against the cluster default and returns it unchanged when there is none, so
    // a store told `now` may collect everything at or below `now`. Every version a round wrote is
    // below it, and the next statement's timestamp is above it, which is what ADR 0110's decision 5
    // needs to serve the lookup that follows.
    let (collected, collecting_safepoint) = rounds("collecting", |cluster, _| {
        cluster.publish_safepoint();
    });

    assert!(
        control_safepoint < 1_000,
        "the control's safepoint {control_safepoint} reached the workload's timestamps, so it is \
         not a control"
    );
    assert!(
        collecting_safepoint > control_safepoint,
        "the collecting arm never got a safepoint above the control's"
    );
    assert!(
        collected < kept,
        "the collector decided nothing: {collected} entries standing with a safepoint above every \
         version, against {kept} with one below all of them, over {ROUNDS} identical rounds"
    );
}
