//! #72: a store that has never been told a safepoint still compacts by itself.
//!
//! # The question
//!
//! Run 127i found **no natural compaction at all** on `b9f8ca83`, where run 127h on `aaf70ad9` had
//! twenty-seven over the same file at the same write buffer. The two commits are separated by
//! ADR 0110, and the guess was that a gate which lets nothing be dropped is a reason not to bother
//! compacting — a store whose safepoint is zero has nothing collectable, so perhaps the scheduler
//! had learned to skip.
//!
//! # Why there is nothing to find in the diff
//!
//! `git diff aaf70ad9 b9f8ca83 -- crates/esker-engine` is **empty**. The scheduling path —
//! `DbInner::maybe_compact` → `Picker::pick` → `Picker::worst_level` → `Picker::score` — is
//! byte-identical across the two, and none of those four reads a safepoint, a compaction filter or
//! the collector: a level's score is its file count at L0 and its bytes below. The whole
//! store-side difference is thirty-five lines that apply a published safepoint and refuse a read
//! beneath it.
//!
//! There is one droppability-shaped early return on that path — `maybe_compact`'s
//! `Picker::can_discharge`, which leaves a column family alone while a snapshot older than a range
//! tombstone is still open — and it is about **snapshots**, not safepoints, and is also unchanged.
//!
//! # What this asserts
//!
//! The claim the guess makes, stated so that it can fail: with the safepoint at zero, compactions
//! still happen. If a future change ever does make scheduling depend on there being something to
//! drop, this is what says so.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use cluster::{Cluster, Settings};

/// Small enough that this workload actually flushes.
///
/// It writes about 36 KiB per store per round, so the engine's own 64 MiB buffer is some eighteen
/// hundred rounds from its first flush and 4 MiB is a hundred — which is why run 127e watched
/// twenty files for forty-six minutes and saw no compaction. A level score has nothing to say
/// until something reaches a level.
const WRITE_BUFFER: usize = 32 * 1024;

#[test]
fn a_store_with_no_safepoint_still_compacts_by_itself() {
    let cluster = Cluster::start_with(Settings {
        write_buffer_size: Some(WRITE_BUFFER),
        ..Settings::default()
    });
    let mut session = cluster.session();

    for round in 1..=12 {
        for at in 0..12 {
            session
                .run(&format!(
                    "CREATE TABLE t{at} (id bigserial primary key, a bigint, b text)"
                ))
                .unwrap();
            for row in 0..4 {
                session
                    .run(&format!("INSERT INTO t{at} (a, b) VALUES ({row}, 'r')"))
                    .unwrap();
            }
            session.run(&format!("DROP TABLE t{at}")).unwrap();
        }
        let (entries, ssts) = cluster.standing();
        println!(
            "  round {round:>3}: compactions {:>4} · snapshots {:>3} · {entries:>7} entries in \
             {ssts:>3} tables · memtable {:>8} B",
            cluster.engine_counter("esker.compactions"),
            cluster.engine_counter("esker.snapshots"),
            cluster.memtable_bytes(),
        );
    }

    // **The denominator.** The claim is about a store with no safepoint, so the test has to be one:
    // nothing in this harness publishes one, and if something ever starts to, this stops being the
    // experiment it says it is.
    assert_eq!(
        cluster.safepoint(),
        0,
        "a safepoint was published, so this no longer tests a store that has none"
    );
    let compactions = cluster.engine_counter("esker.compactions");
    assert!(
        compactions > 0,
        "no compaction ran in twelve rounds with the levels over their trigger — scheduling has \
         come to depend on there being something to drop, which is what #72 asked about"
    );
}
