//! A cluster of [`esker_raft::RawNode`]s driven by the discrete-event simulator.
//!
//! `prompts/03-raft.md` sub-phase 3b: N nodes over the in-memory [`crate::net::SimNetwork`],
//! with partitions, message loss, duplication, reordering, crash and restart, and slow disks —
//! all drawn from one seeded generator so that a failure is a number a test prints and a
//! rerun reproduces exactly.
//!
//! # What lives here
//!
//! * [`checkers`] — the four safety properties, checked after every event.
//!
//! # Invariants
//!
//! * **One random stream owns the run.** The event loop draws from it; per-node streams are
//!   derived from `(seed, node id)`. No `HashMap` iteration decides anything.
//! * **The driver follows the `Ready` contract**: a node's `hard_state` and entries are made
//!   durable *before* the messages from the same `Ready` are handed to the network. That order
//!   is Raft's safety argument, so the driver can be told to break it on purpose — and the
//!   checkers have to notice.
//! * **The persistence boundary is honest.** A restarted node gets exactly what the driver had
//!   made durable, and nothing else. Get that wrong and every safety property passes
//!   vacuously.

pub mod checkers;

pub use checkers::{EntryDigest, NodeSnapshot, SafetyChecker, Violation};
