//! Differential and concurrency tests for the arena skiplist
//! ([ADR 0041](../../../docs/adr/0041-the-in-house-arena-skiplist.md) test plan items 2 and 3).
//!
//! # The model does not share the order it is checking
//!
//! [`Ordered`] restates the internal-key order — user key ascending, tag descending — rather
//! than calling [`InternalKeyComparator`]. A model built on the comparator under test would
//! agree with a skiplist that sorted every key into one bucket, because both would be asking
//! the same possibly-wrong question. One test checks the restatement against the real
//! comparator, so a drift between them is a failure rather than a silent agreement.
//!
//! # What the concurrency test proves, and what it does not
//!
//! `loom` is not on the dependency allowlist, so the concurrent test is threads and a seed. It
//! **does** prove that a reader traversing a list a writer is inserting into never sees a
//! key out of order, a torn key, or a key nobody wrote — the failures a wrong search or a
//! half-linked node produce, which show up on any machine.
//!
//! It does **not** prove the memory ordering. `Relaxed` and `Release` compile to the same store
//! on x86, and on `AArch64` the window is small enough that a passing run is not evidence. The
//! thing that can see a missing `Release` is Miri's data-race detector, and
//! [`SkipList::with_relaxed_publication`] exists so that it can be shown doing so —
//! `relaxed_publication_is_a_data_race` below is that demonstration and it is meant to fail.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as Memory};

use proptest::prelude::*;

use super::skiplist::{NIL, SkipList};
use crate::dbformat::{
    BytewiseComparator, Comparator, EntryKind, InternalKeyComparator, extract_user_key,
    internal_key, lookup_key,
};

fn comparator() -> Arc<InternalKeyComparator> {
    Arc::new(InternalKeyComparator::new(Arc::new(BytewiseComparator)))
}

fn list() -> SkipList {
    SkipList::new(comparator(), super::skiplist::DEFAULT_SEED)
}

/// An internal key under the order the memtable actually uses, restated.
///
/// User key ascending, then the eight-byte tag *descending*, so the newest version of a key
/// sorts first. The `Ord` here is written out rather than delegated on purpose: see the module
/// docs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ordered(Vec<u8>);

impl Ord for Ordered {
    fn cmp(&self, other: &Self) -> Ordering {
        let split = |key: &[u8]| -> (Vec<u8>, u64) {
            let at = key.len().saturating_sub(8);
            let mut tag = [0u8; 8];
            tag.copy_from_slice(&key[at..]);
            (key[..at].to_vec(), u64::from_le_bytes(tag))
        };
        let (user_a, tag_a) = split(&self.0);
        let (user_b, tag_b) = split(&other.0);
        match user_a.cmp(&user_b) {
            Ordering::Equal => tag_b.cmp(&tag_a),
            other => other,
        }
    }
}

impl PartialOrd for Ordered {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The restatement above and the comparator the skiplist is given must agree, or the model is
/// checking a different structure from the one that ships.
#[test]
fn the_model_order_and_the_real_comparator_agree() {
    let order = comparator();
    let keys: Vec<Vec<u8>> = ["", "a", "aa", "b", "z"]
        .iter()
        .flat_map(|user| {
            [0u64, 1, 2, 255, u64::from(u32::MAX)]
                .into_iter()
                .flat_map(move |seqno| {
                    [EntryKind::Put, EntryKind::Delete]
                        .into_iter()
                        .map(move |kind| internal_key(user.as_bytes(), seqno, kind))
                })
        })
        .collect();
    for a in &keys {
        for b in &keys {
            assert_eq!(
                Ordered(a.clone()).cmp(&Ordered(b.clone())),
                order.cmp(a, b),
                "{a:?} against {b:?}"
            );
        }
    }
}

/// One step of a generated program. The cursor is remembered by key rather than by index, so
/// that an insert landing ahead of a live cursor is modelled the way the skiplist behaves:
/// the cursor stays where it is and the next step sees the new entry.
#[derive(Debug, Clone)]
enum Op {
    Insert {
        user: u8,
        seqno: u8,
        value: u8,
    },
    Seek {
        user: u8,
        seqno: u8,
    },
    SeekForPrev {
        user: u8,
        seqno: u8,
    },
    Next,
    Prev,
    First,
    Last,
    /// The MVCC point read: seek to `(user_key, snapshot)` and look at what it landed on.
    PointRead {
        user: u8,
        snapshot: u8,
    },
}

fn op() -> impl Strategy<Value = Op> {
    // A small key space on purpose: collisions, adjacent keys and repeated versions of one key
    // are where the interesting cases are, and a wide space would mostly generate misses.
    prop_oneof![
        4 => (0u8..6, 0u8..6, 0u8..8).prop_map(|(user, seqno, value)| Op::Insert { user, seqno, value }),
        2 => (0u8..7, 0u8..7).prop_map(|(user, seqno)| Op::Seek { user, seqno }),
        2 => (0u8..7, 0u8..7).prop_map(|(user, seqno)| Op::SeekForPrev { user, seqno }),
        3 => Just(Op::Next),
        3 => Just(Op::Prev),
        1 => Just(Op::First),
        1 => Just(Op::Last),
        2 => (0u8..7, 0u8..7).prop_map(|(user, snapshot)| Op::PointRead { user, snapshot }),
    ]
}

fn user_key(user: u8) -> Vec<u8> {
    format!("key-{user}").into_bytes()
}

/// The cursor, in both worlds: a node in the skiplist and a key in the model.
struct Cursor {
    /// Where the skiplist's cursor is. `NIL` is invalid.
    node: u32,
    /// Where the model's cursor is, as a key rather than an index — so that an insert landing
    /// ahead of a live cursor is modelled the way the skiplist behaves: the cursor stays where
    /// it is and the next step sees the new entry.
    at: Option<Ordered>,
}

/// Applies one op to both worlds. Returns `false` when the op was skipped.
fn apply(
    list: &SkipList,
    model: &mut BTreeMap<Ordered, Vec<u8>>,
    cursor: &mut Cursor,
    op: &Op,
    step: usize,
) -> Result<bool, TestCaseError> {
    let sorted: Vec<Ordered> = model.keys().cloned().collect();
    let index = cursor
        .at
        .as_ref()
        .and_then(|key| sorted.iter().position(|k| k == key));
    match *op {
        Op::Insert { user, seqno, value } => {
            let key = Ordered(internal_key(
                &user_key(user),
                u64::from(seqno),
                EntryKind::Put,
            ));
            let value = vec![value; usize::from(value) + 1];
            if model.contains_key(&key) {
                // The skiplist keeps both and sorts the newer first, which has its own test;
                // the map cannot model that, so the program skips it in both.
                return Ok(false);
            }
            prop_assert!(
                list.insert(&key.0, b"", &value),
                "step {}: the arena refused",
                step
            );
            model.insert(key, value);
            return Ok(false);
        }
        Op::Seek { user, seqno } => {
            let key = Ordered(internal_key(
                &user_key(user),
                u64::from(seqno),
                EntryKind::Put,
            ));
            cursor.node = list.seek(&key.0);
            cursor.at = sorted.iter().find(|k| **k >= key).cloned();
        }
        Op::SeekForPrev { user, seqno } => {
            let key = Ordered(internal_key(
                &user_key(user),
                u64::from(seqno),
                EntryKind::Put,
            ));
            cursor.node = list.seek_for_prev(&key.0);
            cursor.at = sorted.iter().rev().find(|k| **k <= key).cloned();
        }
        Op::Next => {
            cursor.node = list.after(cursor.node);
            cursor.at = index.and_then(|i| sorted.get(i + 1).cloned());
        }
        Op::Prev => {
            cursor.node = list.before(cursor.node);
            cursor.at = index
                .filter(|i| *i > 0)
                .and_then(|i| sorted.get(i - 1).cloned());
        }
        Op::First => {
            cursor.node = list.first();
            cursor.at = sorted.first().cloned();
        }
        Op::Last => {
            cursor.node = list.last();
            cursor.at = sorted.last().cloned();
        }
        Op::PointRead { user, snapshot } => {
            // "The first key under a prefix": one seek answers a point read, because versions
            // of a user key sort newest-first within it.
            let user = user_key(user);
            let key = Ordered(lookup_key(&user, u64::from(snapshot)));
            cursor.node = list.seek(&key.0);
            cursor.at = sorted.iter().find(|k| **k >= key).cloned();
            let expected = cursor
                .at
                .as_ref()
                .filter(|k| extract_user_key(&k.0) == user.as_slice());
            let found = (cursor.node != NIL)
                .then(|| list.key(cursor.node))
                .filter(|k| extract_user_key(k) == user.as_slice());
            prop_assert_eq!(
                found.map(<[u8]>::to_vec),
                expected.map(|k| k.0.clone()),
                "step {}: point read of {:?} at {}",
                step,
                user,
                snapshot
            );
        }
    }
    Ok(true)
}

/// Runs `ops` against the skiplist and against a `BTreeMap`, and stops at the first difference.
fn differential(ops: &[Op]) -> Result<(), TestCaseError> {
    let list = list();
    let mut model: BTreeMap<Ordered, Vec<u8>> = BTreeMap::new();
    let mut cursor = Cursor {
        node: NIL,
        at: None,
    };

    for (step, op) in ops.iter().enumerate() {
        if !apply(&list, &mut model, &mut cursor, op, step)? {
            continue;
        }
        prop_assert_eq!(
            cursor.node != NIL,
            cursor.at.is_some(),
            "step {}: validity after {:?}",
            step,
            op
        );
        if let Some(key) = &cursor.at {
            prop_assert_eq!(
                list.key(cursor.node),
                &key.0[..],
                "step {}: key after {:?}",
                step,
                op
            );
            prop_assert_eq!(
                list.value(cursor.node),
                &model[key][..],
                "step {}: value after {:?}",
                step,
                op
            );
        }
    }

    // Whatever the program did to the cursor, the whole list still reads in order.
    let mut walked = Vec::new();
    let mut node = list.first();
    while node != NIL {
        // Bounded: a wrongly linked tower can leave a cycle at level zero, and an unbounded
        // walk over one hangs rather than fails.
        prop_assert!(walked.len() < list.len(), "the level-zero list has a cycle");
        walked.push(Ordered(list.key(node).to_vec()));
        node = list.after(node);
    }
    prop_assert_eq!(walked, model.keys().cloned().collect::<Vec<_>>());
    prop_assert_eq!(list.len(), model.len());
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(if cfg!(miri) { 2 } else { 256 }))]

    /// Insert, seek, seek-for-prev, step forward, step backward and the MVCC point read, all
    /// against a `BTreeMap` under a restated order.
    #[test]
    fn the_skiplist_answers_what_a_sorted_map_would(ops in prop::collection::vec(op(), 1..80)) {
        differential(&ops)?;
    }
}

/// A prefix scan: seek to the first version of a user key and walk while the user key holds.
/// This is what a range scan over one row's versions does, and it is the shape that breaks if
/// `seek` lands one entry off.
#[test]
fn a_prefix_scan_sees_every_version_of_one_key_and_nothing_else() {
    let list = list();
    for user in ["k1", "k2", "k3"] {
        for seqno in 1..=5u64 {
            let key = internal_key(user.as_bytes(), seqno, EntryKind::Put);
            assert!(list.insert(&key, b"", user.as_bytes()));
        }
    }
    let mut node = list.seek(&lookup_key(b"k2", u64::MAX));
    let mut seen = Vec::new();
    while node != NIL && extract_user_key(list.key(node)) == b"k2" {
        seen.push(list.key(node).to_vec());
        node = list.after(node);
    }
    assert_eq!(seen.len(), 5, "every version of k2");
    let seqnos: Vec<u64> = seen
        .iter()
        .map(|key| crate::dbformat::split_internal_key(key).unwrap().1)
        .collect();
    assert_eq!(seqnos, vec![5, 4, 3, 2, 1], "newest first, within the key");
}

/// Released when the writer thread leaves, however it leaves.
///
/// Without it, a writer that panics leaves every reader spinning on a count that will never
/// arrive and the suite hangs instead of failing — which is what a deliberately broken skiplist
/// did to the test below the first time it was run against one.
struct Finished(Arc<AtomicU64>);

impl Drop for Finished {
    fn drop(&mut self) {
        self.0.store(u64::MAX, Memory::Release);
    }
}

/// A reader walking the list while a writer inserts into it must see a strictly increasing
/// sequence of real keys — never a duplicate, never a key going backwards, never a key nobody
/// wrote. See the module docs for what this does and does not prove.
#[test]
fn readers_see_an_ordered_list_while_a_writer_inserts() {
    // Miri interprets every instruction, so a run sized for a native build would take hours
    // there. Small is still meaningful under Miri, because what Miri is looking for is a
    // missing happens-before edge and one insert is enough to have or not have it.
    #[cfg(miri)]
    const WRITES: usize = 40;
    #[cfg(not(miri))]
    const WRITES: usize = 4000;
    let list = Arc::new(list());
    let order = comparator();
    let done = Arc::new(AtomicU64::new(0));

    std::thread::scope(|scope| {
        let writer = {
            let list = Arc::clone(&list);
            let done = Arc::clone(&done);
            scope.spawn(move || {
                let _finished = Finished(Arc::clone(&done));
                for i in 0..WRITES as u64 {
                    // Keys arrive in a scattered order so the writer is linking into the middle
                    // of the list rather than always appending to its end.
                    let user = format!("k{:05}", (i * 2_654_435_761) % 100_000);
                    let key = internal_key(user.as_bytes(), i + 1, EntryKind::Put);
                    assert!(list.insert(&key, b"", &i.to_le_bytes()));
                    done.store(i + 1, Memory::Release);
                }
            })
        };
        for _ in 0..4 {
            let list = Arc::clone(&list);
            let order = Arc::clone(&order);
            let done = Arc::clone(&done);
            scope.spawn(move || {
                while done.load(Memory::Acquire) < WRITES as u64 {
                    let mut previous: Option<Vec<u8>> = None;
                    let mut node = list.first();
                    let mut steps = 0usize;
                    while node != NIL {
                        steps += 1;
                        assert!(steps <= WRITES, "the level-zero list has a cycle");
                        let key = list.key(node).to_vec();
                        assert_eq!(key.len(), "k00000".len() + 8, "a torn key");
                        let value = list.value(node);
                        assert_eq!(value.len(), 8, "a torn value");
                        if let Some(previous) = &previous {
                            assert_eq!(
                                order.cmp(previous, &key),
                                Ordering::Less,
                                "the scan went backwards or repeated an entry"
                            );
                        }
                        previous = Some(key);
                        node = list.after(node);
                    }
                }
            });
        }
        writer.join().expect("the writer thread");
    });

    assert_eq!(list.len(), WRITES, "every insert is present");
}

/// The demonstration ADR 0041 item 3 calls not negotiable, and it is **meant to fail**.
///
/// It builds a list whose publishing store is `Relaxed` instead of `Release`, so a reader that
/// reaches a node has no happens-before edge to the bytes the node names. Run it under Miri:
///
/// ```text
/// cargo +nightly miri test -p esker-engine --lib -- --ignored relaxed_publication
/// ```
///
/// Miri must report a data race. If it passes, the checker is not looking at the thing this
/// change's whole risk lives in, and no green run of the tests above means anything.
///
/// It is `#[ignore]`d because outside Miri it says nothing: on x86 the two orderings emit the
/// same instruction, so the test would pass and look like evidence.
#[test]
#[ignore = "meant to fail, and only Miri can see it: see the doc comment"]
fn relaxed_publication_is_a_data_race() {
    let list = Arc::new(SkipList::with_relaxed_publication(comparator()));
    std::thread::scope(|scope| {
        let writer = {
            let list = Arc::clone(&list);
            scope.spawn(move || {
                for i in 0..40u64 {
                    let key = internal_key(format!("k{i:04}").as_bytes(), i + 1, EntryKind::Put);
                    list.insert(&key, b"", &[b'v'; 32]);
                }
            })
        };
        for _ in 0..2 {
            let list = Arc::clone(&list);
            scope.spawn(move || {
                for _ in 0..40 {
                    let mut node = list.first();
                    while node != NIL {
                        std::hint::black_box(list.key(node));
                        std::hint::black_box(list.value(node));
                        node = list.after(node);
                    }
                }
            });
        }
        writer.join().expect("the writer thread");
    });
}
