//! What a placement driver believes about its own group, across an open, a restart and an upgrade.
//!
//! The rules under test are
//! [ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)'s
//! first two: a group id is **derived once and then recorded**, and after that the record wins over
//! the command line. Everything dynamic membership does rests on those, and the first of them has
//! to be invisible to every deployment that already exists — which is what the first test is.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_pd::clock::TestClock;
use esker_pd::member::{MemberList, PdMember};
use esker_pd::{Clock, Pd, PdOptions};

fn clock() -> Arc<TestClock> {
    Arc::new(TestClock::new(1_700_000_000_000))
}

fn open(dir: &std::path::Path, members: MemberList) -> esker_pd::Result<Arc<Pd>> {
    Pd::open(
        dir,
        PdOptions {
            id: members.members()[0].id,
            members,
            ..PdOptions::with_clock(clock() as Arc<dyn Clock>)
        },
    )
}

fn three() -> Vec<PdMember> {
    vec![
        PdMember::new(1, "127.0.0.1:32379"),
        PdMember::new(2, "127.0.0.1:32380"),
        PdMember::new(3, "127.0.0.1:32381"),
    ]
}

/// **The upgrade is a no-op, and this is the test that says so.**
///
/// Every placement driver running before ADR 0060 computed its group id from its `--peers` list on
/// every start. If this build minted a fresh one, the first member upgraded would refuse the
/// others' traffic and the group would stop. So the id it writes down is exactly the id the old
/// build would have derived — asserted against the derivation itself rather than against a
/// constant, because a constant would pass if both moved together.
#[test]
fn a_placement_driver_records_the_id_the_previous_build_derived() {
    for members in [MemberList::alone(1), MemberList::new(three()).unwrap()] {
        let derived = members.derived_group_id();
        let dir = tempfile::tempdir().unwrap();
        let pd = open(dir.path(), members).unwrap();
        assert_eq!(
            pd.membership().group_id,
            derived,
            "a fresh placement driver named itself something the old build would not have"
        );
        assert_eq!(pd.members().recorded_group_id(), Some(derived));
    }
}

/// Minted once. A restart reads what is there rather than deriving again — which is the whole
/// point, because after a membership change the two answers differ.
#[test]
fn the_group_id_is_minted_once_and_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let first = open(dir.path(), MemberList::alone(1)).unwrap();
    let name = first.membership().group_id;
    drop(first);

    for _ in 0..3 {
        let again = open(dir.path(), MemberList::alone(1)).unwrap();
        assert_eq!(again.membership().group_id, name);
    }
}

/// The record wins over the command line, which is `esker-raft`'s own rule for membership: after a
/// change, `--peers` is exactly the out-of-date flag that rule is about.
#[test]
fn the_recorded_membership_wins_over_the_command_line() {
    let dir = tempfile::tempdir().unwrap();
    let founded = MemberList::new(three()).unwrap();
    let name = founded.derived_group_id();
    drop(open(dir.path(), founded).unwrap());

    // Started again with a list an operator forgot to update.
    let stale = MemberList::new(vec![
        PdMember::new(1, "127.0.0.1:32379"),
        PdMember::new(2, "127.0.0.1:32380"),
    ])
    .unwrap();
    let pd = open(dir.path(), stale).unwrap();
    assert_eq!(pd.members().len(), 3, "the stale flag won");
    assert_eq!(pd.membership().group_id, name);
    assert_eq!(pd.members().address_of(3), Some("127.0.0.1:32381"));
}

/// A member told to join a group it is not in refuses to open, rather than quietly rejoining the
/// wrong one — the mistake ADR 0011 was written about, one layer down.
#[test]
fn a_member_told_the_wrong_group_refuses_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let name = {
        let pd = open(dir.path(), MemberList::alone(1)).unwrap();
        pd.membership().group_id
    };

    let wrong = MemberList::alone(1).with_group_id(name ^ 1);
    let refused = open(dir.path(), wrong);
    assert!(
        refused.is_err(),
        "a placement driver adopted a group id that was not its own"
    );

    // And being told the right one is fine, which is what a restart of a joined member does.
    assert!(open(dir.path(), MemberList::alone(1).with_group_id(name)).is_ok());
}

/// Everything a single durable placement driver did, it still does. The group machinery is
/// underneath it and changes none of its answers.
#[test]
fn a_group_of_one_still_serves_the_moment_it_is_open() {
    let dir = tempfile::tempdir().unwrap();
    let pd = open(dir.path(), MemberList::alone(1)).unwrap();
    assert!(pd.is_serving());
    let done = pd.bootstrap(1, "127.0.0.1:20160").unwrap();
    assert!(done.region.is_some());
    assert!(pd.tso(1).unwrap() > 0);
    assert!(pd.alloc_id(1).unwrap() > 0);

    let membership = pd.membership();
    assert_eq!(membership.this_id, 1);
    assert_eq!(membership.leader_id, 1);
    assert_eq!(membership.members.len(), 1);
    assert_ne!(membership.group_id, 0, "zero is reserved");
}
