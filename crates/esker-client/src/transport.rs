//! The seam between the client and a store.
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
//! `Peer` carries a store id; turning that into a socket address is the placement driver's job.
//! `TODO(debt-c6 #4)`: it is still not PD that does it — [`crate::tcp::TcpStores`] is handed a
//! fixed list of addresses at construction and learns no store it was not given, so a store added
//! to the cluster is unreachable until the client is rebuilt (`docs/plans/debt-c6.md` §4).

//! # Not `esker_proto::transport::Transport`
//!
//! That trait is the *connection*: async, one peer, `Request` in and `Response` out. This one
//! is the *routing* layer above it: it addresses a **store id**, answers with a `RawKvResp`
//! already unwrapped, and is synchronous. [`crate::tcp::TcpStores`] is the adapter between
//! them, and it is the only place in this crate that knows a socket exists.

use std::fmt;
use std::time::Instant;

use crate::wire::{CallResult, Request};

/// One round trip to one store.
pub trait StoreTransport: fmt::Debug + Send + Sync {
    /// Sends `request` to `store_id` and waits for its answer, giving up at `deadline`.
    ///
    /// Returning `Err` must say which side of the ambiguity the failure is on, through
    /// [`esker_proto::ProtoError::outcome`]: an implementation that cannot prove the request
    /// never left must report an error whose outcome is `Unknown`, because the caller turns
    /// that into a typed refusal to retry a write. Answering `NotSent` for a request that may
    /// have arrived is the one way this trait can be implemented wrongly and lose data.
    fn call(&self, store_id: u64, request: &Request, deadline: Instant) -> CallResult;

    /// Largest frame this transport will carry, in bytes.
    ///
    /// The client checks a request against it before sending, so an oversized batch is a
    /// typed error at the call site rather than a connection torn down mid-stream.
    fn max_frame_size(&self) -> usize {
        esker_proto::MAX_FRAME_SIZE
    }
}
