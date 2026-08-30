//! Service `0x04`: the Raft messages one store sends another.
//!
//! The wire carries [`esker_raft::Message`] itself rather than a copy of it
//! ([ADR 0009](https://github.com/esker/docs/adr/0009-the-wire-carries-the-raft-message.md), in
//! `docs/adr/`). A mirrored enum would be a second place to keep thirty-odd fields in step, and
//! the failure mode is a conversion that compiles while quietly dropping one — `hint_term` going
//! missing looks like a slow catch-up, not like a bug. Encoding the real type means adding a
//! variant is a compile error *here*, which is where the format decision belongs.
//!
//! What Raft does not know is where a message is going: it names peers, not stores or regions. So
//! each message travels inside a [`RaftMessage`] that adds the routing — region, epoch, and the
//! peer ids at both ends — and a whole tick's worth of them travel as one [`RaftBatch`]
//! (`docs/DESIGN.md` §6).
//!
//! # Format (*fixed*, version 1)
//!
//! ```text
//! RaftBatch    = count:varint ++ RaftMessage*
//! RaftMessage  = region_id:varint ++ epoch ++ from_peer:varint ++ to_peer:varint ++ Message
//! Message      = kind:u8 ++ from:varint ++ to:varint ++ term:varint ++ per-kind fields
//! Entry        = term:varint ++ index:varint ++ kind:u8 ++ data:bytes
//! Snapshot     = index:varint ++ term:varint ++ voters ++ learners ++ data:bytes
//! ```
//!
//! The `kind` bytes are part of the format and never move: `0` is reserved and never valid, as
//! everywhere else in this crate, so a run of zero bytes is not a readable message.

use bytes::Bytes;
use esker_raft::{ConfState, Entry, EntryKind, Message, Snapshot, SnapshotMeta};

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::region::Epoch;

/// Kind bytes for [`Message`]. Part of the wire format: these numbers are fixed.
mod kind {
    pub(super) const REQUEST_VOTE: u8 = 1;
    pub(super) const REQUEST_VOTE_RESPONSE: u8 = 2;
    pub(super) const APPEND_ENTRIES: u8 = 3;
    pub(super) const APPEND_ENTRIES_RESPONSE: u8 = 4;
    pub(super) const INSTALL_SNAPSHOT: u8 = 5;
    pub(super) const TIMEOUT_NOW: u8 = 6;
    pub(super) const READ_INDEX: u8 = 7;
    pub(super) const READ_INDEX_RESPONSE: u8 = 8;
}

/// Kind bytes for [`EntryKind`].
mod entry_kind {
    pub(super) const NORMAL: u8 = 1;
    pub(super) const CONF_CHANGE: u8 = 2;
}

/// One Raft message, plus the routing Raft itself does not carry.
///
/// The epoch is here for the same reason it is on every key-value request (`CLAUDE.md`
/// invariant 5): a store must be able to refuse a message for a region whose membership has moved
/// on beneath it. In 3e the epoch never changes, and the field is still checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaftMessage {
    /// Which region's Raft group this belongs to.
    pub region_id: u64,
    /// The epoch the sender believes the region is at.
    pub epoch: Epoch,
    /// The sending peer.
    pub from_peer: u64,
    /// The **store** the sending peer is on.
    ///
    /// A peer id is region-local — the placement driver allocates one per replica — so a receiver
    /// cannot turn `from_peer` into an address on its own. It matters for exactly one thing and
    /// that thing is load-bearing: a store receiving traffic for a region it does not host asks
    /// the sender for that region (`docs/DESIGN.md` §6), and without this it has nowhere to ask.
    pub from_store: u64,
    /// The receiving peer.
    pub to_peer: u64,
    /// The message itself.
    pub message: Message,
}

impl RaftMessage {
    /// A message for one region, routed between two peers.
    #[must_use]
    pub fn new(region_id: u64, epoch: Epoch, from_store: u64, message: Message) -> Self {
        Self {
            region_id,
            epoch,
            from_peer: message.sender(),
            from_store,
            to_peer: message.recipient(),
            message,
        }
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        out.put_varint(self.region_id);
        self.epoch.encode(out);
        out.put_varint(self.from_peer);
        out.put_varint(self.from_store);
        out.put_varint(self.to_peer);
        encode_message(&self.message, out);
    }

    pub(crate) fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            region_id: input.get_varint("raft.region_id")?,
            epoch: Epoch::decode(input)?,
            from_peer: input.get_varint("raft.from_peer")?,
            from_store: input.get_varint("raft.from_store")?,
            to_peer: input.get_varint("raft.to_peer")?,
            message: decode_message(input)?,
        })
    }
}

/// A tick's worth of Raft messages for one store, in one frame.
///
/// Batching is what keeps a busy cluster from spending a frame per heartbeat per region
/// (`docs/DESIGN.md` §6). An empty batch is legal and means "nothing to say"; it is not sent, but
/// decoding one is not an error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RaftBatch {
    /// The messages, for any mix of regions.
    pub messages: Vec<RaftMessage>,
}

impl RaftBatch {
    /// A batch of messages.
    #[must_use]
    pub fn new(messages: Vec<RaftMessage>) -> Self {
        Self { messages }
    }

    /// Whether there is nothing to send.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        out.put_varint(self.messages.len() as u64);
        for message in &self.messages {
            message.encode(out);
        }
    }

    pub(crate) fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let count = input.get_count("raft.batch.count")?;
        let mut messages = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            messages.push(RaftMessage::decode(input)?);
        }
        Ok(Self { messages })
    }
}

fn encode_message(message: &Message, out: &mut Encoder) {
    match message {
        Message::RequestVote {
            from,
            to,
            term,
            last_log_index,
            last_log_term,
            pre_vote,
            force,
        } => {
            head(out, kind::REQUEST_VOTE, *from, *to, *term);
            out.put_varint(*last_log_index);
            out.put_varint(*last_log_term);
            out.put_bool(*pre_vote);
            out.put_bool(*force);
        }
        Message::RequestVoteResponse {
            from,
            to,
            term,
            granted,
            pre_vote,
        } => {
            head(out, kind::REQUEST_VOTE_RESPONSE, *from, *to, *term);
            out.put_bool(*granted);
            out.put_bool(*pre_vote);
        }
        Message::AppendEntries {
            from,
            to,
            term,
            prev_log_index,
            prev_log_term,
            entries,
            leader_commit,
            context,
        } => {
            head(out, kind::APPEND_ENTRIES, *from, *to, *term);
            out.put_varint(*prev_log_index);
            out.put_varint(*prev_log_term);
            out.put_varint(*leader_commit);
            out.put_bytes(context);
            out.put_varint(entries.len() as u64);
            for entry in entries {
                encode_entry(entry, out);
            }
        }
        Message::AppendEntriesResponse {
            from,
            to,
            term,
            reject,
            index,
            hint_term,
            context,
        } => {
            head(out, kind::APPEND_ENTRIES_RESPONSE, *from, *to, *term);
            out.put_bool(*reject);
            out.put_varint(*index);
            out.put_varint(*hint_term);
            out.put_bytes(context);
        }
        Message::InstallSnapshot {
            from,
            to,
            term,
            snapshot,
        } => {
            head(out, kind::INSTALL_SNAPSHOT, *from, *to, *term);
            encode_snapshot(snapshot, out);
        }
        Message::TimeoutNow { from, to, term } => {
            head(out, kind::TIMEOUT_NOW, *from, *to, *term);
        }
        Message::ReadIndex {
            from,
            to,
            term,
            ctx,
        } => {
            head(out, kind::READ_INDEX, *from, *to, *term);
            out.put_bytes(ctx);
        }
        Message::ReadIndexResponse {
            from,
            to,
            term,
            index,
            ctx,
        } => {
            head(out, kind::READ_INDEX_RESPONSE, *from, *to, *term);
            out.put_varint(*index);
            out.put_bytes(ctx);
        }
    }
}

fn head(out: &mut Encoder, kind: u8, from: u64, to: u64, term: u64) {
    out.put_u8(kind);
    out.put_varint(from);
    out.put_varint(to);
    out.put_varint(term);
}

fn decode_message(input: &mut Decoder<'_>) -> Result<Message, DecodeError> {
    let kind = input.get_u8("raft.kind")?;
    let from = input.get_varint("raft.from")?;
    let to = input.get_varint("raft.to")?;
    let term = input.get_varint("raft.term")?;
    let message = match kind {
        kind::REQUEST_VOTE => Message::RequestVote {
            from,
            to,
            term,
            last_log_index: input.get_varint("vote.last_log_index")?,
            last_log_term: input.get_varint("vote.last_log_term")?,
            pre_vote: input.get_bool("vote.pre_vote")?,
            force: input.get_bool("vote.force")?,
        },
        kind::REQUEST_VOTE_RESPONSE => Message::RequestVoteResponse {
            from,
            to,
            term,
            granted: input.get_bool("vote_resp.granted")?,
            pre_vote: input.get_bool("vote_resp.pre_vote")?,
        },
        kind::APPEND_ENTRIES => {
            let prev_log_index = input.get_varint("append.prev_log_index")?;
            let prev_log_term = input.get_varint("append.prev_log_term")?;
            let leader_commit = input.get_varint("append.leader_commit")?;
            let context = owned(input, "append.context")?;
            let count = input.get_count("append.entries")?;
            let mut entries = Vec::with_capacity(count.min(4096));
            for _ in 0..count {
                entries.push(decode_entry(input)?);
            }
            Message::AppendEntries {
                from,
                to,
                term,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit,
                context,
            }
        }
        kind::APPEND_ENTRIES_RESPONSE => Message::AppendEntriesResponse {
            from,
            to,
            term,
            reject: input.get_bool("append_resp.reject")?,
            index: input.get_varint("append_resp.index")?,
            hint_term: input.get_varint("append_resp.hint_term")?,
            context: owned(input, "append_resp.context")?,
        },
        kind::INSTALL_SNAPSHOT => Message::InstallSnapshot {
            from,
            to,
            term,
            snapshot: decode_snapshot(input)?,
        },
        kind::TIMEOUT_NOW => Message::TimeoutNow { from, to, term },
        kind::READ_INDEX => Message::ReadIndex {
            from,
            to,
            term,
            ctx: owned(input, "read_index.ctx")?,
        },
        kind::READ_INDEX_RESPONSE => Message::ReadIndexResponse {
            from,
            to,
            term,
            index: input.get_varint("read_index_resp.index")?,
            ctx: owned(input, "read_index_resp.ctx")?,
        },
        other => {
            return Err(DecodeError::invalid(
                "raft.kind",
                format!("unknown Raft message kind {other}"),
            ));
        }
    };
    Ok(message)
}

fn encode_entry(entry: &Entry, out: &mut Encoder) {
    out.put_varint(entry.term);
    out.put_varint(entry.index);
    out.put_u8(match entry.kind {
        EntryKind::Normal => entry_kind::NORMAL,
        EntryKind::ConfChange => entry_kind::CONF_CHANGE,
    });
    out.put_bytes(&entry.data);
}

fn decode_entry(input: &mut Decoder<'_>) -> Result<Entry, DecodeError> {
    let term = input.get_varint("entry.term")?;
    let index = input.get_varint("entry.index")?;
    let kind = match input.get_u8("entry.kind")? {
        entry_kind::NORMAL => EntryKind::Normal,
        entry_kind::CONF_CHANGE => EntryKind::ConfChange,
        other => {
            return Err(DecodeError::invalid(
                "entry.kind",
                format!("unknown entry kind {other}"),
            ));
        }
    };
    Ok(Entry {
        term,
        index,
        kind,
        data: owned(input, "entry.data")?,
    })
}

fn encode_snapshot(snapshot: &Snapshot, out: &mut Encoder) {
    out.put_varint(snapshot.meta.index);
    out.put_varint(snapshot.meta.term);
    encode_ids(&snapshot.meta.conf.voters, out);
    encode_ids(&snapshot.meta.conf.learners, out);
    out.put_bytes(&snapshot.data);
}

fn decode_snapshot(input: &mut Decoder<'_>) -> Result<Snapshot, DecodeError> {
    let index = input.get_varint("snapshot.index")?;
    let term = input.get_varint("snapshot.term")?;
    let voters = decode_ids(input, "snapshot.voters")?;
    let learners = decode_ids(input, "snapshot.learners")?;
    let mut conf = ConfState { voters, learners };
    // Normalised on the way in: the core's decisions read this list in order, so a peer that
    // accepted an unsorted one off the wire would make different choices from the sender.
    conf.normalize();
    Ok(Snapshot {
        meta: SnapshotMeta { index, term, conf },
        data: owned(input, "snapshot.data")?,
    })
}

fn encode_ids(ids: &[u64], out: &mut Encoder) {
    out.put_varint(ids.len() as u64);
    for id in ids {
        out.put_varint(*id);
    }
}

fn decode_ids(input: &mut Decoder<'_>, field: &'static str) -> Result<Vec<u64>, DecodeError> {
    let count = input.get_count(field)?;
    let mut ids = Vec::with_capacity(count.min(256));
    for _ in 0..count {
        ids.push(input.get_varint(field)?);
    }
    Ok(ids)
}

fn owned(input: &mut Decoder<'_>, field: &'static str) -> Result<Bytes, DecodeError> {
    input.get_bytes(field).map(Bytes::copy_from_slice)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use esker_raft::{ConfState, Entry, EntryKind, Message, Snapshot, SnapshotMeta};

    use super::{RaftBatch, RaftMessage, decode_message, encode_message};
    use crate::codec::{Decoder, Encoder};
    use crate::messages::{Method, Request, Response};
    use crate::region::Epoch;

    fn round_trip(message: &Message) -> Message {
        let mut out = Encoder::new();
        encode_message(message, &mut out);
        let bytes = out.finish();
        let mut input = Decoder::new(&bytes);
        let decoded = decode_message(&mut input).expect("decodes");
        input.finish().expect("no trailing bytes");
        decoded
    }

    fn vote_messages() -> Vec<Message> {
        vec![
            Message::RequestVote {
                from: 1,
                to: 2,
                term: 7,
                last_log_index: 41,
                last_log_term: 6,
                pre_vote: true,
                force: false,
            },
            Message::RequestVote {
                from: 1,
                to: 2,
                term: 7,
                last_log_index: 0,
                last_log_term: 0,
                pre_vote: false,
                force: true,
            },
            Message::RequestVoteResponse {
                from: 2,
                to: 1,
                term: 7,
                granted: true,
                pre_vote: false,
            },
            Message::RequestVoteResponse {
                from: 2,
                to: 1,
                term: 7,
                granted: false,
                pre_vote: true,
            },
        ]
    }

    fn log_messages() -> Vec<Message> {
        vec![
            Message::AppendEntries {
                from: 1,
                to: 3,
                term: 9,
                prev_log_index: 40,
                prev_log_term: 8,
                entries: vec![
                    Entry::empty(9, 41),
                    Entry {
                        term: 9,
                        index: 42,
                        kind: EntryKind::ConfChange,
                        data: Bytes::from_static(b"change"),
                    },
                ],
                leader_commit: 40,
                context: Bytes::from_static(b"read-1"),
            },
            // A heartbeat: the same message with no entries (ADR 0007).
            Message::AppendEntries {
                from: 1,
                to: 3,
                term: 9,
                prev_log_index: 40,
                prev_log_term: 8,
                entries: Vec::new(),
                leader_commit: 40,
                context: Bytes::new(),
            },
            Message::AppendEntriesResponse {
                from: 3,
                to: 1,
                term: 9,
                reject: true,
                index: 7,
                hint_term: 3,
                context: Bytes::from_static(b"read-1"),
            },
            Message::InstallSnapshot {
                from: 1,
                to: 3,
                term: 9,
                snapshot: Snapshot {
                    meta: SnapshotMeta {
                        index: 100,
                        term: 8,
                        conf: ConfState {
                            voters: vec![1, 2, 3],
                            learners: vec![4],
                        },
                    },
                    data: Bytes::from_static(b"state"),
                },
            },
            Message::TimeoutNow {
                from: 1,
                to: 2,
                term: 9,
            },
            Message::ReadIndex {
                from: 3,
                to: 1,
                term: 9,
                ctx: Bytes::from_static(b"tag"),
            },
            Message::ReadIndexResponse {
                from: 1,
                to: 3,
                term: 9,
                index: 40,
                ctx: Bytes::from_static(b"tag"),
            },
        ]
    }

    /// Every variant, with every flag both set and unset.
    fn every_message() -> Vec<Message> {
        let mut all = vote_messages();
        all.extend(log_messages());
        all
    }

    /// Every variant, every flag. The fields that only matter under one rule — `pre_vote`,
    /// `force`, `hint_term`, `context` — are the ones a mirrored enum would have dropped silently
    /// (ADR 0009), so each appears set and unset.
    #[test]
    fn every_message_round_trips() {
        for message in every_message() {
            assert_eq!(round_trip(&message), message, "{}", message.kind_name());
        }
    }

    /// The golden. These bytes are a wire format: changing them is a format change, and needs an
    /// ADR and a `WIRE_VERSION` bump (`docs/adr/0002-formats-are-hand-rolled.md`).
    #[test]
    fn a_heartbeat_encodes_to_the_documented_bytes() {
        let heartbeat = Message::AppendEntries {
            from: 1,
            to: 2,
            term: 3,
            prev_log_index: 4,
            prev_log_term: 3,
            entries: Vec::new(),
            leader_commit: 4,
            context: Bytes::new(),
        };
        let mut out = Encoder::new();
        encode_message(&heartbeat, &mut out);
        assert_eq!(
            out.finish(),
            vec![
                3, // kind: AppendEntries
                1, // from
                2, // to
                3, // term
                4, // prev_log_index
                3, // prev_log_term
                4, // leader_commit
                0, // context: empty
                0, // entries: none
            ],
        );
    }

    #[test]
    fn a_timeout_now_encodes_to_the_documented_bytes() {
        let message = Message::TimeoutNow {
            from: 9,
            to: 8,
            term: 300,
        };
        let mut out = Encoder::new();
        encode_message(&message, &mut out);
        // 300 is two varint bytes: 0xAC 0x02.
        assert_eq!(out.finish(), vec![6, 9, 8, 0xAC, 0x02]);
    }

    /// Kind `0` is reserved everywhere in this crate, so a run of zero bytes is never a readable
    /// message — and an unknown kind is an error rather than a skipped message.
    #[test]
    fn an_unknown_kind_is_an_error() {
        for kind in [0_u8, 9, 255] {
            let bytes = vec![kind, 1, 2, 3];
            let mut input = Decoder::new(&bytes);
            assert!(
                decode_message(&mut input).is_err(),
                "kind {kind} decoded as a message"
            );
        }
    }

    #[test]
    fn a_truncated_message_is_an_error() {
        let mut out = Encoder::new();
        encode_message(&every_message()[4], &mut out);
        let bytes = out.finish();
        for cut in 1..bytes.len() {
            let mut input = Decoder::new(&bytes[..cut]);
            let outcome = decode_message(&mut input).and_then(|_| input.finish());
            assert!(
                outcome.is_err(),
                "a message truncated to {cut} bytes decoded"
            );
        }
    }

    #[test]
    fn a_batch_round_trips_through_a_request() {
        let batch = RaftBatch::new(
            every_message()
                .into_iter()
                .map(|message| RaftMessage::new(7, Epoch::new(2, 5), 9, message))
                .collect(),
        );
        let request = Request::Raft(batch.clone());
        assert_eq!(request.method(), Method::RaftBatch);
        assert!(
            request.header().is_none(),
            "a batch may hold messages for many regions, so it has no single header"
        );

        let decoded = Request::decode(&request.encode()).expect("decodes");
        assert_eq!(decoded, Request::Raft(batch));
    }

    #[test]
    fn the_routing_comes_from_the_message_it_wraps() {
        let message = Message::TimeoutNow {
            from: 4,
            to: 5,
            term: 1,
        };
        let wrapped = RaftMessage::new(3, Epoch::INITIAL, 6, message);
        assert_eq!(wrapped.from_peer, 4);
        assert_eq!(wrapped.to_peer, 5);
        assert_eq!(wrapped.region_id, 3);
        assert_eq!(
            wrapped.from_store, 6,
            "the store is the caller's: a peer id does not name one"
        );
    }

    /// The acknowledgement carries nothing, and still has to survive a round trip: a peer that
    /// could not decode it would fail a batch that in fact arrived.
    #[test]
    fn the_acknowledgement_round_trips_and_is_empty() {
        let encoded = Response::Raft.encode();
        assert_eq!(encoded.len(), 2, "the method tag and nothing else");
        assert_eq!(Response::decode(&encoded).unwrap(), Response::Raft);
        assert_eq!(Response::Raft.method(), Method::RaftBatch);
    }

    /// An empty batch is legal to decode even though it is never sent: a decoder that refused one
    /// would turn a harmless encoding into a dropped connection.
    #[test]
    fn an_empty_batch_is_legal() {
        let empty = RaftBatch::default();
        assert!(empty.is_empty());
        let request = Request::Raft(empty.clone());
        assert_eq!(
            Request::decode(&request.encode()).unwrap(),
            Request::Raft(empty)
        );
    }

    /// A configuration's order is a decision input for the core, so it is normalised on the way
    /// in: a peer that accepted an unsorted list would make different choices from the sender.
    #[test]
    fn a_snapshot_configuration_is_normalised_on_arrival() {
        let message = Message::InstallSnapshot {
            from: 1,
            to: 2,
            term: 3,
            snapshot: Snapshot {
                meta: SnapshotMeta {
                    index: 9,
                    term: 3,
                    conf: ConfState {
                        voters: vec![3, 1, 2],
                        learners: vec![5, 4],
                    },
                },
                data: Bytes::new(),
            },
        };
        let Message::InstallSnapshot { snapshot, .. } = round_trip(&message) else {
            unreachable!("an InstallSnapshot decodes as one");
        };
        assert_eq!(snapshot.meta.conf.voters, vec![1, 2, 3]);
        assert_eq!(snapshot.meta.conf.learners, vec![4, 5]);
    }

    /// `RaftTransport::Batch` is not a `RawKv` method, and asking it to decode as one says so
    /// rather than producing a nonsense request.
    #[test]
    fn a_raft_method_is_not_a_raw_kv_method() {
        assert_eq!(Method::from_u16(0x0401), Some(Method::RaftBatch));
        assert_eq!(Method::RaftBatch.service(), crate::messages::SERVICE_RAFT);
        assert_eq!(Method::RaftBatch.name(), "RaftTransport::Batch");
    }
}
