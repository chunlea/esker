//! Primitives that Esker implements itself instead of taking from crates.io.
//!
//! Every layer above needs a checksum, a variable-length integer, a cheap hash and a
//! reproducible random number generator. `CLAUDE.md` forbids buying any of them: they are
//! small, they are part of what this project is for, and each one is on the ban list
//! (`crc32c`, `crc32fast`, `rand`, `lru`). They live in their own crate rather than in
//! `esker-keys` so that `esker-engine` can use them without depending on key semantics,
//! which invariant 7 forbids.
//!
//! # Invariants
//!
//! * Nothing here allocates on a decode path unless the caller asked for an owned value,
//!   and nothing here panics on adversarial input — malformed bytes come back as an error
//!   (`CLAUDE.md` invariant 9).
//! * [`crc32c`] and [`rng`] match published reference vectors, not just themselves. A
//!   self-consistent property test will happily bless a wrong algorithm.
//! * [`rng::Pcg32`] is the only source of randomness in the project. No component may reach
//!   for OS entropy or a thread-local generator: determinism is what makes the simulator and
//!   the model checker useful (`docs/DESIGN.md` §5, §11).
//! * Every byte layout here is part of an on-disk or on-wire format. Changing one is a
//!   format change and needs an ADR plus a new format version.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod crc32c;
pub mod hash;
pub mod rng;
pub mod varint;
