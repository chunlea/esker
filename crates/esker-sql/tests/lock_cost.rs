//! **What a hundred-row `FOR UPDATE` would cost as a lock-only prewrite** — the number
//! [ADR 0088](../../../docs/adr/0088-a-row-lock-across-nodes.md) (a') said it wanted before
//! anything was built.
//!
//! The decision the number settles is not *whether* the lock goes to the store — that is ruled —
//! but whether the acquisition is **one round trip per statement** or **one per row**. A hundred
//! sequential round trips inside a statement would be a different feature at a different price,
//! and the ADR says the batching has to come first if that is what it is.
//!
//! Four things are timed, all against the same three-store cluster:
//!
//! * **today** — `SELECT … FOR UPDATE` over the rows, minus the same `SELECT` without the clause.
//!   The difference is what the in-process hash table costs, which is the thing being replaced.
//! * **batched, one region** — a transaction that prewrites one key and *checks* a hundred, minus
//!   the same transaction checking none. `TxnMutation::Check` is the lock ADR 0088 uses (tag 5,
//!   ADR 0067 — or tag 7, carrying a READ COMMITTED statement's read timestamp, since ADR 0114 §2),
//!   and `commit` groups the checked keys by region exactly as it groups writes, so a
//!   hundred keys of one table are one request.
//! * **batched, three regions** — the same hundred keys spread across all three, which is what a
//!   split table looks like. It is the same measurement with the group count changed, and it is
//!   here to show that the cost is per *region* and not per row.
//! * **per row** — a hundred sequential `LatestCommit` round trips. That is the floor of an
//!   unbatched shape: the real thing would also write a lock record, so a per-row acquisition
//!   cannot be cheaper than this.
//!
//! It prints a table and asserts only that it ran. A timing assertion here would be a stopwatch on
//! a shared box, which is the detector `crates/esker-client/tests/prewrite_ordering.rs` deliberately
//! removed from its own two-node test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster;

use std::time::{Duration, Instant};

use bytes::Bytes;

/// Rows locked, which is the ADR's own number.
const ROWS: usize = 100;

/// Rounds per measurement; the median is reported, so the box's worst moment does not decide.
const ROUNDS: usize = 5;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn timed(mut body: impl FnMut()) -> Duration {
    median(
        (0..ROUNDS)
            .map(|_| {
                let started = Instant::now();
                body();
                started.elapsed()
            })
            .collect(),
    )
}

fn ms(duration: Duration) -> String {
    format!("{:.2} ms", duration.as_secs_f64() * 1000.0)
}

/// `a - b`, floored at zero: a difference of two medians can come out negative on a noisy box, and
/// reporting a negative cost would be reporting the noise as a finding.
fn minus(a: Duration, b: Duration) -> Duration {
    a.checked_sub(b).unwrap_or_default()
}

#[test]
#[ignore = "a measurement, not an assertion; starts a real cluster and prints a table"]
fn what_a_hundred_row_lock_costs() {
    let cluster = cluster::Cluster::start();
    let mut node = cluster.session();
    node.run("CREATE TABLE locked (id bigint primary key, n bigint)")
        .unwrap();
    let values = (1..=ROWS)
        .map(|i| format!("({i}, {i})"))
        .collect::<Vec<_>>()
        .join(", ");
    node.run(&format!("INSERT INTO locked (id, n) VALUES {values}"))
        .unwrap();

    // The keys as the store actually holds them: region 3 is `['t', )`, which is where every row
    // and index entry of every table lives (`cluster`'s own module docs).
    let keys: Vec<Bytes> = {
        let txn = cluster.client.begin().unwrap();
        txn.scan(b"t", b"u", 4096)
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .take(ROWS)
            .collect()
    };
    assert_eq!(keys.len(), ROWS, "the scan did not find the rows");

    // The same count spread over all three regions, which is what a split table looks like.
    let spread: Vec<Bytes> = (0..ROWS)
        .map(|i| {
            let namespace = b"anu"[i % 3];
            Bytes::from(format!("{}spread-{i:04}", char::from(namespace)))
        })
        .collect();

    // A key of its own for the primary, in region 3 with the rows, so that "one region" is one
    // region for the whole transaction and not two.
    let primary = Bytes::from_static(b"tzz-lock-cost-primary");

    let select = "SELECT id, n FROM locked ORDER BY id".to_owned();
    let for_update = format!("{select} FOR UPDATE");
    let plain = timed(|| {
        node.rows(&select);
    });
    let locking = timed(|| {
        node.rows(&for_update);
    });

    let commit_with = |checks: &[Bytes]| {
        let mut txn = cluster.client.begin().unwrap();
        txn.put(&primary, b"1");
        txn.checking(checks.iter().cloned(), Vec::new());
        txn.commit().unwrap();
    };
    let baseline = timed(|| commit_with(&[]));
    // The one-region hundred is the middle rung of the loop below, so it is not measured twice.
    let three_regions = timed(|| commit_with(&spread));

    // What writing the same hundred rows costs, which is the comparison that turns the number
    // below into a sentence: a lock record is a replicated write like any other.
    let update = timed(|| {
        node.run("UPDATE locked SET n = n + 1").unwrap();
    });

    let per_row = timed(|| {
        let txn = cluster.client.begin().unwrap();
        for key in &keys {
            txn.latest_commit(key).unwrap();
        }
    });

    println!("\n  {ROWS} rows, median of {ROUNDS} rounds, one three-store cluster\n");
    println!("  today   SELECT … FOR UPDATE      {}", ms(locking));
    println!("          SELECT … (no clause)     {}", ms(plain));
    println!(
        "          what the lock costs      {}\n",
        ms(minus(locking, plain))
    );
    println!("          UPDATE the same {ROWS} rows {}\n", ms(update));
    println!("  batched commit, no checks        {}", ms(baseline));
    for rung in [10, ROWS, 2 * ROWS] {
        let checks: Vec<Bytes> = keys.iter().cycle().take(rung).cloned().collect();
        let checks: Vec<Bytes> = {
            // `cycle` repeats a key past `ROWS`, and a repeated key is one lock, not two — so the
            // rung above the row count is padded with keys of its own instead.
            let mut unique = checks;
            unique.truncate(ROWS.min(rung));
            unique.extend((ROWS..rung).map(|i| Bytes::from(format!("tzz-pad-{i:04}"))));
            unique
        };
        let at = timed(|| commit_with(&checks));
        println!(
            "          + {rung:3} checks, 1 region   {}   the locks cost {}",
            ms(at),
            ms(minus(at, baseline))
        );
    }
    println!(
        "          + {ROWS} checks, 3 regions  {}",
        ms(three_regions)
    );
    println!(
        "          the {ROWS} locks cost       {}\n",
        ms(minus(three_regions, baseline))
    );
    println!("  per row {ROWS} sequential round trips {}", ms(per_row));
    println!(
        "          one round trip           {}\n",
        ms(per_row / u32::try_from(ROWS).unwrap())
    );
}
