//! Models of the mechanisms this project's fixes pinned, one module each.
//!
//! # Why these exist
//!
//! Every fix in `docs/plans/debt-c3.md` and `debt-c4.md` landed with a test that holds **one
//! ordering** still — the ordering somebody found by hand, which is what took the day. That test
//! is the right thing to have: it is fast, it names the bug, and it fails for one reason. What it
//! cannot do is say anything about the orderings nobody constructed.
//!
//! These modules are the other half. Each is a deterministic model of one mechanism, and each is
//! **generic over the decision under test** rather than containing it. That is the whole design:
//! a model that transcribes the fix and then checks its own transcription passes at every
//! revision, the buggy one included, and is therefore decoration.
//!
//! # The seam
//!
//! Each module defines
//!
//! * plain data types for the world the decision is taken in — no dependency on the crate that
//!   takes it, so `esker-sim` stays below every layer it models;
//! * a **policy trait**, which is the decision;
//! * a **checker**, which states the invariant in terms of the model's own state and never in
//!   terms of what the policy answered.
//!
//! The real proof lives in the crate that owns the code: `crates/esker-pd/tests/sim_balance.rs`,
//! `crates/esker-client/tests/sim_retry.rs` and the two in `crates/esker-store/tests/` implement
//! the policy by calling the real function. Copy the model into a detached worktree at a fix's
//! parent, and the same checker meets the code as it was.
//!
//! Each module also keeps a **reference** implementation so that `esker-sim`'s own tests can show
//! the model reaches the states it claims to reach. A reference implementation is never a proof
//! of anything about the real code.
//!
//! # Invariants
//!
//! * **The checker owns the ground truth.** A model that asks the policy what the world looks
//!   like cannot catch the policy being wrong about it. Where the model creates a condition — it
//!   is the thing that adds the learner, that takes the store down — it remembers that it did,
//!   and the checker reads its own memory.
//! * **No wall clock, no sockets, no sleeping.** Time is a counter the model advances.
//! * **Every failure is a seed.** A violation carries the seed and the round it happened in.

pub mod placement;
pub mod reference;
pub mod retry;
pub mod sweep;

pub use placement::{BalancePolicy, ClusterView, MidRepair, Move, PeerView, RegionView, StoreView};
pub use reference::ReferenceBalance;
pub use retry::{Answer, Budget, RetryClient, Script};
pub use sweep::{Case, Expected, Observed, PdAnswer, RegionSpan};
