//! The deterministic simulator: a logical clock, an injected seeded random number generator,
//! a `Network` the rest of the system talks to — implemented once in memory with fault
//! injection and once, later, by the real TCP transport — and the history checkers that decide
//! whether what came out of a run was allowed to. Every run is a function of its seed, so
//! every failure is reproducible from the one number the test prints (`docs/DESIGN.md` §11).
//!
//! # Invariants
//!
//! * **Determinism is the product.** No `Instant`, no OS entropy, no `HashMap` iteration in
//!   any code that affects ordering. The event queue is keyed so that ties have a total
//!   order, because a tie broken differently on two runs is a lost bug.
//! * **The same seed replays exactly.** Two runs of the same scenario with the same seed
//!   produce identical event traces, on every platform.
//! * **Faults are declared, not incidental.** Drops, delays, duplicates and reordering come
//!   from an explicit [`FaultPlan`], so a scenario says what it is testing.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod clock;
pub mod fault;
pub mod lin;
pub mod mech;
pub mod net;
pub mod raft;

pub use clock::{Clock, Millis};
pub use fault::FaultPlan;
pub use lin::{CheckOutcome, Checker, History, Register, RegisterInput, RegisterOutput};
pub use net::{
    Envelope, Network, NetworkError, NodeId, NodeView, SimNetwork, TraceEvent, scenario_rng,
};
