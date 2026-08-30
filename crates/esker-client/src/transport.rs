//! The seam between the client and the network.
//!
//! Everything above this trait — routing, retries, backoff, deadlines — is ordinary
//! synchronous code with no sockets and no clock of its own, which is what makes it testable
//! against [`crate::testing::FakeTransport`] rather than against a server. `tokio` lives
//! *inside* an implementation of this trait, at the network edge and nowhere else
//! (`CLAUDE.md`, "Toolchain and conventions").
//!
//! # Why the call is synchronous
//!
//! The same reason `esker-raft` is a pure state machine: a core with no I/O in it can be
//! driven by a test. A blocking `call` also makes the CLI and the benchmark driver — both of
//! which are threads, not futures — trivial, and it costs the real transport only a channel
//! round trip to its runtime.
//!
//! # Addressing
//!
//! A call names a **store id**, not an address. The client routes by region and peer, and
//! `Peer` carries a store id; turning that into a socket address is the placement driver's
//! job. Until PD exists the mapping is a one-entry table inside the transport
//! (`// TODO(phase-4)`).

// TODO(phase-2): `Transport` belongs to `esker-proto`, whose single writer is the sibling
// lane. When it lands, this module keeps only the doc comment and re-exports the real trait.

use std::fmt;
use std::time::Instant;

use crate::wire::{CallResult, Request};

/// One round trip to one store.
pub trait Transport: fmt::Debug + Send + Sync {
    /// Sends `request` to `store_id` and waits for its answer, giving up at `deadline`.
    ///
    /// Returning `Err` must say which side of the ambiguity the failure is on: an
    /// implementation that cannot prove the request never left is required to report
    /// [`crate::wire::TransportError::Ambiguous`], because the caller turns that into a
    /// typed refusal to retry a write. Reporting `NotSent` for a request that may have
    /// arrived is the one way this trait can be implemented wrongly and lose data.
    fn call(&self, store_id: u64, request: &Request, deadline: Instant) -> CallResult;

    /// Largest frame this transport will carry, in bytes.
    ///
    /// The client checks a request against it before sending, so an oversized batch is a
    /// typed error at the call site rather than a connection torn down mid-stream.
    fn max_frame_size(&self) -> usize {
        esker_proto::MAX_FRAME_SIZE
    }
}
