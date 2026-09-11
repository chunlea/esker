//! The port allocator every real-process test starts from, and the property it exists for.
//!
//! The failure it replaces is not reproducible on demand — it needs two processes to be in the
//! same microsecond-wide window — so what is asserted here is the property that makes it
//! impossible rather than the collision itself: **two reservations that are alive at the same
//! moment do not overlap.** Against the fixed-band allocators this file replaces, the first
//! assertion below fails outright and without any timing at all: the first call returned 34,100
//! and released, and the second call scanning the same band from the same end returned 34,100
//! again.
//!
//! See `tests/port_band/mod.rs` for the four gates that paid for this.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod port_band;

use std::net::TcpListener;

/// **The red one.** Two live reservations cannot be given the same ports.
#[test]
fn two_live_reservations_never_overlap() {
    const SPAN: u16 = 4;

    let first = port_band::reserve(SPAN);
    // Deliberately *not* released: the whole point is that the second call is made while the first
    // run is still held, which is the arrangement two concurrent `nextest` processes are in.
    let second = port_band::reserve(SPAN);

    let (a, b) = (first.base(), second.base());
    assert!(
        a + SPAN <= b || b + SPAN <= a,
        "two reservations held at the same moment overlap: {a}..{} and {b}..{}",
        a + SPAN,
        b + SPAN,
    );
}

/// And the ports are real: released, every one of them binds.
///
/// The companion to the assertion above, because "never overlap" is satisfied perfectly by an
/// allocator that answers ports nothing can use.
#[test]
fn a_released_run_is_bindable_end_to_end() {
    const SPAN: u16 = 3;

    let ports = port_band::reserve(SPAN);
    let expected: Vec<u16> = (0..SPAN).map(|step| ports.at(step)).collect();
    let base = ports.into_base();
    assert_eq!(expected[0], base);

    let bound: Vec<TcpListener> = expected
        .iter()
        .filter_map(|port| TcpListener::bind(("127.0.0.1", *port)).ok())
        .collect();
    assert_eq!(
        bound.len(),
        usize::from(SPAN),
        "a released run of {SPAN} ports from {base} did not bind: {expected:?}"
    );
}

/// A run of one is a run, and it is what the single-port tests take.
#[test]
fn a_run_of_one_is_a_port() {
    let one = port_band::reserve_one();
    let port = one.at(0);
    assert_eq!(port, one.into_base());
    assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
}
