//! The same work, twelve times, and the store does not get more expensive.
//!
//! # What this is the acceptance for
//!
//! #58's remaining half is space: the same statements cost more the longer a node has run,
//! because the store keeps what nothing can read.
//! [ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md) settled
//! **whether** a version may be dropped — a published safepoint — and #62 settled **that a
//! compaction drops it once nothing below can be hiding an older one**. What is left is **when a
//! compaction happens at all**, which is #70, and the answer today is "it does not".
//!
//! r1's four runs measured the gap. At the default 64 MiB write buffer, twenty files over
//! forty-six minutes produced **no compaction at all** (run 127e), and one forced pass then threw
//! away 73.9% of the entries with the live relation count unmoved. At 4 MiB, compactions fire
//! often enough to make the cost oscillate, and run 127h aligned the oscillation with them: the
//! round after one that compacted cost **−1.3%**, the round after one that did not **+13.8%**.
//!
//! # What this measures, and why it is not what the files hold
//!
//! The obvious probe is what the SSTs hold — and at the default write buffer it reports **zero**,
//! for twelve rounds, because nothing ever flushes. That is 127e's finding stated from the other
//! side, and it is why the curve here is `esker.entries-stepped`: the entries a read walks,
//! wherever they live. It sees the memtable, which is where a short run keeps everything, and it
//! is what #58 is a complaint about — the same statement, walking more dead versions each time.
//!
//! Measured before any of this was built, it climbed by **+4032 entries a round, exactly**, from
//! round four on, with no scatter at all. That is twelve dropped tables' worth of versions that
//! nothing can read and nothing removes.
//!
//! # Why the assertion is a slope and not a ceiling
//!
//! A ceiling on the final count is a number somebody has to choose, and it passes for a workload
//! that is merely smaller than the guess. What #58 is about is **growth**: identical work, and the
//! store keeps more of it every round. So this fits a line through the rounds and requires the
//! slope's interval to contain zero — a store that holds steady passes at any size, and one that
//! ratchets fails however small it starts.
//!
//! # The denominator
//!
//! "It did not grow" is satisfied perfectly by a run in which nothing happened at all — no work,
//! no safepoint, no compaction. Each of those is a way to pass while measuring nothing, so each is
//! asserted before the slopes are.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    reason = "a test, and the counts it fits a line through are far below f64's exact range"
)]

mod cluster;

use cluster::Cluster;

/// Rounds. Twelve is the smallest number that gives the fit something to say and still runs in a
/// test's worth of time; r1's 127h used eighteen.
const ROUNDS: usize = 12;

/// Tables created and dropped per round — the shape `DROP TABLE IF EXISTS` gave #58, and the one
/// that leaves a version nothing can read behind every time.
const TABLES: usize = 12;

/// One round of identical work: the same names, created, filled, and dropped.
///
/// **The names repeat on purpose.** A round that used fresh names would grow the catalog, and the
/// growth this test is about is the one that happens when the catalog's *size* is held still —
/// which is what #58's round 3 established, and what makes this a test about retained versions
/// rather than about data.
fn one_round(session: &mut cluster::Session) {
    for at in 0..TABLES {
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
}

/// The least-squares slope of `ys` against `0..n`, and the half-width of its 95% interval.
///
/// Written out rather than pulled in: the dependency policy allows no statistics crate, and the
/// lines below are the whole of what this needs. The interval is the textbook one for the slope of
/// a simple regression, with `t ≈ 2` — at ten degrees of freedom the true value is 2.23, so this is
/// slightly *generous* to the implementation, which is the direction a test should err in when it
/// is about to call something a regression.
fn slope_with_interval(ys: &[u64]) -> (f64, f64) {
    let n = ys.len() as f64;
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = ys.iter().sum::<u64>() as f64 / n;
    let sxx: f64 = (0..ys.len()).map(|x| (x as f64 - mean_x).powi(2)).sum();
    let sxy: f64 = ys
        .iter()
        .enumerate()
        .map(|(x, y)| (x as f64 - mean_x) * (*y as f64 - mean_y))
        .sum();
    let slope = sxy / sxx;
    let residuals: f64 = ys
        .iter()
        .enumerate()
        .map(|(x, y)| (*y as f64 - (mean_y + slope * (x as f64 - mean_x))).powi(2))
        .sum();
    let standard_error = (residuals / (n - 2.0) / sxx).sqrt();
    (slope, 2.0 * standard_error)
}

/// One arm: twelve identical rounds, and what each of them cost to read.
///
/// Returns the per-round read cost and the files standing at the end of each round.
fn twelve_rounds(collecting: bool) -> (Vec<u64>, Vec<u64>) {
    let cluster = Cluster::start_with(cluster::Settings {
        collecting,
        ..cluster::Settings::default()
    });
    let mut session = cluster.session();

    let mut read_cost = Vec::with_capacity(ROUNDS);
    let mut tables = Vec::with_capacity(ROUNDS);
    let mut last_stepped = 0u64;
    println!("  --- collecting: {collecting} ---");
    for round in 1..=ROUNDS {
        one_round(&mut session);
        // What a store's heartbeat delivers, and nothing more: a number. Whether anything happens
        // because of it is the whole question this asks, and it is published on both arms so that
        // the only difference between them is whether the store acts on it.
        cluster.publish_safepoint();

        let stepped = cluster.engine_counter("esker.entries-stepped");
        let (held, files) = cluster.standing();
        println!(
            "  round {round:>3}: stepped {:>8} · {held:>8} entries in {files:>3} tables · \
             memtable {:>9} B · levels {:?}",
            stepped - last_stepped,
            cluster.memtable_bytes(),
            cluster.levels(),
        );
        read_cost.push(stepped - last_stepped);
        tables.push(files);
        last_stepped = stepped;
    }

    // **The denominator**, on whichever arm can have one. Each of these is a way an arm could
    // report nothing and be mistaken for a result.
    assert!(
        last_stepped > 0,
        "no entry was read in {ROUNDS} rounds, so nothing on this arm is being measured"
    );
    assert!(
        cluster.safepoint() > 0,
        "no store is working to a safepoint, so neither arm was ever allowed to drop anything \
         and the two are the same experiment"
    );
    let compactions = cluster.engine_counter("esker.compactions");
    assert_eq!(
        compactions > 0,
        collecting,
        "the collecting arm must compact and the control must not; this one ran {compactions}"
    );
    (read_cost, tables)
}

/// **#70's acceptance, with its own control.** Identical work, twelve rounds, default write
/// buffer: collecting takes the climb out.
///
/// # Why two arms and not a flat line
///
/// The shape this was specified in is "the slope's 95% interval contains zero", and that is still
/// the goal — but it is **not reachable yet**, and the reason is worth more than the assertion
/// would be. With collection on, the climb drops by about 93%: `+4032` entries a round becomes
/// `+289`. What is left is **+96 entries and exactly one SST per round**, every file in L6, and
/// the cause is one thing seen twice:
///
/// A dropped table's catalog rows keep their newest version at or below the safepoint, because
/// that is what a read at the safepoint would return — `MvccCollector::keep_as_newest`, which
/// does not look at the record's kind, so a `Kind::Delete` is kept exactly as a `Put` would be.
/// Twelve dropped tables leave ninety-six such records a round. And because `CREATE TABLE` takes a
/// **fresh table id** every time, each round's survivors sit at a strictly higher, disjoint key
/// range, so the sweep's output overlaps nothing already in L6 and lands as a new file — which the
/// bottom level never merges away, `Picker::worst_level` having no level to compact it into.
///
/// Dropping that record needs the collector to know that nothing older survives anywhere, which is
/// a question about the key's **whole version range** and not about one engine key. That is a
/// design decision of its own; this test asserts what is settled and names what is not.
///
/// # What the control is for
///
/// A fraction needs a denominator, and a hard number for "small enough" would be a number somebody
/// chose. The control arm is the same workload on the same build with collection switched off, so
/// the comparison is against this machine on this day rather than against a constant.
#[test]
fn collecting_takes_the_climb_out_of_the_same_work() {
    let (collected, collected_files) = twelve_rounds(true);
    let (control, control_files) = twelve_rounds(false);

    let with = slope_with_interval(&collected);
    let without = slope_with_interval(&control);
    println!(
        "  read cost: collecting {:+.1}/round ±{:.1} · control {:+.1}/round ±{:.1} · \
         files: collecting {:+.2} · control {:+.2}",
        with.0,
        with.1,
        without.0,
        without.1,
        slope_with_interval(&collected_files).0,
        slope_with_interval(&control_files).0,
    );

    // The control has to climb, or there is nothing for collecting to have taken out.
    assert!(
        without.0 - without.1 > 0.0,
        "the control did not get more expensive ({:+.1}/round, 95% interval ±{:.1}), so this run \
         has no #58 in it to fix — {control:?}",
        without.0,
        without.1,
    );
    // Four times, not a hard number: measured, it is fourteen times, and the margin is there so
    // that a slower machine reports a regression rather than the weather.
    // **ADR 0111's own acceptance**, and the half that is now flat outright rather than merely
    // smaller. Before it, the sweep's output for a round of dropped tables was a file of immortal
    // delete records at a key range nothing would write again — `levels {6: 1}` through
    // `{6: 12}` over twelve rounds, one more every time, each consulted by every read. With the
    // segment rule the files oscillate between two and three and the count does not grow.
    //
    // **A slope on this arm alone, because this quantity has no denominator.** The spread was
    // `high - low <= 2` over the settled rounds and it went red on a gate running several clusters
    // at once: the sweeper's debounce lands differently against the rounds when the box is busy, so
    // a round is sampled just before a sweep instead of just after and an oscillation between two
    // and three reads as one to four. Nothing about the claim had changed.
    //
    // The control cannot stand in for it. `twelve_rounds` asserts that the control **never
    // compacts**, so it has no sweep output at all and its file count is flat — measured `+0.00`
    // against the collecting arm's `+0.08`. The `{6: 1}` … `{6: 12}` climb this rule is about was
    // the *collecting* arm before ADR 0111, not the control. So the read cost gets a ratio and this
    // gets a bound, and the bound is named rather than borrowed.
    //
    // **The bound is the mechanism, not a taste.** Before the segment rule each sweep of a round of
    // dropped tables emitted exactly one file of immortal delete records — one a round, twelve over
    // twelve, each consulted by every read. So "less than half a file a round" is not a threshold
    // somebody picked between two numbers; it is "that does not happen any more", sitting between
    // the defect's `+1.00` by construction and the `+0.08` and `+0.15` measured on two runs here.
    // Two observations is thin, and the bound is deliberately nearer the defect than the noise:
    // a slope of `+0.50` over eleven rounds is five more files by the end, which is the mechanism
    // and not a sampling artefact.
    //
    // **A slope and not a spread, which is the load-sensitive half of the old assertion.** A spread
    // is decided by its two extremes, so one unlucky sample widens it; a least-squares slope over
    // eleven rounds is decided by the trend, and load moves *where in the oscillation* a round
    // lands without making the count climb. The objection that retired the slope once — that a fit
    // over an oscillation is a small number falling on either side of zero — was about asserting a
    // **sign**. This asserts a one-sided bound half an order of magnitude away from zero, which a
    // wobbling sign cannot reach.
    //
    // **From the second round.** The first has no files at all — nothing has been swept yet, so its
    // zero is "no count" and not "a count of zero".
    let files_with = slope_with_interval(&collected_files[1..]).0;
    assert!(
        files_with < 0.5,
        "the tables on the books grew {files_with:+.2} a round while collecting — before ADR 0111 \
         that was one more every round, each a file of delete records at a key range nothing \
         writes again and each consulted by every read. collecting {collected_files:?}, control \
         {control_files:?}"
    );
    assert!(
        with.0 * 4.0 <= without.0,
        "collecting left {:+.1} entries a round of climb against the control's {:+.1} — under a \
         quarter is what collecting keeping up looks like, and it was a fourteenth when this was \
         written. collecting {collected:?}, control {control:?}",
        with.0,
        without.0,
    );
}

/// One arm of the spilling workload: what `default` holds after each round.
fn twelve_spilling_rounds(collecting: bool) -> Vec<u64> {
    /// Past `SHORT_VALUE_MAX_LEN` (255), so prewrite spills it rather than inlining it.
    const WIDE: usize = 1024;

    let cluster = Cluster::start_with(cluster::Settings {
        collecting,
        ..cluster::Settings::default()
    });
    let mut session = cluster.session();
    let wide = "x".repeat(WIDE);

    let mut spilled = Vec::with_capacity(ROUNDS);
    println!("  --- collecting: {collecting} ---");
    for round in 1..=ROUNDS {
        for at in 0..TABLES {
            session
                .run(&format!(
                    "CREATE TABLE w{at} (id bigserial primary key, b text)"
                ))
                .unwrap();
            for row in 0..2 {
                session
                    .run(&format!("INSERT INTO w{at} (b) VALUES ('{wide}{row}')"))
                    .unwrap();
            }
            session.run(&format!("DROP TABLE w{at}")).unwrap();
        }
        // Both arms publish, so the only difference between them is whether the store acts on it.
        cluster.publish_safepoint();
        // And both flush, so both are counted the same way: the collecting arm flushes on its way
        // through a sweep and the control would otherwise report zero for everything it holds.
        cluster.flush();
        let held = cluster.entries_in("default");
        println!("  round {round:>3}: default holds {held:>7} entries");
        spilled.push(held);
    }
    spilled
}

/// **ADR 0112's arm of this probe.** A workload whose values spill does not grow `default` either.
///
/// Every value the rounds above write is a catalog record short enough to inline, so `default`
/// stays empty and the measurement says nothing about **bytes** — which is exactly why #58's space
/// half was not closed by it. A value past `esker_txn::SHORT_VALUE_MAX_LEN` lands in `default`
/// keyed by `(user_key, start_ts)`, and until ADR 0112 nothing collected those: #60 measured
/// `write` 96 → 8 against `default` 96 → 96.
///
/// **With its own control**, because "it did not grow" is satisfied perfectly by a workload that
/// never spilled: the same rounds with collection off are what say the values were there to be
/// collected.
#[test]
fn a_spilling_workload_does_not_grow_the_default_family() {
    let collected = twelve_spilling_rounds(true);
    let control = twelve_spilling_rounds(false);

    // **The denominator, and it is a curve rather than a threshold.** Without collection the
    // spilled values accumulate, one per version written and never removed.
    let grew = control.last().copied().unwrap_or(0);
    assert!(
        grew > 0,
        "the control held nothing in `default` after {ROUNDS} rounds, so nothing spilled and this \
         measured the same inlined workload as the test above — {control:?}"
    );

    let held = collected.iter().max().copied().unwrap_or(0);
    assert!(
        held * 4 <= grew,
        "collecting left {held} spilled values standing against the control's {grew} — a value \
         whose `write` record was collected has nothing that can ever name it again, and nothing \
         collected those until ADR 0112. collecting {collected:?}, control {control:?}"
    );
}
