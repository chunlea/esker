//! What PD writes into its Raft log: one command per durable write (*fixed*, format version 1).
//!
//! Every record of [ADR 0010](../../../docs/adr/0010-pd-durable-state.md) is now written by
//! exactly one place — [`Machine::apply`](crate::machine::Machine::apply) — and reached by exactly
//! one route: a leader proposes one of these, waits for it to commit and apply, and only then
//! answers ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)).
//!
//! # The rule this module exists to keep
//!
//! **Every non-deterministic input the leader sampled travels inside the command.** Three members
//! that each read a wall clock inside `apply` would diverge, and a state machine that diverges is
//! not one. It would also be a second place in Esker that orders on a wall clock, which
//! `CLAUDE.md` invariant 6 forbids outright. So `now_ms` is a *field* here, sampled once by the
//! leader, and `apply` reads no clock at all.
//!
//! The same rule decides the two ids in [`Command::Bootstrap`]: the cluster id and the region's
//! base id are minted by the leader and carried, rather than re-minted per member.
//!
//! Where no clock is involved the command carries the **request** and `apply` decides, because
//! there the log order *is* the decision. [`Command::RegionBeat`] is that case: the epoch guard
//! compares the beat against what PD holds, and letting the log settle which beat is newer is
//! exactly what the guard means.
//!
//! # Format
//!
//! ```text
//! Command = version:u8 ++ kind:u8 ++ fields
//! ```
//!
//! The kind bytes are fixed and never move; `0` is reserved and never valid, so a run of zero
//! bytes is not a readable command. A decode that finds an unknown version, an unknown kind, a
//! short field or a trailing byte is an error value, never a guess (`CLAUDE.md` invariants 2
//! and 9).
//!
//! This is not a wire format. It is PD's log, read only by PD, and it changes with a migration
//! rather than with `WIRE_VERSION` — the same separation `record` keeps for the same reason.

use bytes::Bytes;
use esker_proto::pd::ColumnarWish;

use crate::error::{PdError, Result};
use crate::record::{
    ColumnarRecord, OperatorEvent, RegionRecord, StoreStats, get_event, put_event,
};

/// Version byte on every command.
const COMMAND_VERSION: u8 = 1;

/// Kind bytes. Part of the format: these numbers are fixed, and zero is reserved.
mod kind {
    pub(super) const TAKE_OFFICE: u8 = 1;
    pub(super) const BOOTSTRAP: u8 = 2;
    pub(super) const RESERVE_IDS: u8 = 3;
    pub(super) const ADVANCE_TSO: u8 = 4;
    pub(super) const STORE_BEAT: u8 = 5;
    pub(super) const REGION_BEAT: u8 = 6;
    pub(super) const COLUMNAR: u8 = 7;
    pub(super) const HISTORY: u8 = 8;
}

/// One durable write, as it travels through PD's Raft log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A new leader's barrier: it serves nothing until this applies.
    ///
    /// Raft promises a new leader's *log* holds every committed entry and says nothing about
    /// `applied`, and PD's oracle and allocator are rebuilt out of applied state. Applying this
    /// means every entry before it has applied too — which is every entry committed under any
    /// earlier leader — so this is the moment the leader may start answering
    /// ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)).
    ///
    /// It writes nothing. Its value is entirely in where it sits in the log.
    TakeOffice {
        /// The term the leader took office in. Recorded for the log's readers, not read by apply.
        term: u64,
        /// The leader's clock as it took office; what the oracle resumes against.
        now_ms: u64,
    },

    /// Register a store, and create the cluster if this is the first one.
    Bootstrap {
        /// The registering store.
        store_id: u64,
        /// Where to reach it.
        address: String,
        /// The first of the two ids the leader reserved: the region's, and its first peer's.
        base_id: u64,
        /// The cluster id the leader minted, used only if there is no cluster yet.
        cluster_id: u64,
        /// The leader's clock.
        now_ms: u64,
    },

    /// Move the allocator's reserved batch end. Applied as `max`, never as assignment.
    ReserveIds {
        /// The last id now reserved.
        end: u64,
    },

    /// Move the oracle's high-water mark. Applied as `max`, never as assignment.
    AdvanceTso {
        /// The mark, in physical milliseconds.
        mark: u64,
    },

    /// What a store reported about itself.
    StoreBeat {
        /// Which store.
        store_id: u64,
        /// Its capacity and load.
        stats: StoreStats,
        /// The leader's clock. Liveness is PD's clock and never the store's own report.
        now_ms: u64,
    },

    /// What a region's leader reported, subject to the epoch guard at apply.
    ///
    /// The payload is the record that *would* be written — the beat plus the leader's clock —
    /// because that is exactly what apply writes when the guard accepts. Building it here rather
    /// than at apply keeps the clock out of the state machine.
    RegionBeat {
        /// The record the beat asks PD to hold.
        record: RegionRecord,
    },

    /// Which key ranges want columnar replicas, as the SQL layer last reported.
    Columnar {
        /// The whole list. A report is a full assertion, never a delta.
        wishes: Vec<ColumnarWish>,
    },

    /// One operator event for the history ring.
    History {
        /// What happened.
        event: OperatorEvent,
    },
}

impl Command {
    /// The command's bytes.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut out = esker_proto::Encoder::new();
        out.put_u8(COMMAND_VERSION);
        match self {
            Self::TakeOffice { term, now_ms } => {
                out.put_u8(kind::TAKE_OFFICE);
                out.put_varint(*term);
                out.put_varint(*now_ms);
            }
            Self::Bootstrap {
                store_id,
                address,
                base_id,
                cluster_id,
                now_ms,
            } => {
                out.put_u8(kind::BOOTSTRAP);
                out.put_varint(*store_id);
                out.put_str(address);
                out.put_varint(*base_id);
                out.put_u64(*cluster_id);
                out.put_varint(*now_ms);
            }
            Self::ReserveIds { end } => {
                out.put_u8(kind::RESERVE_IDS);
                out.put_u64(*end);
            }
            Self::AdvanceTso { mark } => {
                out.put_u8(kind::ADVANCE_TSO);
                out.put_varint(*mark);
            }
            Self::StoreBeat {
                store_id,
                stats,
                now_ms,
            } => {
                out.put_u8(kind::STORE_BEAT);
                out.put_varint(*store_id);
                out.put_varint(stats.capacity);
                out.put_varint(stats.available);
                out.put_varint(stats.region_count);
                out.put_varint(stats.leader_count);
                out.put_varint(stats.applied_bytes);
                out.put_varint(*now_ms);
            }
            Self::RegionBeat { record } => {
                out.put_u8(kind::REGION_BEAT);
                // The record's own encoder, which is golden-tested. A second encoding of a
                // `Region` inside this crate would be a second place to keep in step.
                out.put_bytes(&record.encode());
            }
            Self::Columnar { wishes } => {
                out.put_u8(kind::COLUMNAR);
                out.put_bytes(
                    &ColumnarRecord {
                        wishes: wishes.clone(),
                    }
                    .encode(),
                );
            }
            Self::History { event } => {
                out.put_u8(kind::HISTORY);
                put_event(&mut out, event);
            }
        }
        Bytes::from(out.finish())
    }

    /// Reads a command written by [`Command::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const WHAT: &str = "pd command";
        let bad = |error: esker_proto::DecodeError| PdError::corrupt(WHAT, error.to_string());

        let mut input = esker_proto::Decoder::new(bytes);
        let version = input.get_u8("command.version").map_err(bad)?;
        if version != COMMAND_VERSION {
            return Err(PdError::corrupt(
                WHAT,
                format!("format version {version}, expected {COMMAND_VERSION}"),
            ));
        }
        let command = match input.get_u8("command.kind").map_err(bad)? {
            kind::TAKE_OFFICE => Self::TakeOffice {
                term: input.get_varint("take_office.term").map_err(bad)?,
                now_ms: input.get_varint("take_office.now_ms").map_err(bad)?,
            },
            kind::BOOTSTRAP => Self::Bootstrap {
                store_id: input.get_varint("bootstrap.store_id").map_err(bad)?,
                address: input.get_str("bootstrap.address").map_err(bad)?.to_owned(),
                base_id: input.get_varint("bootstrap.base_id").map_err(bad)?,
                cluster_id: input.get_u64("bootstrap.cluster_id").map_err(bad)?,
                now_ms: input.get_varint("bootstrap.now_ms").map_err(bad)?,
            },
            kind::RESERVE_IDS => Self::ReserveIds {
                end: input.get_u64("reserve_ids.end").map_err(bad)?,
            },
            kind::ADVANCE_TSO => Self::AdvanceTso {
                mark: input.get_varint("advance_tso.mark").map_err(bad)?,
            },
            kind::STORE_BEAT => Self::StoreBeat {
                store_id: input.get_varint("store_beat.store_id").map_err(bad)?,
                stats: StoreStats {
                    capacity: input.get_varint("store_beat.capacity").map_err(bad)?,
                    available: input.get_varint("store_beat.available").map_err(bad)?,
                    region_count: input.get_varint("store_beat.region_count").map_err(bad)?,
                    leader_count: input.get_varint("store_beat.leader_count").map_err(bad)?,
                    applied_bytes: input.get_varint("store_beat.applied_bytes").map_err(bad)?,
                },
                now_ms: input.get_varint("store_beat.now_ms").map_err(bad)?,
            },
            kind::REGION_BEAT => Self::RegionBeat {
                record: RegionRecord::decode(input.get_bytes("region_beat.record").map_err(bad)?)?,
            },
            kind::COLUMNAR => Self::Columnar {
                wishes: ColumnarRecord::decode(input.get_bytes("columnar.record").map_err(bad)?)?
                    .wishes,
            },
            kind::HISTORY => Self::History {
                event: get_event(&mut input, WHAT)?,
            },
            other => {
                return Err(PdError::corrupt(WHAT, format!("command kind {other}")));
            }
        };
        input.finish().map_err(bad)?;
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::{COMMAND_VERSION, Command};
    use crate::record::{EventKind, EventOutcome, OperatorEvent, RegionRecord, StoreStats};
    use esker_proto::pd::ColumnarWish;
    use esker_proto::{Peer, PeerRole, Region};

    fn region() -> Region {
        Region {
            id: 7,
            start_key: bytes::Bytes::from_static(b"a"),
            end_key: bytes::Bytes::new(),
            epoch: esker_proto::Epoch {
                conf_ver: 2,
                version: 3,
            },
            peers: vec![
                Peer {
                    peer_id: 1,
                    store_id: 10,
                    role: PeerRole::Voter,
                },
                Peer {
                    peer_id: 2,
                    store_id: 11,
                    role: PeerRole::Learner,
                },
            ],
        }
    }

    fn every_kind() -> Vec<Command> {
        vec![
            Command::TakeOffice {
                term: 4,
                now_ms: 1_700_000_000_000,
            },
            Command::Bootstrap {
                store_id: 1,
                address: "127.0.0.1:20160".to_owned(),
                base_id: 1,
                cluster_id: 0xDEAD_BEEF,
                now_ms: 1_700_000_000_000,
            },
            Command::ReserveIds { end: 1_000 },
            Command::AdvanceTso {
                mark: 1_700_000_003_000,
            },
            Command::StoreBeat {
                store_id: 3,
                stats: StoreStats {
                    capacity: 1 << 40,
                    available: 1 << 39,
                    region_count: 12,
                    leader_count: 5,
                    applied_bytes: 999,
                },
                now_ms: 1_700_000_000_001,
            },
            Command::RegionBeat {
                record: RegionRecord {
                    region: region(),
                    leader_peer_id: 1,
                    term: 9,
                    approximate_size: 4_096,
                    applied_index: 55,
                    last_heartbeat_ms: 1_700_000_000_002,
                },
            },
            Command::Columnar {
                wishes: vec![ColumnarWish {
                    start_key: bytes::Bytes::from_static(b"t"),
                    end_key: bytes::Bytes::new(),
                    replicas: 2,
                }],
            },
            Command::History {
                event: OperatorEvent {
                    at_ms: 1_700_000_000_003,
                    region_id: 7,
                    kind: EventKind::AddPeer,
                    outcome: EventOutcome::Issued,
                    store_id: 11,
                    peer_id: 2,
                },
            },
        ]
    }

    /// Every kind round trips. A command that cannot be read back is a member that cannot
    /// replay its own log.
    #[test]
    fn every_command_round_trips() {
        for command in every_kind() {
            let bytes = command.encode();
            assert_eq!(Command::decode(&bytes).unwrap(), command, "{command:?}");
        }
    }

    /// The kind bytes are the format. A reordering of the enum must not silently renumber them.
    #[test]
    fn the_kind_bytes_are_where_they_are_pinned() {
        let kinds: Vec<u8> = every_kind()
            .iter()
            .map(|command| command.encode()[1])
            .collect();
        assert_eq!(kinds, [1, 2, 3, 4, 5, 6, 7, 8]);
        for command in every_kind() {
            assert_eq!(command.encode()[0], COMMAND_VERSION);
        }
    }

    /// Bytes off a disk are never trusted: a wrong version, an unknown kind, a truncation and a
    /// trailing byte are all errors, and none of them is a panic
    /// (`CLAUDE.md` invariants 2 and 9).
    #[test]
    fn a_corrupt_command_is_an_error_and_never_a_panic() {
        assert!(Command::decode(&[]).is_err(), "empty");
        assert!(Command::decode(&[9, 1]).is_err(), "version");
        assert!(Command::decode(&[COMMAND_VERSION, 0]).is_err(), "kind zero");
        assert!(
            Command::decode(&[COMMAND_VERSION, 200]).is_err(),
            "unknown kind"
        );
        for command in every_kind() {
            let bytes = command.encode();
            for cut in 2..bytes.len() {
                assert!(
                    Command::decode(&bytes[..cut]).is_err(),
                    "{command:?} truncated to {cut} decoded"
                );
            }
            let mut trailing = bytes.to_vec();
            trailing.push(0);
            assert!(
                Command::decode(&trailing).is_err(),
                "{command:?} accepted a trailing byte"
            );
        }
    }

    /// Zero is reserved everywhere in this codebase's formats, so a run of zero bytes must not
    /// read as a command.
    #[test]
    fn a_run_of_zeroes_is_not_a_command() {
        assert!(Command::decode(&[0; 32]).is_err());
    }
}
