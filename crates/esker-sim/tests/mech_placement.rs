//! The placement model reaches the state the narrow rule could not see, and the checker fires on
//! it.
//!
//! Nothing here proves anything about `esker-pd`. It proves things about the **model**, which is
//! the prerequisite: a model that never reaches a learner on a live store cannot catch a rule
//! that only mishandles one, and a checker that cannot be made to go red is decoration. The proof
//! about the real placement driver is `crates/esker-pd/tests/sim_balance.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sim::mech::placement::{MidRepair, Report, Violation, run};
use esker_sim::mech::reference::ReferenceBalance;

/// Seeds every scenario runs. Fixed, so a failure is a rerun rather than a story.
const SEEDS: [u64; 24] = [
    1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597, 2584, 4181, 6765, 10946,
    17711, 28657, 46368, 75025,
];

/// Rounds per seed. Long enough that a repair started early is still open when a store comes back.
const ROUNDS: u64 = 400;

#[test]
fn the_model_reaches_a_learner_on_a_live_store() {
    let mut totals = Report::default();
    for seed in SEEDS {
        let report = run(seed, ROUNDS, &ReferenceBalance::current())
            .unwrap_or_else(|violation| panic!("the current rule should be clean: {violation}"));
        totals.moves_applied += report.moves_applied;
        totals.declined_mid_repair += report.declined_mid_repair;
        totals.declined_learner_all_live += report.declined_learner_all_live;
        totals.learners_added += report.learners_added;
        totals.learners_promoted += report.learners_promoted;
        totals.stores_downed += report.stores_downed;
    }

    println!("{} seeds x {ROUNDS} rounds: {totals:?}", SEEDS.len());

    // The state the whole model exists to reach: a region holding an unpromoted learner with
    // every one of its stores up. Asserted with a floor rather than an equality because the
    // schedule is seeded and the exact count is not the point — that it happens thousands of
    // times over 24 seeds is.
    assert!(
        totals.declined_learner_all_live > 1_000,
        "the model reached a learner-on-a-live-store only {} times in {} seeds x {ROUNDS} \
         rounds. That is the one state `is_mid_repair` was widened to cover, so a model that \
         does not reach it cannot catch the bug. Totals: {totals:?}",
        totals.declined_learner_all_live,
        SEEDS.len(),
    );
    assert!(
        totals.learners_promoted > 0 && totals.stores_downed > 0 && totals.moves_applied > 0,
        "the model never got through a whole repair, or never moved anything: {totals:?}"
    );
}

#[test]
fn the_checker_goes_red_against_the_rule_as_it_was() {
    // `ReferenceBalance::narrow` is the definition before 548dd62: a region is mid-repair only
    // while it holds a peer on a *down* store. Every seed must produce a violation, and every
    // violation must name the learner rather than the down store — if the checker fired on
    // `PeerOnDownStore` it would be catching something the narrow rule handles correctly, which
    // would mean the model, not the rule, was wrong.
    let mut seen = 0;
    for seed in SEEDS {
        let Err(violation) = run(seed, ROUNDS, &ReferenceBalance::narrow()) else {
            panic!(
                "seed {seed}: the narrow rule ran {ROUNDS} rounds clean. The checker cannot see \
                 the bug it was written for."
            );
        };
        let Violation::BalancedMidRepair { why, .. } = violation;
        assert!(
            matches!(
                why,
                MidRepair::UnpromotedLearner {
                    all_stores_live: true,
                    ..
                }
            ),
            "seed {seed}: the checker fired for the wrong reason ({why:?}). The narrow rule \
             handles a peer on a down store correctly; the only thing it gets wrong is a learner \
             on a live store."
        );
        seen += 1;
    }
    assert_eq!(seen, SEEDS.len(), "every seed must reproduce it");
}

#[test]
fn a_run_is_a_function_of_its_seed() {
    for seed in [7, 99, 12_345] {
        let first = run(seed, ROUNDS, &ReferenceBalance::current()).unwrap();
        let second = run(seed, ROUNDS, &ReferenceBalance::current()).unwrap();
        assert_eq!(
            first, second,
            "seed {seed} produced two different runs, so a failure it printed would not reproduce"
        );
    }
}
