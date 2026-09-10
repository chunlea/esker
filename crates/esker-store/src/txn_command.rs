//! What a transactional write looks like **in the Raft log**.
//!
//! Deliberately not `TxnKvReq`, for the reason [`crate::apply`]'s header gives about
//! `RawKvReq`: the log is an on-disk format and the wire is not, and tying them together would
//! make a wire change rewrite every log in the cluster
//! (`docs/adr/0002-formats-are-hand-rolled.md`). The two look alike because they describe the
//! same acts; they are separate so that either may move.
//!
//! # Why the request travels and not the mutations
//!
//! The obvious design puts the *decision* in the log: the leader reads its engine, runs
//! `esker-txn`, and proposes the resulting `Mutations`. It is wrong, and the reason is the one
//! `CompareAndSwap` already knows (`docs/plans/phase-5.md` §10.1). Two prewrites of one key
//! both read "no lock" on the leader, both propose, both apply — and the second overwrites the
//! first's lock, so two live transactions hold one key. Deciding *before* the log means racing
//! outside it.
//!
//! So the request travels, and every peer decides at apply against the state it holds at that
//! log index — identical on all of them, and sequential per region, so the decision is a pure
//! function of `(state, request)` and every peer reaches the same one.
//!
//! # Format (*fixed*, version 1)
//!
//! ```text
//! verb:u8      1 Prewrite, 2 Commit, 3 Rollback, 4 ResolveLock, 5 Heartbeat,
//!              6 ReleaseLock
//! fields       as each verb documents
//! ```
//!
//! Carried inside [`crate::apply::Command`]'s tag 7, so it is an *addition* to that format and
//! not a change to it: every byte an earlier tag produced still decodes to the same command.
//!
//! `GcSafepoint` is **not** here. It sets a store-local threshold rather than changing a
//! region's data, every peer learns it from the placement driver directly, and applying it is
//! idempotent and monotonic — so replicating it would put a number in the log that nothing
//! reads back.

use bytes::Bytes;
use esker_proto::codec::{Decoder, Encoder};
use esker_proto::{ProtoError, TxnKvReq, TxnMutation};

const VERB_PREWRITE: u8 = 1;
const VERB_COMMIT: u8 = 2;
const VERB_ROLLBACK: u8 = 3;
const VERB_RESOLVE_LOCK: u8 = 4;
const VERB_HEARTBEAT: u8 = 5;
/// **An addition to the verb space, not a change to it**
/// ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md) §2):
/// every byte an earlier verb produced still decodes to the same command. A peer that does not
/// know verb 6 refuses to decode it, which is the same rolling-upgrade rule every replicated
/// addition in this format carries.
const VERB_RELEASE_LOCK: u8 = 6;

/// One key's worth of a [`TxnCommand::Prewrite`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxnWrite {
    /// Write `value` at `key`.
    Put {
        /// The user key.
        key: Bytes,
        /// The value.
        value: Bytes,
        /// **The snapshot this value was computed from**, or `None` for "the transaction's own"
        /// (ADR 0057 §4).
        ///
        /// `None` in the log today: this is a **replicated** command, so carrying it is a log
        /// format change as well as a wire one, and both wait for the human's ruling. The field is
        /// here so that everything above it is built and tested; the encoding is untouched.
        read_ts: Option<u64>,
    },
    /// Remove `key`.
    Delete {
        /// The user key.
        key: Bytes,
        /// As [`TxnWrite::Put::read_ts`].
        read_ts: Option<u64>,
    },
    /// **A key a SERIALIZABLE transaction read**, verified and locked but never written
    /// ([ADR 0067](../../../docs/adr/0067-the-check-mutation-and-the-latest-commit-question.md)).
    Check {
        /// The user key that was read.
        key: Bytes,
    },
    /// **A range a SERIALIZABLE transaction scanned.** The phantom half: a row that did not exist
    /// when the scan ran is in no read set, and only the range can name it.
    CheckRange {
        /// Inclusive lower bound.
        start: Bytes,
        /// Exclusive upper bound.
        end: Bytes,
    },
}

impl TxnWrite {
    /// The user key this touches.
    #[must_use]
    pub fn key(&self) -> &Bytes {
        match self {
            Self::Put { key, .. } | Self::Delete { key, .. } | Self::Check { key } => key,
            // A range is addressed by its lower bound, as every range request is.
            Self::CheckRange { start, .. } => start,
        }
    }

    /// The wire form, for handing to `esker-txn`'s decision functions.
    #[must_use]
    pub fn to_wire(&self) -> TxnMutation {
        match self {
            Self::Put {
                key,
                value,
                read_ts,
            } => TxnMutation::Put {
                key: key.clone(),
                value: value.clone(),
                read_ts: *read_ts,
            },
            Self::Delete { key, read_ts } => TxnMutation::Delete {
                key: key.clone(),
                read_ts: *read_ts,
            },
            Self::Check { key } => TxnMutation::Check { key: key.clone() },
            Self::CheckRange { start, end } => TxnMutation::CheckRange {
                start: start.clone(),
                end: end.clone(),
            },
        }
    }
}

/// A transactional write, as the log carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxnCommand {
    /// Lock and stage a batch of one transaction's keys.
    Prewrite {
        /// The transaction's snapshot.
        start_ts: u64,
        /// The user key of its primary.
        primary: Bytes,
        /// How long the locks live without a heartbeat.
        ttl_ms: u64,
        /// What to write.
        writes: Vec<TxnWrite>,
    },
    /// Write the commit records for keys already prewritten.
    Commit {
        /// The transaction's snapshot.
        start_ts: u64,
        /// The timestamp every key of it commits at.
        commit_ts: u64,
        /// The user keys.
        keys: Vec<Bytes>,
    },
    /// Abandon this transaction's keys, leaving a marker on each.
    Rollback {
        /// The transaction's snapshot.
        start_ts: u64,
        /// The user keys.
        keys: Vec<Bytes>,
    },
    /// Give **this** transaction's own locks on these keys back, and leave it running
    /// ([ADR 0104](../../../docs/adr/0104-where-a-conflict-becomes-40001-and-where-40p01.md) §2).
    ///
    /// Replicated, and it has to be: it deletes a lock record, and a follower that never learned
    /// of the deletion would still refuse a reader for a lock the leader gave back — and would go
    /// on refusing after it became leader.
    ReleaseLock {
        /// The transaction's snapshot, and the owner every lock is checked against.
        start_ts: u64,
        /// The user keys.
        keys: Vec<Bytes>,
    },
    /// Apply a verdict about **another** transaction to the keys of it that live here.
    ResolveLock {
        /// The stuck transaction's snapshot.
        start_ts: u64,
        /// Its commit timestamp, or zero to roll it back. The caller's verdict, made against
        /// the primary's own region — see [`crate::txnkv::resolve_lock`].
        commit_ts: u64,
        /// The user keys.
        keys: Vec<Bytes>,
    },
    /// Extend a live transaction's lock TTL.
    Heartbeat {
        /// The transaction's snapshot.
        start_ts: u64,
        /// Its primary, the only lock that is heartbeated.
        primary: Bytes,
        /// The TTL to extend to. Never shortens one.
        ttl_ms: u64,
    },
}

impl TxnCommand {
    /// The command a request becomes, or `None` for one that changes nothing.
    ///
    /// `Get`, `Scan`, `LatestCommit`, `GcSafepoint` and `ReclaimRange` answer `None`: the first
    /// three are reads — `LatestCommit` asks what the store already knows and takes no lock
    /// (ADR 0067 §2) — and the last two are applied locally rather than through the log.
    ///
    /// **`ReclaimRange` writes, and still does not become a command.** It deletes the storage under
    /// a range nothing can route to any more, which is housekeeping each replica does to its own
    /// copy on its own schedule — the shape [ADR 0034](../../../docs/adr/0034-a-removed-peer-is-swept-and-its-range-reclaimed.md)
    /// already uses for a retired region's range. Putting it through the log would make one
    /// replica's compaction schedule the whole group's business, and would need a new
    /// `Command` variant — a replicated format change — to say nothing more than this does.
    #[must_use]
    pub fn from_request(request: &TxnKvReq) -> Option<Self> {
        match request {
            TxnKvReq::Prewrite {
                start_ts,
                primary,
                ttl_ms,
                mutations,
            } => Some(Self::Prewrite {
                start_ts: *start_ts,
                primary: primary.clone(),
                ttl_ms: *ttl_ms,
                writes: mutations
                    .iter()
                    .map(|mutation| match mutation {
                        TxnMutation::Put {
                            key,
                            value,
                            read_ts,
                        } => TxnWrite::Put {
                            key: key.clone(),
                            value: value.clone(),
                            read_ts: *read_ts,
                        },
                        TxnMutation::Delete { key, read_ts } => TxnWrite::Delete {
                            key: key.clone(),
                            read_ts: *read_ts,
                        },
                        TxnMutation::Check { key } => TxnWrite::Check { key: key.clone() },
                        TxnMutation::CheckRange { start, end } => TxnWrite::CheckRange {
                            start: start.clone(),
                            end: end.clone(),
                        },
                    })
                    .collect(),
            }),
            TxnKvReq::Commit {
                start_ts,
                commit_ts,
                keys,
            } => Some(Self::Commit {
                start_ts: *start_ts,
                commit_ts: *commit_ts,
                keys: keys.clone(),
            }),
            TxnKvReq::Rollback { start_ts, keys } => Some(Self::Rollback {
                start_ts: *start_ts,
                keys: keys.clone(),
            }),
            TxnKvReq::ReleaseLock { start_ts, keys } => Some(Self::ReleaseLock {
                start_ts: *start_ts,
                keys: keys.clone(),
            }),
            TxnKvReq::ResolveLock {
                start_ts,
                commit_ts,
                keys,
            } => Some(Self::ResolveLock {
                start_ts: *start_ts,
                commit_ts: *commit_ts,
                keys: keys.clone(),
            }),
            TxnKvReq::Heartbeat {
                start_ts,
                primary,
                ttl_ms,
            } => Some(Self::Heartbeat {
                start_ts: *start_ts,
                primary: primary.clone(),
                ttl_ms: *ttl_ms,
            }),
            TxnKvReq::Get { .. }
            | TxnKvReq::Scan { .. }
            | TxnKvReq::LatestCommit { .. }
            | TxnKvReq::GcSafepoint { .. }
            | TxnKvReq::ReclaimRange { .. } => None,
        }
    }

    /// Every user key this command touches, for the region's ownership check.
    ///
    /// A command must not be proposed into a region that does not own its keys — the check
    /// `esker-store` makes of every write, and the reason a `Prewrite` names its keys rather
    /// than only its primary: the primary may be elsewhere entirely.
    pub fn keys(&self) -> impl Iterator<Item = &Bytes> {
        // One iterator type for every arm, so the caller does not have to match again.
        let (writes, keys, primary): (&[TxnWrite], &[Bytes], Option<&Bytes>) = match self {
            Self::Prewrite { writes, .. } => (writes, &[], None),
            Self::Commit { keys, .. }
            | Self::Rollback { keys, .. }
            | Self::ReleaseLock { keys, .. }
            | Self::ResolveLock { keys, .. } => (&[], keys, None),
            Self::Heartbeat { primary, .. } => (&[], &[], Some(primary)),
        };
        writes
            .iter()
            .map(TxnWrite::key)
            .chain(keys.iter())
            .chain(primary)
    }

    /// The command's bytes, without the outer tag [`crate::apply::Command`] adds.
    pub(crate) fn encode_to(&self, out: &mut Encoder) {
        match self {
            Self::Prewrite {
                start_ts,
                primary,
                ttl_ms,
                writes,
            } => {
                out.put_u8(VERB_PREWRITE);
                out.put_varint(*start_ts);
                out.put_bytes(primary);
                out.put_varint(*ttl_ms);
                out.put_varint(writes.len() as u64);
                for write in writes {
                    match write {
                        // **Four kinds, and the first two are unchanged.** A write whose value
                        // came from the transaction's own snapshot encodes exactly the bytes it
                        // always did, so every log entry ever written still decodes and every
                        // golden is byte-identical. A write that says which snapshot it read at
                        // takes a kind of its own, which an older node refuses as unknown rather
                        // than misreading as a shorter entry (ADR 0057 §4).
                        TxnWrite::Put {
                            key,
                            value,
                            read_ts: None,
                        } => {
                            out.put_u8(1);
                            out.put_bytes(key);
                            out.put_bytes(value);
                        }
                        TxnWrite::Delete { key, read_ts: None } => {
                            out.put_u8(2);
                            out.put_bytes(key);
                        }
                        TxnWrite::Put {
                            key,
                            value,
                            read_ts: Some(read_ts),
                        } => {
                            out.put_u8(3);
                            out.put_bytes(key);
                            out.put_bytes(value);
                            out.put_varint(*read_ts);
                        }
                        TxnWrite::Delete {
                            key,
                            read_ts: Some(read_ts),
                        } => {
                            out.put_u8(4);
                            out.put_bytes(key);
                            out.put_varint(*read_ts);
                        }
                        // Kinds 5 and 6, beside the four: a check carries no value and a range
                        // carries two keys (ADR 0067 §1).
                        TxnWrite::Check { key } => {
                            out.put_u8(5);
                            out.put_bytes(key);
                        }
                        TxnWrite::CheckRange { start, end } => {
                            out.put_u8(6);
                            out.put_bytes(start);
                            out.put_bytes(end);
                        }
                    }
                }
            }
            Self::Commit {
                start_ts,
                commit_ts,
                keys,
            } => {
                out.put_u8(VERB_COMMIT);
                out.put_varint(*start_ts);
                out.put_varint(*commit_ts);
                put_keys(out, keys);
            }
            Self::Rollback { start_ts, keys } => {
                out.put_u8(VERB_ROLLBACK);
                out.put_varint(*start_ts);
                put_keys(out, keys);
            }
            Self::ReleaseLock { start_ts, keys } => {
                out.put_u8(VERB_RELEASE_LOCK);
                out.put_varint(*start_ts);
                put_keys(out, keys);
            }
            Self::ResolveLock {
                start_ts,
                commit_ts,
                keys,
            } => {
                out.put_u8(VERB_RESOLVE_LOCK);
                out.put_varint(*start_ts);
                out.put_varint(*commit_ts);
                put_keys(out, keys);
            }
            Self::Heartbeat {
                start_ts,
                primary,
                ttl_ms,
            } => {
                out.put_u8(VERB_HEARTBEAT);
                out.put_varint(*start_ts);
                out.put_bytes(primary);
                out.put_varint(*ttl_ms);
            }
        }
    }

    /// Reads a command. An unknown verb is an error, never a skipped entry: these bytes come
    /// off disk, and a log entry nobody can read is not one to shrug at
    /// (`CLAUDE.md` invariant 9).
    pub(crate) fn decode_from(input: &mut Decoder<'_>) -> Result<Self, ProtoError> {
        let verb = input
            .get_u8("txn.verb")
            .map_err(|error| ProtoError::corrupt("txn command", error.to_string()))?;
        let command = match verb {
            VERB_PREWRITE => {
                let start_ts = varint(input, "txn.start_ts")?;
                let primary = bytes(input, "txn.primary")?;
                let ttl_ms = varint(input, "txn.ttl_ms")?;
                let count = count(input, "txn.writes")?;
                let mut writes = Vec::with_capacity(count);
                for _ in 0..count {
                    let kind = input
                        .get_u8("txn.write.kind")
                        .map_err(|error| ProtoError::corrupt("txn command", error.to_string()))?;
                    writes.push(match kind {
                        1 => TxnWrite::Put {
                            key: bytes(input, "txn.write.key")?,
                            value: bytes(input, "txn.write.value")?,
                            read_ts: None,
                        },
                        3 => TxnWrite::Put {
                            key: bytes(input, "txn.write.key")?,
                            value: bytes(input, "txn.write.value")?,
                            read_ts: Some(varint(input, "txn.write.read_ts")?),
                        },
                        4 => TxnWrite::Delete {
                            key: bytes(input, "txn.write.key")?,
                            read_ts: Some(varint(input, "txn.write.read_ts")?),
                        },
                        2 => TxnWrite::Delete {
                            key: bytes(input, "txn.write.key")?,
                            read_ts: None,
                        },
                        5 => TxnWrite::Check {
                            key: bytes(input, "txn.write.key")?,
                        },
                        6 => TxnWrite::CheckRange {
                            start: bytes(input, "txn.write.start")?,
                            end: bytes(input, "txn.write.end")?,
                        },
                        other => {
                            return Err(ProtoError::corrupt(
                                "txn command",
                                format!("{other} is not a write kind"),
                            ));
                        }
                    });
                }
                Self::Prewrite {
                    start_ts,
                    primary,
                    ttl_ms,
                    writes,
                }
            }
            VERB_COMMIT => Self::Commit {
                start_ts: varint(input, "txn.start_ts")?,
                commit_ts: varint(input, "txn.commit_ts")?,
                keys: get_keys(input)?,
            },
            VERB_ROLLBACK => Self::Rollback {
                start_ts: varint(input, "txn.start_ts")?,
                keys: get_keys(input)?,
            },
            VERB_RELEASE_LOCK => Self::ReleaseLock {
                start_ts: varint(input, "txn.start_ts")?,
                keys: get_keys(input)?,
            },
            VERB_RESOLVE_LOCK => Self::ResolveLock {
                start_ts: varint(input, "txn.start_ts")?,
                commit_ts: varint(input, "txn.commit_ts")?,
                keys: get_keys(input)?,
            },
            VERB_HEARTBEAT => Self::Heartbeat {
                start_ts: varint(input, "txn.start_ts")?,
                primary: bytes(input, "txn.primary")?,
                ttl_ms: varint(input, "txn.ttl_ms")?,
            },
            other => {
                return Err(ProtoError::corrupt(
                    "txn command",
                    format!("{other} is not a transactional verb"),
                ));
            }
        };
        Ok(command)
    }
}

fn put_keys(out: &mut Encoder, keys: &[Bytes]) {
    out.put_varint(keys.len() as u64);
    for key in keys {
        out.put_bytes(key);
    }
}

fn get_keys(input: &mut Decoder<'_>) -> Result<Vec<Bytes>, ProtoError> {
    let count = count(input, "txn.keys")?;
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        keys.push(bytes(input, "txn.key")?);
    }
    Ok(keys)
}

fn varint(input: &mut Decoder<'_>, field: &'static str) -> Result<u64, ProtoError> {
    input
        .get_varint(field)
        .map_err(|error| ProtoError::corrupt("txn command", error.to_string()))
}

fn bytes(input: &mut Decoder<'_>, field: &'static str) -> Result<Bytes, ProtoError> {
    input
        .get_bytes(field)
        .map(Bytes::copy_from_slice)
        .map_err(|error| ProtoError::corrupt("txn command", error.to_string()))
}

fn count(input: &mut Decoder<'_>, field: &'static str) -> Result<usize, ProtoError> {
    input
        .get_count(field)
        .map_err(|error| ProtoError::corrupt("txn command", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::{TxnCommand, TxnWrite};
    use bytes::Bytes;
    use esker_proto::codec::{Decoder, Encoder};
    use esker_proto::{TxnKvReq, TxnMutation};

    fn round_trip(command: &TxnCommand) {
        let mut out = Encoder::new();
        command.encode_to(&mut out);
        let bytes = out.finish();
        let mut input = Decoder::new(&bytes);
        assert_eq!(&TxnCommand::decode_from(&mut input).unwrap(), command);
        input.finish().unwrap();
    }

    fn every_command() -> Vec<TxnCommand> {
        vec![
            TxnCommand::Prewrite {
                start_ts: 1 << 41,
                primary: Bytes::from_static(b"p"),
                ttl_ms: 3_000,
                writes: vec![
                    TxnWrite::Put {
                        key: Bytes::from_static(b"a"),
                        value: Bytes::from_static(b"1"),
                        read_ts: None,
                    },
                    TxnWrite::Delete {
                        key: Bytes::from_static(b"b"),
                        read_ts: None,
                    },
                ],
            },
            TxnCommand::Commit {
                start_ts: 10,
                commit_ts: 20,
                keys: vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")],
            },
            TxnCommand::Rollback {
                start_ts: 10,
                keys: vec![Bytes::from_static(b"a")],
            },
            TxnCommand::ResolveLock {
                start_ts: 10,
                commit_ts: 0,
                keys: vec![Bytes::from_static(b"a")],
            },
            TxnCommand::Heartbeat {
                start_ts: 10,
                primary: Bytes::from_static(b"p"),
                ttl_ms: 60_000,
            },
        ]
    }

    #[test]
    fn every_command_round_trips() {
        for command in every_command() {
            round_trip(&command);
        }
        // The shapes a hand-written case forgets: empty batches and empty keys.
        round_trip(&TxnCommand::Prewrite {
            start_ts: 0,
            primary: Bytes::new(),
            ttl_ms: 0,
            writes: vec![],
        });
        round_trip(&TxnCommand::Commit {
            start_ts: 0,
            commit_ts: 0,
            keys: vec![],
        });
    }

    /// A log entry nobody can read is not one to shrug at: an unknown verb, an unknown write
    /// kind and a truncation are all errors.
    #[test]
    fn a_malformed_command_is_an_error() {
        assert!(
            TxnCommand::decode_from(&mut Decoder::new(&[])).is_err(),
            "empty"
        );
        assert!(
            TxnCommand::decode_from(&mut Decoder::new(&[0])).is_err(),
            "verb 0"
        );
        assert!(
            TxnCommand::decode_from(&mut Decoder::new(&[9])).is_err(),
            "verb 9"
        );

        let mut out = Encoder::new();
        every_command()[0].encode_to(&mut out);
        let good = out.finish();
        for cut in 0..good.len() {
            let mut input = Decoder::new(&good[..cut]);
            let read = TxnCommand::decode_from(&mut input);
            assert!(
                read.is_err() || input.finish().is_err(),
                "a command cut to {cut} bytes decoded cleanly"
            );
        }

        // A write kind this version does not define.
        let mut out = Encoder::new();
        out.put_u8(1);
        out.put_varint(1);
        out.put_bytes(b"p");
        out.put_varint(1);
        out.put_varint(1);
        out.put_u8(9);
        let bytes = out.finish();
        assert!(TxnCommand::decode_from(&mut Decoder::new(&bytes)).is_err());
    }

    /// Reads and the safepoint never become commands: two of them change nothing, and the
    /// third is store-local (see the module header).
    #[test]
    fn only_the_writes_become_commands() {
        assert!(
            TxnCommand::from_request(&TxnKvReq::Get {
                key: Bytes::from_static(b"k"),
                ts: 1
            })
            .is_none()
        );
        assert!(
            TxnCommand::from_request(&TxnKvReq::Scan {
                start: Bytes::new(),
                end: Bytes::new(),
                limit: 1,
                ts: 1,
                reverse: false
            })
            .is_none()
        );
        assert!(TxnCommand::from_request(&TxnKvReq::GcSafepoint { safepoint: 1 }).is_none());
        assert!(
            TxnCommand::from_request(&TxnKvReq::Rollback {
                start_ts: 1,
                keys: vec![]
            })
            .is_some()
        );
    }

    /// Every key a command touches has to be visible to the region check, or a command could be
    /// proposed into a region that does not own what it writes.
    #[test]
    fn a_command_names_every_key_it_touches() {
        let prewrite = TxnCommand::from_request(&TxnKvReq::Prewrite {
            start_ts: 1,
            // The primary is elsewhere; this batch is secondaries only, and the check must be
            // about *these* keys rather than about it.
            primary: Bytes::from_static(b"far-away"),
            ttl_ms: 1,
            mutations: vec![
                TxnMutation::Put {
                    key: Bytes::from_static(b"a"),
                    value: Bytes::from_static(b"1"),
                    read_ts: None,
                },
                TxnMutation::Delete {
                    key: Bytes::from_static(b"b"),
                    read_ts: None,
                },
            ],
        })
        .unwrap();
        let keys: Vec<&Bytes> = prewrite.keys().collect();
        assert_eq!(
            keys,
            vec![&Bytes::from_static(b"a"), &Bytes::from_static(b"b")]
        );

        let heartbeat = TxnCommand::Heartbeat {
            start_ts: 1,
            primary: Bytes::from_static(b"p"),
            ttl_ms: 1,
        };
        assert_eq!(
            heartbeat.keys().collect::<Vec<_>>(),
            vec![&Bytes::from_static(b"p")],
            "a heartbeat is addressed to the primary's region"
        );
    }
}
