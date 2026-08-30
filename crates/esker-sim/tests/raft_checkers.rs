//! The four Raft safety checkers, each shown red.
//!
//! These histories are written by hand rather than produced by a cluster, so that each one
//! isolates exactly one property. A checker that has only ever seen legal input is a checker
//! nobody has tested.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sim::raft::{EntryDigest, NodeSnapshot, SafetyChecker, Violation};

/// A log of `(term, payload byte)` pairs starting at index 1.
fn log(entries: &[(u64, u8)]) -> Vec<EntryDigest> {
    entries
        .iter()
        .enumerate()
        .map(|(offset, &(term, byte))| EntryDigest::of(offset as u64 + 1, term, &[byte]))
        .collect()
}

/// A node in the ordinary case: online, following, nothing compacted.
fn node(id: u64, term: u64, commit: u64, log: &[EntryDigest]) -> NodeSnapshot<'_> {
    NodeSnapshot {
        id,
        online: true,
        is_leader: false,
        term,
        commit,
        compacted_through: 0,
        snapshot_term: 0,
        prefix_anchor: 0,
        settled: true,
        log,
        applied: &[],
    }
}

fn leader(id: u64, term: u64, commit: u64, log: &[EntryDigest]) -> NodeSnapshot<'_> {
    NodeSnapshot {
        is_leader: true,
        ..node(id, term, commit, log)
    }
}

#[test]
fn an_ordinary_replicated_cluster_is_clean() {
    let entries = log(&[(1, b'a'), (1, b'b'), (2, b'c')]);
    let mut checker = SafetyChecker::new();
    for commit in 0..=3 {
        checker
            .observe(&[
                leader(1, 2, commit, &entries),
                node(2, 2, commit, &entries),
                node(
                    3,
                    2,
                    commit.saturating_sub(1),
                    &entries[..2.min(entries.len())],
                ),
            ])
            .unwrap();
    }
    assert_eq!(checker.leader_of(2), Some(1));
    assert_eq!(checker.committed_upto(), 3);
}

#[test]
fn two_leaders_in_one_term_are_caught() {
    let entries = log(&[(1, b'a')]);
    let mut checker = SafetyChecker::new();
    checker.observe(&[leader(1, 7, 1, &entries)]).unwrap();

    let violation = checker
        .observe(&[leader(2, 7, 1, &entries)])
        .expect_err("two leaders in term 7 were accepted");
    assert_eq!(
        violation,
        Violation::ElectionSafety {
            term: 7,
            first: 1,
            second: 2
        }
    );
}

/// The same node leading the same term across many events is not two leaders, and a node
/// leading a *later* term is the normal case.
#[test]
fn one_leader_per_term_across_terms_is_fine() {
    let entries = log(&[(1, b'a')]);
    let mut checker = SafetyChecker::new();
    for term in 1..6 {
        for _ in 0..3 {
            checker
                .observe(&[leader(term % 3 + 1, term, 1, &entries)])
                .unwrap();
        }
    }
}

#[test]
fn a_diverged_prefix_under_a_shared_index_and_term_is_caught() {
    // Both nodes have an entry at index 3 in term 2, but they disagree at index 2 — exactly
    // what the log matching property forbids.
    let honest = log(&[(1, b'a'), (1, b'b'), (2, b'c')]);
    let diverged = log(&[(1, b'a'), (1, b'X'), (2, b'c')]);

    let mut checker = SafetyChecker::new();
    checker.observe(&[node(1, 2, 0, &honest)]).unwrap();
    let violation = checker
        .observe(&[node(2, 2, 0, &diverged)])
        .expect_err("two logs with the same index and term but different prefixes were accepted");
    match violation {
        Violation::LogMatching {
            index,
            term,
            node,
            other,
            ..
        } => {
            assert_eq!((index, term), (2, 1), "the first divergence is at index 2");
            assert_eq!((node, other), (2, 1));
        }
        other => panic!("expected a log matching violation, got {other}"),
    }
}

/// A follower whose tail is overwritten by a new leader is *not* a violation: the entries it
/// loses had a different term, so no rule was broken.
#[test]
fn a_legally_truncated_follower_is_not_a_violation() {
    let stale = log(&[(1, b'a'), (1, b'b'), (1, b'c')]);
    let winner = log(&[(1, b'a'), (1, b'b'), (2, b'z')]);
    let mut checker = SafetyChecker::new();
    checker.observe(&[node(2, 1, 2, &stale)]).unwrap();
    checker
        .observe(&[leader(1, 2, 2, &winner), node(2, 2, 2, &winner)])
        .unwrap();
}

#[test]
fn a_leader_missing_a_committed_entry_is_caught() {
    let committed = log(&[(1, b'a'), (1, b'b')]);
    let mut checker = SafetyChecker::new();
    // Index 2 is committed in term 1.
    checker
        .observe(&[leader(1, 1, 2, &committed), node(2, 1, 2, &committed)])
        .unwrap();

    // Node 3 becomes leader of term 2 with a log that never had index 2.
    let short = log(&[(1, b'a')]);
    let violation = checker
        .observe(&[leader(3, 2, 1, &short)])
        .expect_err("a leader without a committed entry was accepted");
    match violation {
        Violation::LeaderCompleteness {
            leader,
            leader_term,
            index,
            committed_in,
            ..
        } => assert_eq!((leader, leader_term, index, committed_in), (3, 2, 2, 1)),
        other => panic!("expected a leader completeness violation, got {other}"),
    }
}

/// A leader that has the committed entries and more is the ordinary case, and a leader is not
/// answerable for what was committed in its own term before it heard about it.
#[test]
fn a_leader_that_has_everything_committed_is_fine() {
    let entries = log(&[(1, b'a'), (1, b'b')]);
    let longer = log(&[(1, b'a'), (1, b'b'), (2, b'c')]);
    let mut checker = SafetyChecker::new();
    checker.observe(&[leader(1, 1, 2, &entries)]).unwrap();
    checker.observe(&[leader(2, 2, 2, &longer)]).unwrap();
    checker.observe(&[leader(2, 2, 3, &longer)]).unwrap();
}

/// The logs agree, so log matching is silent; it is the *apply loop* that differs. This is the
/// bug state-machine safety exists to catch: two replicas feeding their state machines
/// different things from the same log.
#[test]
fn two_state_machines_applying_different_entries_are_caught() {
    let shared = log(&[(1, b'a'), (1, b'b')]);
    let forged = log(&[(1, b'a'), (1, b'X')]);

    let mut checker = SafetyChecker::new();
    let first = NodeSnapshot {
        applied: &shared,
        ..node(1, 1, 0, &shared)
    };
    checker.observe(&[first]).unwrap();

    let second = NodeSnapshot {
        applied: &forged,
        ..node(2, 1, 0, &shared)
    };
    let violation = checker
        .observe(&[second])
        .expect_err("two nodes applied different entries at index 2");
    match violation {
        Violation::StateMachineSafety {
            index, node, other, ..
        } => assert_eq!((index, node, other), (2, 2, 1)),
        other => panic!("expected a state machine safety violation, got {other}"),
    }
}

#[test]
fn a_state_machine_that_skips_an_index_is_caught() {
    let entries = log(&[(1, b'a'), (1, b'b'), (1, b'c')]);
    let skipped = [entries[0], entries[2]];
    let mut checker = SafetyChecker::new();
    let observed = NodeSnapshot {
        applied: &skipped,
        ..node(1, 1, 3, &entries)
    };
    let violation = checker
        .observe(&[observed])
        .expect_err("a state machine skipped index 2");
    assert_eq!(
        violation,
        Violation::ApplyOutOfOrder {
            node: 1,
            previous: 1,
            got: 3
        }
    );
}

/// Applying in several observations, a few entries at a time, is the normal case and must not
/// be mistaken for a gap.
#[test]
fn applying_incrementally_is_fine() {
    let entries = log(&[(1, b'a'), (1, b'b'), (1, b'c'), (1, b'd')]);
    let mut checker = SafetyChecker::new();
    for upto in 0..=entries.len() {
        let observed = NodeSnapshot {
            applied: &entries[..upto],
            ..node(1, 1, upto as u64, &entries)
        };
        checker.observe(&[observed]).unwrap();
    }
}

#[test]
fn a_second_entry_committed_at_an_already_committed_index_is_caught() {
    let first = log(&[(1, b'a'), (1, b'b')]);
    let second = log(&[(1, b'a'), (2, b'z')]);
    let mut checker = SafetyChecker::new();
    checker.observe(&[node(1, 1, 2, &first)]).unwrap();
    let violation = checker
        .observe(&[node(2, 2, 2, &second)])
        .expect_err("index 2 was committed twice with different entries");
    assert!(
        matches!(violation, Violation::CommittedTwice { index: 2, .. }),
        "expected a double-commit violation at index 2, got {violation}"
    );
}

/// A log whose indices are not a contiguous ascending run is a driver bug, and the checker
/// says so instead of computing nonsense from it.
#[test]
fn a_malformed_log_is_reported_not_silently_accepted() {
    let entries = [EntryDigest::of(1, 1, b"a"), EntryDigest::of(3, 1, b"c")];
    let mut checker = SafetyChecker::new();
    assert_eq!(
        checker.observe(&[node(1, 1, 0, &entries)]),
        Err(Violation::MalformedLog {
            node: 1,
            expected: 2,
            got: 3
        })
    );
}

/// A compacted log starts above index 1, and the anchor is what keeps its prefix digests
/// comparable with an uncompacted node's.
#[test]
fn a_compacted_log_still_matches_an_uncompacted_one() {
    let full = log(&[(1, b'a'), (1, b'b'), (2, b'c')]);
    let mut checker = SafetyChecker::new();
    checker.observe(&[node(1, 2, 3, &full)]).unwrap();

    let anchor = checker
        .prefix_digest(1, 1)
        .expect("index 1 term 1 should be recorded");
    let compacted = NodeSnapshot {
        compacted_through: 1,
        snapshot_term: 1,
        prefix_anchor: anchor,
        log: &full[1..],
        ..node(2, 2, 3, &full[1..])
    };
    checker
        .observe(&[compacted])
        .expect("a compacted log with the right anchor must agree with the full one");

    // ...and a wrong anchor — a snapshot that does not describe the prefix it claims — does not.
    let lying = NodeSnapshot {
        compacted_through: 1,
        snapshot_term: 1,
        prefix_anchor: anchor ^ 1,
        log: &full[1..],
        ..node(3, 2, 3, &full[1..])
    };
    assert!(
        checker.observe(&[lying]).is_err(),
        "a snapshot claiming a prefix it does not have was accepted"
    );
}

/// A restart drops whatever was not durable, so a node's log can get *shorter* between
/// observations. That is legal, and the incremental chain has to survive it.
#[test]
fn a_node_whose_log_shrinks_across_a_restart_is_rechecked_not_trusted() {
    let full = log(&[(1, b'a'), (1, b'b'), (1, b'c')]);
    let mut checker = SafetyChecker::new();
    checker.observe(&[node(1, 1, 0, &full)]).unwrap();
    // Restarted: only the first two entries were fsynced.
    checker.observe(&[node(1, 1, 0, &full[..2])]).unwrap();
    // Re-replicated, same entries: still consistent.
    checker.observe(&[node(1, 1, 0, &full)]).unwrap();
    // Re-replicated with a *different* entry under the same index and term: not consistent.
    let forged = log(&[(1, b'a'), (1, b'b'), (1, b'X')]);
    assert!(
        checker.observe(&[node(1, 1, 0, &forged)]).is_err(),
        "an entry was replaced under the same index and term"
    );
}
