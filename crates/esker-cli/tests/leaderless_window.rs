//! **How long a region has no leader — on four real store processes.**
//!
//! `docs/plans/debts-v1.1.md` #34 was measured on the in-process harness: at 192–325 regions a
//! region that loses its leader was still refusing writes thirty seconds later, fourteen sightings
//! in four runs and not one of them recovered. The row's first question is not the fix, it is
//! **whether that is the system or the harness** — four stores in one process driving three hundred
//! Raft groups at a five-millisecond tick is its own explanation, and nothing has ruled it out.
//!
//! This is the arm that rules it in or out: the same load, the same measurement, on the same
//! binaries a user would run — four `esker server` processes, a real placement driver and a real
//! SQL node, each in its own address space with its own scheduler.
//!
//! **It measures, it does not assert a duration.** What it prints is how long the caller would have
//! had to wait, which is the number #34 is short of on this side of the comparison.
//!
//! `cluster_harness`'s own `Cluster::run` waits these refusals out, which is right for a test of
//! something else and wrong here — the wait *is* the measurement — so this drives `query` directly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster_harness;

use std::time::{Duration, Instant};

use cluster_harness::Cluster;

/// Where the instrument stops calling it a window.
const GIVE_UP_AFTER: Duration = Duration::from_secs(30);

/// How many sightings are enough to say something about the shape.
const SIGHTINGS: usize = 6;

#[test]
#[ignore = "debts #34, arm (a): four store processes, minutes, and it wants a quiet box"]
fn how_long_a_region_has_no_leader_on_real_processes() {
    let cluster = Cluster::start_with(4, 8 * 1024);
    cluster.run("CREATE TABLE t (id bigint PRIMARY KEY, filler text)");

    let filler = "x".repeat(256);
    let mut met = 0_usize;
    let mut cleared: Vec<Duration> = Vec::new();
    println!("\n  four store processes, 8 KiB split threshold");
    println!("  row     regions   outcome");

    for at in (1..=10_000_i64).step_by(250) {
        if met >= SIGHTINGS {
            break;
        }
        let values: Vec<String> = (at..at + 250)
            .map(|id| format!("({id}, '{filler}')"))
            .collect();
        let statement = format!("INSERT INTO t VALUES {}", values.join(", "));

        let answer = cluster.query(&statement);
        if !answer.contains("peer is not the leader") {
            // Anything else — a commit, or a refusal this measurement is not about — is somebody
            // else's row. A `40003` is retried by hand here for the reason `Cluster::run` gives:
            // every insert names its own primary key, so a retry either writes it or answers
            // `23505` about its own first attempt.
            if answer.contains("ERROR") && Cluster::waited_out(&answer) {
                let _ = cluster.query(&statement);
            }
            continue;
        }

        met += 1;
        let regions = cluster.regions();
        let began = Instant::now();
        loop {
            let again = cluster.query(&statement);
            if !again.contains("ERROR") || again.contains("23505") {
                // **Printed here, not at the end.** A measurement under a budget that prints a
                // summary prints nothing when the budget ends it — learned at 602 s on the
                // in-process arm.
                println!(
                    "  {at:<6}  {regions:>7}   cleared in {:>8.1} ms",
                    began.elapsed().as_secs_f64() * 1000.0
                );
                cleared.push(began.elapsed());
                break;
            }
            if began.elapsed() >= GIVE_UP_AFTER {
                println!(
                    "  {at:<6}  {regions:>7}   still refused after {:?}: {}",
                    began.elapsed(),
                    again.trim().lines().next().unwrap_or("")
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let mut millis: Vec<u128> = cleared.iter().map(Duration::as_millis).collect();
    millis.sort_unstable();
    println!(
        "\n  {met} sightings, {} cleared inside {GIVE_UP_AFTER:?}",
        millis.len()
    );
    if let (Some(min), Some(max)) = (millis.first(), millis.last()) {
        println!(
            "  min {min} ms   median {} ms   max {max} ms",
            millis[millis.len() / 2]
        );
    }
    // **The comparison this exists for**, stated where the reader is: the in-process arm saw
    // fourteen sightings and zero recoveries inside thirty seconds. A run here that clears them is
    // the harness being the cause; a run that does not is the system.
    println!(
        "  in-process arm, for comparison: 14 sightings at 192-325 regions, 0 cleared inside 30 s"
    );
}
