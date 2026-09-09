//! **Bounds a user supplied in the wrong order** — the sweep the `BETWEEN` panic asked for.
//!
//! `WHERE id BETWEEN 3 AND 2` bounded a primary-key scan below by 3 and above by 2, and
//! `BTreeMap::range` panics on such a pair rather than answering empty: a query anyone can type
//! took the node down. The fix was one guard; the question it left was **where else** a range is
//! built from bounds nobody checked, and this file is the answer.
//!
//! Two more were there. `tsrange(hi, lo)` built an impossible range object where the *literal*
//! `'[hi,lo)'::tsrange` raised `22000`, and `daterange(hi, lo)` quietly answered `empty` — the
//! worse of the two, because a query then filters nothing and nothing reports a problem. Both are
//! one bug: a constructor that did not go through the normalisation the literal takes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the one table it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `daterange` there and `text` here. The `daterange(a, b)` **expression** is modelled as its
    // text — a half-open pair with no brackets to report — while a stored range column is a
    // `Datum::Range`; the two shapes are documented where they part company
    // (`exec::cursor::range_value_function`). The rows agree, which is what this file is about.
    types: &[],
    answers: &[],
};

#[test]
fn every_crossed_bounds_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_crossed_bounds.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 15, "the corpus shrank: {checked} statements");
}

/// **Every `Txn` this crate has answers a crossed range with no rows**, which is the rule the
/// panic was a violation of.
///
/// Asserted against the backend rather than through SQL because that is where the rule lives: the
/// planner is *right* to ask for `[3, 2)` — nothing is in it — and it is the implementation that
/// must not panic. `BTreeMap::range` does, which is why the check cannot be left to callers.
#[test]
fn a_crossed_key_range_is_no_rows_and_not_a_panic() {
    use esker_sql::backend::{Backend, MemoryBackend};

    let backend = MemoryBackend::new();
    let mut txn = backend.begin().unwrap();
    txn.put(b"a", b"1");
    txn.put(b"b", b"2");
    txn.put(b"c", b"3");
    txn.commit().unwrap();

    let txn = backend.begin().unwrap();
    assert_eq!(txn.scan(b"a", b"d", 0).unwrap().len(), 3);
    // The empty range, three ways: crossed, equal, and crossed by one byte.
    assert!(txn.scan(b"c", b"a", 0).unwrap().is_empty());
    assert!(txn.scan(b"b", b"b", 0).unwrap().is_empty());
    assert!(txn.scan(b"b\x00", b"b", 0).unwrap().is_empty());
    // And a transaction's **own buffered writes** are ranged by the same pair, so the guard has to
    // come before both seeks and not between them.
    let mut txn = backend.begin().unwrap();
    txn.put(b"bb", b"4");
    assert!(txn.scan(b"c", b"a", 0).unwrap().is_empty());
    assert_eq!(txn.scan(b"a", b"d", 0).unwrap().len(), 4);
}
