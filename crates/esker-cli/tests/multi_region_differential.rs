//! **A multi-region table never answers wrong** — whichever engine the system says it will use.
//!
//! # What this is for
//!
//! Measured on a real four-store cluster on 2026-09-05: a table in four regions with a columnar
//! learner on each, every fragment answered, and every aggregate **four times** its true value —
//! `count(*)` of twenty thousand rows answering eighty thousand. A learner's columnar runs were
//! not scoped to the region the fragment asked about, so every row was counted once per region
//! (`docs/bench/mpp-baseline.md` §10). That is the failure
//! [ADR 0022](../../../docs/adr/0022-columnar-learner-replica.md) names as the worst this feature
//! can have: the two engines disagreeing, silently.
//!
//! # Why it asserts the declared state rather than one engine
//!
//! The store-side fix arrives in halves, and a half of it is already on `main`: with the copy
//! built from the region's range but the scan range not yet applied, a two-hundred-row table
//! answers 357; with the scan range on and the build scoping absent, 52. So the system's honest
//! answer today is *"do not route this"*, and `FragmentSource::runs_are_region_scoped` is where it
//! says so.
//!
//! This test reads that declaration **end to end**, out of `EXPLAIN`, and asserts what the
//! declaration promises:
//!
//! * **declared `false`** — the guard must have fired, `EXPLAIN` must name the rule, and the
//!   answers must still be *right*, because a fallback that answers wrongly is no better than the
//!   thing it guards;
//! * **declared `true`** — every comparison must have run on the columns (`Engine: columnar`) and
//!   agreed with the row engine.
//!
//! So it is green on `main` in the guarded state and its demand turns on the moment the flip
//! happens. **No `#[ignore]`**: it runs every time, and what it checks — that a multi-region table
//! is never answered wrongly — is true in both states and is the property that actually matters.
//!
//! # Retirement
//!
//! Nothing retires this test. What changes is which branch it takes: when `esker-store`'s columnar
//! copy is region-scoped in **both** halves and `ClientFragments::runs_are_region_scoped` returns
//! `true`, the `declared true` branch becomes the live one and this becomes the multi-region
//! differential ADR 0022 asks for by name. Until then it is the check that the guard is really
//! guarding.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod cluster_harness;

use cluster_harness::{Cluster, rows_of};

/// Four stores: a region has three voters and PD places a columnar learner on a store with no peer
/// of that region, so four is the fewest that can hold one.
const STORES: u16 = 4;

/// Rows loaded before the comparison: few and fat, so the table splits in seconds.
const ROWS: i64 = 200;

/// Bytes of payload per row.
const PAYLOAD: usize = 4_096;

/// The region-split threshold the stores run with.
const SPLIT_SIZE: u64 = 262_144;

/// The reason string the interim guard prints, from `esker_sql::exec::fragment`.
const GUARD: &str = "not region-scoped";

/// Every query compared, with the answer the fixture makes true.
///
/// **The expected values are computed from the fixture, not from one of the engines.** Agreement
/// between two engines is not correctness — they agreed at four times the right answer when both
/// were reading too much would have been caught only by knowing the number — and in the guarded
/// state both arms are the row engine, where agreement is free.
fn queries() -> Vec<(&'static str, Vec<Vec<String>>)> {
    let sum: i64 = (1..=ROWS).sum();
    vec![
        ("SELECT count(*) FROM ledger", vec![vec![ROWS.to_string()]]),
        (
            "SELECT count(amount), sum(amount) FROM ledger",
            vec![vec![ROWS.to_string(), sum.to_string()]],
        ),
        (
            "SELECT min(amount), max(amount) FROM ledger",
            vec![vec!["1".to_owned(), ROWS.to_string()]],
        ),
        (
            "SELECT bucket, count(*) FROM ledger GROUP BY bucket ORDER BY bucket",
            (0..4)
                .map(|bucket| vec![bucket.to_string(), (ROWS / 4).to_string()])
                .collect(),
        ),
    ]
}

/// How long one query's fragments may take to start answering before it is compared without them.
///
/// Smaller than the table-wide wait above it on purpose: by the time the loop runs, *some* query
/// has already answered from the columns, so this is the tail of one learner catching up rather
/// than placement completing.
const READINESS: u64 = 60;

#[test]
fn a_multi_region_table_is_never_answered_wrongly() {
    let cluster = Cluster::start_with(STORES, SPLIT_SIZE);
    cluster.run("CREATE TABLE ledger (id int8 PRIMARY KEY, bucket int8, amount int8, note text)");
    cluster.run("ALTER TABLE ledger SET (columnar_replicas = 1)");
    let payload = "x".repeat(PAYLOAD);
    for batch in 0..ROWS / 20 {
        let values: Vec<String> = (0..20)
            .map(|row| {
                let id = batch * 20 + row + 1;
                format!("({id},{},{id},'{payload}')", id % 4)
            })
            .collect();
        cluster.run(&format!("INSERT INTO ledger VALUES {}", values.join(",")));
    }

    let regions = cluster.wait_for_a_split(120);
    cluster.wait_for_learners(180);

    // **Placement is not readiness**, and the difference is exactly the confusion this test must
    // not make: a learner that exists and has not caught up refuses, the planner answers from the
    // rows, and "columnar agreed" would then mean "columnar never ran". Waited for before the
    // declaration is read, because the declaration is read out of a plan.
    if cluster
        .query_on("auto", "EXPLAIN SELECT count(*) FROM ledger")
        .contains("Engine: columnar")
        || !cluster
            .query_on("auto", "EXPLAIN SELECT count(*) FROM ledger")
            .contains(GUARD)
    {
        cluster.wait_until_fragments_answer("SELECT count(*) FROM ledger", 180);
    }

    // **The declaration, read end to end.** `EXPLAIN` is where a `FragmentSource` that says it
    // cannot scope a fragment to one region becomes visible, and reading it here rather than
    // hard-coding a state is what lets one test hold in both.
    let probe = cluster.query_on("auto", "EXPLAIN SELECT count(*) FROM ledger");
    let guarded = probe.contains(GUARD);
    eprintln!(
        "the source declares runs_are_region_scoped = {}",
        if guarded { "false" } else { "true" }
    );

    // **Readiness is waited for per query, and a timeout defers that query rather than failing
    // it.** The wait above is one query's — `count(*)` — and a learner catches up per column
    // family and per region, so it says nothing about `min(amount)`. Asserting `Engine: columnar`
    // for every query after waiting for one is what reddened this test six times in a night, each
    // time green on the rerun: the message said "had to be answered by the columns" where the
    // truth was "was not ready yet", which is a readiness state wearing a correctness message's
    // clothes.
    //
    // What is **not** relaxed is the answer. `rows == expected` is asserted for every query in
    // every state, and a query whose columns did answer is still compared against them. A
    // deferral only drops the *declaration* for that one query, and the guard after the loop
    // refuses the degenerate case where nothing ran on the columns at all — a test that deferred
    // everything would be comparing the row engine with itself and calling the agreement
    // evidence, which is the trap this file's own header warns about.
    let mut on_the_columns = 0_usize;
    let mut deferred: Vec<&str> = Vec::new();

    for (query, expected) in queries() {
        // **One observation, kept.** `columnar_within` answering `true` and then planning the
        // query again is two samples of a state that is not monotone: a learner that has caught
        // up can fall behind under load, and the second plan then says `Engine: rows` for a query
        // the wait had ready — which is how this assertion fired at load 12 *after* the bounded
        // wait landed, on `SELECT count(*) FROM ledger`, the first query in the list. So the plan
        // the wait saw is the plan this loop asserts on, and the guarded arm plans for itself.
        let ready_plan = if guarded {
            None
        } else {
            cluster.columnar_plan_within(query, READINESS)
        };
        let ready = guarded || ready_plan.is_some();
        let rows = rows_of(&cluster.query_on("row", query));
        let plan = ready_plan
            .unwrap_or_else(|| cluster.query_on("auto", &format!("EXPLAIN ANALYZE {query}")));
        let flat: Vec<String> = expected
            .iter()
            .map(|row| row.first().cloned().unwrap_or_default())
            .collect();

        if guarded {
            assert!(
                plan.contains("Engine: rows") && plan.contains(GUARD),
                "the source cannot scope a fragment, so `{query}` over {regions} regions had to \
                 be guarded and was not:\n{plan}"
            );
        } else if ready {
            assert!(
                plan.contains("Engine: columnar"),
                "the source declares its runs are region-scoped, so `{query}` over {regions} \
                 regions had to be answered by the columns:\n{plan}"
            );
            on_the_columns += 1;
        } else {
            deferred.push(query);
        }

        // True in both states, and the whole point: the answer is right.
        assert_eq!(
            rows, flat,
            "the row engine answered `{query}` wrongly over {regions} regions"
        );
        // The columnar arm is compared whenever it can answer at all. In the guarded state that is
        // the declaration's own claim; in the deferred one there is nothing to compare against,
        // and saying so is the point of the guard below.
        if ready {
            let columns = rows_of(&cluster.query_on("columnar", query));
            assert_eq!(
                columns, flat,
                "the columnar arm answered `{query}` wrongly over {regions} regions"
            );
        }
    }

    if !deferred.is_empty() {
        eprintln!(
            "harness: {} of {} queries were still on the rows after {READINESS}s and were \
             compared without the columns: {deferred:?}",
            deferred.len(),
            queries().len()
        );
    }
    assert!(
        guarded || on_the_columns > 0,
        "every one of the {} queries deferred, so nothing ran on the columns and this round \
         compared the row engine with itself: {deferred:?}",
        deferred.len()
    );
}

/// **The transient-refusal list, against the sentences it was built from.**
///
/// The three rounds this change was verified with all passed on the first attempt and printed no
/// `harness:` line, which means neither new waiting path ran: at moderate load the split produced
/// no transient refusal and every fragment was ready. Three green runs prove the change did not
/// break the test; they prove nothing about the code that only runs when it is *not* green.
///
/// So the half that is a pure function of a string gets a test, with the strings copied from the
/// gate logs that reddened this test six times in one night. The other half —
/// `Cluster::columnar_plan_within` answering `None` and the loop deferring that query — is only
/// reachable against a cluster whose learner is behind, and is not exercised here; the guard after
/// the loop is what stops a deferral from being silent.
///
/// **And the round after that one found the third state.** The wait was bounded and the deferral
/// was recorded, and the test still failed at load 12 on `SELECT count(*) FROM ledger`: the wait
/// saw `Engine: columnar` and the loop's own `EXPLAIN ANALYZE`, a moment later, saw
/// `Engine: rows`. Readiness is not monotone — a learner that has caught up falls behind again —
/// so two samples of it disagree under load. The loop keeps the plan the wait saw instead of
/// asking twice. Nothing about the *answers* moved: `rows == expected` is still asserted in every
/// state and the columnar arm is still compared whenever it answered at all.
#[test]
fn the_refusals_the_harness_waits_out_are_the_ones_it_measured() {
    for waited in [
        "ERROR 40003: the transaction's outcome is unknown: the TxnPrewrite may or may not have \
         been applied: connection closed: region 1 stopped leading with this proposal in its log; \
         it may still commit",
        "ERROR 08006: could not reach the store: deadline passed after 14 attempts",
        "ERROR 08006: could not reach the store: gave up after 9 attempts: peer is not the leader \
         of region 1",
        "ERROR 25006: this node's schema lease has expired and the placement driver is unreachable",
    ] {
        assert!(
            Cluster::waited_out(waited),
            "should be waited out: {waited}"
        );
    }

    // And the ones that must fail on sight. The first is the reason `08006` is matched by its
    // *sentence* and not by its sqlstate: a store that is gone answers the same class as a region
    // being re-elected, and only the words tell them apart. The second is a duplicate key, which
    // `run` accepts **only** after an unknown outcome — a fixture that writes a row twice on the
    // first attempt is a defect in the fixture.
    for refused in [
        "ERROR 08006: could not reach the store: connection refused",
        "ERROR 23505: duplicate key value violates unique constraint \"ledger_pkey\"",
        "ERROR 42P01: relation \"ledger\" does not exist",
        "ERROR 22003: bigint out of range",
    ] {
        assert!(
            !Cluster::waited_out(refused),
            "should fail on sight: {refused}"
        );
    }
}
