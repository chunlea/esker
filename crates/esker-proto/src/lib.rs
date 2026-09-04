//! The wire protocol: hand-rolled length-prefixed frames over one TCP connection, carrying
//! multiplexed requests, responses and streams for `RawKV`, `TxnKV`, the placement driver and
//! Raft transport. There is no `tonic`, no `prost` and no `serde` — every message has an
//! `encode`/`decode` pair written by hand and pinned by golden tests (`docs/DESIGN.md` §9,
//! `docs/adr/0002-formats-are-hand-rolled.md`).
//!
//! # Invariants
//!
//! * **Every frame is checksummed** with CRC32C, like every byte on disk
//!   (`CLAUDE.md` invariant 2). The checksum covers the kind and the request id as well as the
//!   body, because a flipped request id would otherwise deliver a response to the wrong
//!   caller with everything else intact.
//! * **Unknown methods and unknown fields are errors, never ignored.** Compatibility is
//!   negotiated once, through [`WIRE_VERSION`] on connect, rather than guessed per message.
//!   Trailing bytes after a message decodes are an error for the same reason.
//! * **Every key-value request carries `{ region_id, epoch, peer }`** so the server can reject
//!   a stale epoch with a redirect hint (invariant 5).
//! * **Byte-opaque.** Keys and values cross the wire as byte strings; this crate never
//!   interprets them, and the `'r'` namespace of `docs/DESIGN.md` §3 is added by the store,
//!   never by a client (invariant 7).
//! * **Nothing here is unbounded.** Frames have a maximum size, requests in flight have a
//!   limit, and every channel is a bounded one. A server at its limit answers
//!   [`ProtoError::ServerIsBusy`]; it does not queue until it dies.
//!
//! # Module map
//!
//! | Module | What it decides |
//! |---|---|
//! | [`error`] | the typed error, its wire codes, and whether a failed request may have applied |
//! | [`codec`] | how a field becomes bytes: varints, length prefixes, and canonical encodings |
//! | [`frame`] | the envelope: length, checksum, kind, request id |
//! | [`region`] | regions, epochs and peers — what a request is addressed to |
//! | [`messages`] | one `encode`/`decode` pair per message, and the method numbers |
//! | [`pd`] | service `0x03`: the placement driver's six methods, and the caller's channel |
//! | [`raft`] | service `0x04`: `esker_raft::Message` on the wire, with its routing |
//! | [`transport`] | tokio TCP: the writer task, the demultiplexer, keepalive and streams |

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod codec;
pub mod error;
pub mod fragment;
pub mod frame;
pub mod messages;
pub mod pd;
pub mod raft;
pub mod region;
pub mod schema;
pub mod transport;
pub mod txn;

pub use codec::{DecodeError, Decoder, Encoder};
pub use error::{ProtoError, RequestOutcome};
pub use frame::{FRAME_HEADER_SIZE, Frame, FrameDecoder, FrameKind, MAX_BODY_SIZE, MAX_FRAME_SIZE};
pub use messages::{
    AdminReq, AdminResp, Hello, HelloAck, Method, RawKvReq, RawKvResp, RegionStatus, Request,
    RequestHeader, Response, SnapshotRequest,
};
pub use pd::{
    Operator, OperatorProgress, OperatorStatus, PdChannel, PdMemberInfo, PdMembership, PdRaftBatch,
    PdReq, PdResp, ScannedRegion, StoreInfo,
};
pub use raft::{RaftBatch, RaftMessage};
pub use region::{Epoch, Peer, PeerRole, Region};
pub use transport::{
    BlockingTransport, BoxFuture, ChunkSender, ChunkStream, Reply, Server, ServerHandle, Service,
    StreamResponse, TcpTransport, Transport, TransportConfig,
};
pub use txn::{LockInfo, TxnKvReq, TxnKvResp, TxnMutation, TxnStatus};

/// Version of the framing and of every message encoding. Negotiated when a connection opens;
/// a mismatch is a hard error, not a downgrade (`docs/DESIGN.md` §9).
pub const WIRE_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::{FRAME_HEADER_SIZE, MAX_FRAME_SIZE, WIRE_VERSION};

    /// The header layout is fixed; if this drifts, every peer on the old version is unable to
    /// find the body.
    #[test]
    fn frame_header_layout_is_frozen() {
        assert_eq!(FRAME_HEADER_SIZE, 17);
        assert_eq!(WIRE_VERSION, 1);
    }

    /// A snapshot chunk plus its framing must fit in one frame, otherwise the store cannot
    /// send the chunk size it is configured for.
    #[test]
    fn max_frame_holds_a_snapshot_chunk() {
        assert!(MAX_FRAME_SIZE > esker_store_snapshot_chunk() + FRAME_HEADER_SIZE);
    }

    /// The value from `esker-store`, restated rather than imported: `esker-proto` sits below
    /// the store and must not depend on it.
    const fn esker_store_snapshot_chunk() -> usize {
        1024 * 1024
    }
}
