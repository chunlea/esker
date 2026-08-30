//! The linearizability checker, proven in both directions.
//!
//! A checker that has never been shown red is decoration, so every "this passes" test here is
//! paired with a "this fails" test that changes one thing. The property tests generate legal
//! histories from a real sequential execution (which must always pass) and then corrupt one
//! response (which must always fail).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use bytes::Bytes;
use esker_sim::lin::{
    CheckOutcome, Checker, Completion, History, HistoryError, Model, Op, OpId, Register,
    RegisterInput, RegisterOutput,
};
use proptest::prelude::*;

fn value(byte: u8) -> Bytes {
    Bytes::copy_from_slice(&[byte])
}

fn read_of(byte: u8) -> RegisterOutput {
    RegisterOutput::Value(Some(value(byte)))
}

fn check(history: &History<RegisterInput, RegisterOutput>) -> CheckOutcome {
    Checker::new().check(&Register, history)
}

/// The trivial case: one client, no concurrency at all. If this is ever not linearizable the
/// checker is broken, not the history.
#[test]
fn a_sequential_history_is_linearizable() {
    let mut history = History::new();
    for byte in 0..8_u8 {
        let write = history.invoke(1, RegisterInput::Write(value(byte)));
        history.respond(write, RegisterOutput::Written).unwrap();
        let read = history.invoke(1, RegisterInput::Read);
        history.respond(read, read_of(byte)).unwrap();
    }
    assert!(check(&history).is_linearizable());
}

/// The canonical violation DESIGN §11 cares about: a write is acknowledged, and a read that
/// starts strictly afterwards still returns the old value. No order explains that.
#[test]
fn a_stale_read_after_an_acked_write_is_caught() {
    let mut history = History::new();
    let write = history.invoke(1, RegisterInput::Write(value(1)));
    history.respond(write, RegisterOutput::Written).unwrap();

    let read = history.invoke(2, RegisterInput::Read);
    history.respond(read, RegisterOutput::Value(None)).unwrap();

    let outcome = check(&history);
    assert!(
        !outcome.is_linearizable(),
        "a stale read after an acked write was accepted: {outcome}"
    );
    let CheckOutcome::NotLinearizable { linearized, report } = outcome else {
        panic!("expected a violation")
    };
    assert_eq!(
        linearized,
        vec![OpId(0)],
        "the report should place the write and get stuck on the read"
    );
    assert!(report.contains("! op1"), "the read is not marked: {report}");
}

/// A read that *overlaps* the write may return either value: both orders exist.
#[test]
fn an_overlapping_read_may_return_either_value() {
    for observed in [RegisterOutput::Value(None), read_of(1)] {
        let mut history = History::new();
        let write = history.invoke(1, RegisterInput::Write(value(1)));
        let read = history.invoke(2, RegisterInput::Read);
        history.respond(read, observed.clone()).unwrap();
        history.respond(write, RegisterOutput::Written).unwrap();
        assert!(
            check(&history).is_linearizable(),
            "an overlapping read returning {observed:?} was rejected"
        );
    }
}

/// Two reads that overlap the same write must agree on an order: once one has seen the new
/// value, a later non-overlapping read cannot unsee it.
#[test]
fn a_value_cannot_be_unseen() {
    let mut history = History::new();
    let write = history.invoke(1, RegisterInput::Write(value(1)));
    let first = history.invoke(2, RegisterInput::Read);
    history.respond(first, read_of(1)).unwrap();
    history.respond(write, RegisterOutput::Written).unwrap();

    let second = history.invoke(2, RegisterInput::Read);
    history
        .respond(second, RegisterOutput::Value(None))
        .unwrap();

    assert!(
        !check(&history).is_linearizable(),
        "a read went backwards in time and was accepted"
    );
}

/// Compare-and-swap is where a register model earns its keep: exactly one of two concurrent
/// swaps from the same expected value can succeed.
#[test]
fn two_concurrent_cas_from_the_same_value_cannot_both_win() {
    let mut history = History::new();
    let seed = history.invoke(1, RegisterInput::Write(value(0)));
    history.respond(seed, RegisterOutput::Written).unwrap();

    let left = history.invoke(
        1,
        RegisterInput::Cas {
            expected: Some(value(0)),
            new: value(1),
        },
    );
    let right = history.invoke(
        2,
        RegisterInput::Cas {
            expected: Some(value(0)),
            new: value(2),
        },
    );
    history
        .respond(left, RegisterOutput::Swapped(true))
        .unwrap();
    history
        .respond(right, RegisterOutput::Swapped(true))
        .unwrap();

    assert!(
        !check(&history).is_linearizable(),
        "both compare-and-swaps from the same expected value were allowed to win"
    );
}

#[test]
fn one_cas_winning_and_one_losing_is_fine() {
    let mut history = History::new();
    let seed = history.invoke(1, RegisterInput::Write(value(0)));
    history.respond(seed, RegisterOutput::Written).unwrap();

    let left = history.invoke(
        1,
        RegisterInput::Cas {
            expected: Some(value(0)),
            new: value(1),
        },
    );
    let right = history.invoke(
        2,
        RegisterInput::Cas {
            expected: Some(value(0)),
            new: value(2),
        },
    );
    history
        .respond(left, RegisterOutput::Swapped(true))
        .unwrap();
    history
        .respond(right, RegisterOutput::Swapped(false))
        .unwrap();

    let read = history.invoke(1, RegisterInput::Read);
    history.respond(read, read_of(1)).unwrap();

    assert!(check(&history).is_linearizable());
}

/// An operation that never responded may have taken effect or not — so it must not be able to
/// rescue a history that is otherwise broken, and must not break one that is otherwise fine.
#[test]
fn a_pending_write_may_have_happened_or_not() {
    // Not linearizable on its own, and the pending write cannot explain the stale read
    // because the read that follows it saw the *older* value than an acked write.
    let mut fine = History::new();
    let pending = fine.invoke(1, RegisterInput::Write(value(9)));
    let read = fine.invoke(2, RegisterInput::Read);
    fine.respond(read, RegisterOutput::Value(None)).unwrap();
    assert_eq!(fine.pending(), 1);
    assert!(
        check(&fine).is_linearizable(),
        "a pending write must be allowed to have not happened yet"
    );
    let _ = pending;

    // The same read, but after an *acknowledged* write, is still a violation: a pending
    // operation cannot un-acknowledge one that finished.
    let mut broken = History::new();
    let acked = broken.invoke(1, RegisterInput::Write(value(9)));
    broken.respond(acked, RegisterOutput::Written).unwrap();
    let _ignored = broken.invoke(1, RegisterInput::Write(value(8)));
    let stale = broken.invoke(2, RegisterInput::Read);
    broken.respond(stale, RegisterOutput::Value(None)).unwrap();
    assert!(
        !check(&broken).is_linearizable(),
        "a pending write was used to excuse a stale read"
    );
}

/// A write whose result the client never learned still counts if a later read saw its value.
#[test]
fn an_unknown_response_takes_any_result() {
    let mut history = History::new();
    let write = history.invoke(1, RegisterInput::Write(value(3)));
    history.respond_unknown(write).unwrap();
    let read = history.invoke(2, RegisterInput::Read);
    history.respond(read, read_of(3)).unwrap();
    assert!(check(&history).is_linearizable());

    // But "unknown" is not "anything goes": the value it wrote is still the value it wrote.
    let mut broken = History::new();
    let write = broken.invoke(1, RegisterInput::Write(value(3)));
    broken.respond_unknown(write).unwrap();
    let read = broken.invoke(2, RegisterInput::Read);
    broken.respond(read, read_of(4)).unwrap();
    assert!(
        !check(&broken).is_linearizable(),
        "a read invented a value nobody wrote"
    );
}

#[test]
fn an_empty_history_is_linearizable() {
    let history: History<RegisterInput, RegisterOutput> = History::new();
    assert_eq!(
        check(&history),
        CheckOutcome::Linearizable { order: Vec::new() }
    );
}

/// The budget is what keeps an exponential search out of a test suite's critical path.
#[test]
fn an_exhausted_budget_is_reported_not_hidden() {
    let mut history = History::new();
    // Sixteen writes all in flight at once: a large permutation space, and the checker is
    // given four steps.
    let mut ids = Vec::new();
    for byte in 0..16_u8 {
        ids.push(history.invoke(u64::from(byte), RegisterInput::Write(value(byte))));
    }
    for id in ids {
        history.respond(id, RegisterOutput::Written).unwrap();
    }
    let outcome = Checker::with_budget(4).check(&Register, &history);
    assert_eq!(outcome, CheckOutcome::Inconclusive { steps: 4 });
    assert!(!outcome.is_linearizable(), "inconclusive is not a pass");
}

/// Times supplied by hand are validated, because a response recorded before its own
/// invocation would silently corrupt the event order.
#[test]
fn a_response_before_its_invocation_is_rejected() {
    let ops = vec![Op {
        client: 1,
        input: RegisterInput::Read,
        invoked: 10,
        completion: Completion::Ok {
            at: 10,
            output: RegisterOutput::Value(None),
        },
    }];
    assert_eq!(
        History::from_ops(ops),
        Err(HistoryError::ResponseNotAfterInvocation {
            op: OpId(0),
            invoked: 10,
            at: 10
        })
    );
}

#[test]
fn responding_twice_is_an_error_not_a_silent_overwrite() {
    let mut history = History::new();
    let read = history.invoke(1, RegisterInput::Read);
    history.respond(read, RegisterOutput::Value(None)).unwrap();
    assert_eq!(
        history.respond(read, read_of(1)),
        Err(HistoryError::AlreadyResponded { op: read })
    );
    assert_eq!(
        history.respond(OpId(42), RegisterOutput::Written),
        Err(HistoryError::NoSuchOp { op: OpId(42) })
    );
}

/// The checker is generic over the model, and this proves it by checking a history against a
/// second model — a monotonic counter — that has nothing to do with a register.
#[test]
fn the_checker_is_generic_over_the_model() {
    #[derive(Debug)]
    struct Counter;
    impl Model for Counter {
        type State = u64;
        type Input = ();
        type Output = u64;
        fn init(&self) -> u64 {
            0
        }
        fn apply(&self, state: &mut u64, (): &()) -> u64 {
            *state += 1;
            *state
        }
    }

    let mut good: History<(), u64> = History::new();
    let first = good.invoke(1, ());
    let second = good.invoke(2, ());
    good.respond(second, 2).unwrap();
    good.respond(first, 1).unwrap();
    assert!(Checker::new().check(&Counter, &good).is_linearizable());

    let mut bad: History<(), u64> = History::new();
    let first = bad.invoke(1, ());
    bad.respond(first, 1).unwrap();
    let second = bad.invoke(2, ());
    bad.respond(second, 1).unwrap();
    assert!(
        !Checker::new().check(&Counter, &bad).is_linearizable(),
        "the counter went back to 1"
    );
}

/// One step of a generated concurrent history.
#[derive(Debug, Clone)]
enum GenOp {
    Read,
    Write(u8),
    Cas(u8, u8),
}

fn gen_op() -> impl Strategy<Value = GenOp> {
    prop_oneof![
        3 => Just(GenOp::Read),
        3 => (0_u8..4).prop_map(GenOp::Write),
        2 => (0_u8..4, 0_u8..4).prop_map(|(expected, new)| GenOp::Cas(expected, new)),
    ]
}

/// Builds a history by *executing* the operations against the model in a chosen sequential
/// order, then recording invocations and responses in an interleaved order that still admits
/// that sequential order. Whatever comes out is linearizable by construction.
///
/// `overlap` says how many operations are in flight at once: with 1 the history is
/// sequential, with more the intervals overlap and the checker has to search.
fn legal_history(ops: &[GenOp], overlap: usize) -> History<RegisterInput, RegisterOutput> {
    let model = Register;
    let mut state = model.init();
    let mut history = History::new();
    let mut in_flight: Vec<(OpId, RegisterOutput)> = Vec::new();
    let overlap = overlap.max(1);

    for (index, op) in ops.iter().enumerate() {
        let input = match op {
            GenOp::Read => RegisterInput::Read,
            GenOp::Write(byte) => RegisterInput::Write(value(*byte)),
            GenOp::Cas(expected, new) => RegisterInput::Cas {
                expected: Some(value(*expected)),
                new: value(*new),
            },
        };
        // The linearization point is the invocation, so responses may be deferred but never
        // reordered: every deferred response is still inside its own interval.
        let id = history.invoke((index % 3) as u64, input.clone());
        let output = model.apply(&mut state, &input);
        in_flight.push((id, output));
        if in_flight.len() >= overlap {
            let (id, output) = in_flight.remove(0);
            history.respond(id, output).unwrap();
        }
    }
    for (id, output) in in_flight {
        history.respond(id, output).unwrap();
    }
    history
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Anything that a real sequential execution produced is linearizable, however much the
    /// responses overlap.
    #[test]
    fn generated_legal_histories_pass(ops in prop::collection::vec(gen_op(), 1..14), overlap in 1_usize..5) {
        let history = legal_history(&ops, overlap);
        let outcome = Checker::new().check(&Register, &history);
        prop_assert!(
            outcome.is_linearizable(),
            "a history produced by a real sequential run was rejected:\n{outcome}"
        );
    }

    /// Corrupt one read's response into a value that no operation in the history ever wrote,
    /// and the checker must notice. (Only reads are corrupted, and only to a value outside the
    /// generated alphabet, so the corruption can never accidentally still be linearizable.)
    #[test]
    fn one_corrupted_read_is_always_caught(
        ops in prop::collection::vec(gen_op(), 1..14),
        overlap in 1_usize..5,
        victim in 0_usize..14,
    ) {
        let history = legal_history(&ops, overlap);
        let reads: Vec<usize> = history
            .ops()
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op.input, RegisterInput::Read))
            .map(|(index, _)| index)
            .collect();
        prop_assume!(!reads.is_empty());
        let target = reads[victim % reads.len()];

        let mut corrupted: Vec<Op<RegisterInput, RegisterOutput>> = history.ops().to_vec();
        // 200 is outside the 0..4 alphabet the generator draws from, so no order can explain it.
        corrupted[target].completion = match corrupted[target].completion.clone() {
            Completion::Ok { at, .. } => Completion::Ok { at, output: read_of(200) },
            other => other,
        };
        let corrupted = History::from_ops(corrupted).unwrap();
        let outcome = Checker::new().check(&Register, &corrupted);
        prop_assert!(
            !outcome.is_linearizable(),
            "a read of a value nobody ever wrote was accepted:\n{}",
            corrupted.render(&[])
        );
    }
}
