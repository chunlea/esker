//! Test harnesses that are part of the crate rather than of one test binary.
//!
//! `docs/DESIGN.md` §11 gives the engine a crash-test obligation — "subprocess `kill -9` loop
//! under load; recovery must equal model", "with fault injection into `FileSystem` (partial
//! writes, failed fsync, failed rename)". The machinery for that has to live in the library,
//! because more than one test binary needs it and because a subprocess crash test needs it
//! inside the child.
//!
//! Everything here is behind `#[cfg(any(test, feature = "testing"))]`, so it is compiled for
//! this crate's own tests and for anyone who asks for it by name, and is absent from a normal
//! build. To reach it from another crate or from an integration test:
//!
//! ```toml
//! [dev-dependencies]
//! esker-engine = { path = "../esker-engine", features = ["testing"] }
//! ```
//!
//! [`MemFileSystem`](crate::memfs::MemFileSystem) is deliberately *not* here: it is a plain
//! implementation of a public trait that the simulator will want in a normal build, and only
//! the deliberate misbehaviour belongs behind a feature.

pub mod fault_fs;
pub mod pause;
pub mod plan;

pub use fault_fs::FaultFileSystem;
pub use pause::{PauseHook, PausePoint};
pub use plan::{Fault, FaultPlan, FaultRecord, Operation};
