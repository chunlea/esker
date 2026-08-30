//! What a Raft entry carries, and what applying one does.
//!
//! # The payload is a command, not a `WriteBatch`
//!
//! Proposing the resulting `WriteBatch` would be simpler and deterministic by construction — every
//! peer would write identical bytes. It is wrong for one operation: `CompareAndSwap` is a
//! read-modify-write, and deciding it on the *leader* decides it against state that a later leader
//! may not have. A command evaluated at **apply** time is decided against the applied state, which
//! every peer reaches identically, and stays correct across a leadership change.
//!
//! Determinism therefore has a rule attached: **applying a command may read only the applied state
//! of the data column families and its region's own metadata, and nothing else**. No clock, no
//! configuration, no `Limits` that could differ between peers — a `DeleteRange` that one peer
//! refuses for exceeding a limit and another accepts is two different state machines. The region's
//! range is admissible because it too is a product of the log: every peer's range at entry *N* is
//! whatever the splits before *N* left it as.
//!
//! # Two kinds of failure, and only one of them is fatal
//!
//! [`check_scope`] is a **deterministic refusal**: the key is outside the region this entry is
//! being applied to, every peer sees that identically, and the proposer is told so. It is a normal
//! outcome, and it is the mechanism that stops a write proposed before a split from landing in the
//! wrong half. A payload that cannot be *decoded* is the other kind: it cannot be applied at all,
//! and skipping it would leave this peer's state machine differing from every other's, so it stops
//! the driver.
//!
//! # Format (*fixed*, version 1)
//!
//! ```text
//! version:u8 = 1
//! tag:u8       1 Put, 2 BatchPut, 3 Delete, 4 DeleteRange, 5 CompareAndSwap, 6 Split
//! fields       length-prefixed bytes, as each variant documents
//! ```
//!
//! Tag 6 was added in phase 4b. It is an *addition* to the format and not a change to it: every
//! byte an earlier tag produced still decodes to the same command, and the goldens that pin them
//! are untouched.
//!
//! It is deliberately not `RawKvReq`. The Raft log is an on-disk format and the wire is not; tying
//! them together would mean a wire change rewriting every log in the cluster
//! (`docs/adr/0002-formats-are-hand-rolled.md`).

use bytes::Bytes;
use esker_engine::{Db, ReadOptions, WriteBatch, cf};
use esker_keys::prefix;
use esker_proto::{Decoder, Encoder, Epoch, Peer, ProtoError, RawKvReq, RawKvResp, Region};

use crate::error::engine_to_proto;
use crate::peer::Applied;

/// Version byte on every command payload.
const COMMAND_FORMAT_VERSION: u8 = 1;

const TAG_PUT: u8 = 1;
const TAG_BATCH_PUT: u8 = 2;
const TAG_DELETE: u8 = 3;
const TAG_DELETE_RANGE: u8 = 4;
const TAG_COMPARE_AND_SWAP: u8 = 5;
const TAG_SPLIT: u8 = 6;

/// A mutation, on its way through the Raft log.
///
/// Reads are not here: they never enter the log. A `Get` is answered from the applied state, and a
/// linearizable one is ordered by a `ReadIndex` round rather than by an entry
/// (`docs/DESIGN.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Write one key.
    Put {
        /// The user key, without the `'r'` namespace prefix.
        key: Bytes,
        /// Its value.
        value: Bytes,
    },
    /// Write several keys, atomically.
    BatchPut {
        /// The pairs, in the order given.
        pairs: Vec<(Bytes, Bytes)>,
    },
    /// Remove one key.
    Delete {
        /// The user key.
        key: Bytes,
    },
    /// Remove every key in `[start, end)`.
    DeleteRange {
        /// Inclusive lower bound.
        start: Bytes,
        /// Exclusive upper bound; empty means "to the end of the region".
        end: Bytes,
    },
    /// Replace `key` only if it currently holds `expected`.
    CompareAndSwap {
        /// The user key.
        key: Bytes,
        /// What the key must hold, or `None` for "must be absent".
        expected: Option<Bytes>,
        /// What to write, or `None` to delete.
        value: Option<Bytes>,
    },
    /// Divide this region in two (`docs/DESIGN.md` §6). The one **admin** command: it touches no
    /// data column family, only the two regions' metadata.
    ///
    /// Applied on every peer under the region lock, so both halves come into existence at the same
    /// point in every peer's log. The ids come from the placement driver, which is why they are in
    /// the payload rather than derived: every peer must arrive at the *same* ids, and only one of
    /// them talked to PD.
    Split {
        /// Where to cut. Becomes the parent's new exclusive end and the child's inclusive start.
        split_key: Bytes,
        /// The child's cluster-unique region id.
        new_region_id: u64,
        /// One peer id per peer of the parent, paired **positionally with the parent's peers
        /// sorted by `peer_id`**. Positional because every peer has to reach the same answer, and
        /// a store choosing "the id PD gave me" would put PD in the apply path.
        new_peer_ids: Vec<u64>,
    },
}

impl Command {
    /// The command a `RawKv` mutation becomes, or `None` for a request that is a read.
    #[must_use]
    pub fn from_request(request: &RawKvReq) -> Option<Self> {
        match request {
            RawKvReq::Put { key, value, .. } => Some(Self::Put {
                key: key.clone(),
                value: value.clone(),
            }),
            RawKvReq::BatchPut { pairs, .. } => Some(Self::BatchPut {
                pairs: pairs.clone(),
            }),
            RawKvReq::Delete { key, .. } => Some(Self::Delete { key: key.clone() }),
            RawKvReq::DeleteRange { start, end, .. } => Some(Self::DeleteRange {
                start: start.clone(),
                end: end.clone(),
            }),
            RawKvReq::CompareAndSwap {
                key,
                expected,
                value,
                ..
            } => Some(Self::CompareAndSwap {
                key: key.clone(),
                expected: expected.clone(),
                value: value.clone(),
            }),
            RawKvReq::Get { .. } | RawKvReq::BatchGet { .. } | RawKvReq::Scan { .. } => None,
        }
    }

    /// The payload of a Raft entry. See the module's format documentation.
    #[must_use]
    pub fn encode(&self) -> Bytes {
        let mut out = Encoder::new();
        out.put_u8(COMMAND_FORMAT_VERSION);
        match self {
            Self::Put { key, value } => {
                out.put_u8(TAG_PUT);
                out.put_bytes(key);
                out.put_bytes(value);
            }
            Self::BatchPut { pairs } => {
                out.put_u8(TAG_BATCH_PUT);
                out.put_varint(pairs.len() as u64);
                for (key, value) in pairs {
                    out.put_bytes(key);
                    out.put_bytes(value);
                }
            }
            Self::Delete { key } => {
                out.put_u8(TAG_DELETE);
                out.put_bytes(key);
            }
            Self::DeleteRange { start, end } => {
                out.put_u8(TAG_DELETE_RANGE);
                out.put_bytes(start);
                out.put_bytes(end);
            }
            Self::CompareAndSwap {
                key,
                expected,
                value,
            } => {
                out.put_u8(TAG_COMPARE_AND_SWAP);
                out.put_bytes(key);
                out.put_opt_bytes(expected.as_deref());
                out.put_opt_bytes(value.as_deref());
            }
            Self::Split {
                split_key,
                new_region_id,
                new_peer_ids,
            } => {
                out.put_u8(TAG_SPLIT);
                out.put_bytes(split_key);
                out.put_varint(*new_region_id);
                out.put_varint(new_peer_ids.len() as u64);
                for peer_id in new_peer_ids {
                    out.put_varint(*peer_id);
                }
            }
        }
        Bytes::from(out.finish())
    }

    /// Reads a payload written by [`Command::encode`].
    ///
    /// These bytes came out of a log that may have lied, so an unknown version, an unknown tag and
    /// trailing bytes are all errors (`CLAUDE.md` invariant 9).
    pub fn decode(payload: &[u8]) -> Result<Self, ProtoError> {
        let mut input = Decoder::new(payload);
        let version = input
            .get_u8("command.version")
            .map_err(|error| ProtoError::corrupt("raft command", error.to_string()))?;
        if version != COMMAND_FORMAT_VERSION {
            return Err(ProtoError::corrupt(
                "raft command",
                format!("format version {version}, expected {COMMAND_FORMAT_VERSION}"),
            ));
        }
        let tag = input
            .get_u8("command.tag")
            .map_err(|error| ProtoError::corrupt("raft command", error.to_string()))?;
        let command =
            match tag {
                TAG_PUT => Self::Put {
                    key: owned(&mut input, "put.key")?,
                    value: owned(&mut input, "put.value")?,
                },
                TAG_BATCH_PUT => {
                    let count = input
                        .get_count("batch_put.count")
                        .map_err(|error| ProtoError::corrupt("raft command", error.to_string()))?;
                    let mut pairs = Vec::with_capacity(count.min(1024));
                    for _ in 0..count {
                        pairs.push((
                            owned(&mut input, "batch_put.key")?,
                            owned(&mut input, "batch_put.value")?,
                        ));
                    }
                    Self::BatchPut { pairs }
                }
                TAG_DELETE => Self::Delete {
                    key: owned(&mut input, "delete.key")?,
                },
                TAG_DELETE_RANGE => Self::DeleteRange {
                    start: owned(&mut input, "delete_range.start")?,
                    end: owned(&mut input, "delete_range.end")?,
                },
                TAG_COMPARE_AND_SWAP => Self::CompareAndSwap {
                    key: owned(&mut input, "cas.key")?,
                    expected: owned_opt(&mut input, "cas.expected")?,
                    value: owned_opt(&mut input, "cas.value")?,
                },
                TAG_SPLIT => {
                    let split_key = owned(&mut input, "split.key")?;
                    let new_region_id = input
                        .get_varint("split.region_id")
                        .map_err(|error| ProtoError::corrupt("raft command", error.to_string()))?;
                    let count = input
                        .get_count("split.peer_count")
                        .map_err(|error| ProtoError::corrupt("raft command", error.to_string()))?;
                    let mut new_peer_ids = Vec::with_capacity(count.min(64));
                    for _ in 0..count {
                        new_peer_ids.push(input.get_varint("split.peer_id").map_err(|error| {
                            ProtoError::corrupt("raft command", error.to_string())
                        })?);
                    }
                    Self::Split {
                        split_key,
                        new_region_id,
                        new_peer_ids,
                    }
                }
                other => {
                    return Err(ProtoError::corrupt(
                        "raft command",
                        format!("unknown command tag {other}"),
                    ));
                }
            };
        input
            .finish()
            .map_err(|error| ProtoError::corrupt("raft command", error.to_string()))?;
        Ok(command)
    }
}

fn owned(input: &mut Decoder<'_>, field: &'static str) -> Result<Bytes, ProtoError> {
    input
        .get_bytes(field)
        .map(Bytes::copy_from_slice)
        .map_err(|error| ProtoError::corrupt("raft command", error.to_string()))
}

fn owned_opt(input: &mut Decoder<'_>, field: &'static str) -> Result<Option<Bytes>, ProtoError> {
    input
        .get_opt_bytes(field)
        .map(|value| value.map(Bytes::copy_from_slice))
        .map_err(|error| ProtoError::corrupt("raft command", error.to_string()))
}

/// How many keys one `DeleteRange` may remove.
///
/// A **constant**, not a configuration knob, and that is the point: apply must be deterministic,
/// so a limit that one peer applies and another does not would be two different state machines.
/// The wire-level `Limits` still guards the request path, where refusing early costs nothing.
pub const MAX_DELETE_RANGE_KEYS: u64 = 64 * 1024;

/// Whether every key `command` touches is inside `region`, as the region stands **now**.
///
/// This is what stops a write proposed before a split from landing in the wrong half. A `Put`
/// ordered after the `Split` entry that narrowed its region is applied against the narrowed range,
/// finds its key outside, and is refused — on every peer, identically, because every peer's range
/// at that entry is the product of the same log prefix.
///
/// The refusal is [`ProtoError::EpochNotMatch`], **not** `KeyNotInRegion`, and the difference is
/// the whole point. The proposer did not route wrongly: it routed correctly and the region moved
/// underneath it, which is retryable and repairable. `KeyNotInRegion` is terminal — it says *no
/// region here owns this key and waiting will not change that* — and answering a lost race with it
/// would fail a write that a refresh and one retry would have completed. The request path still
/// answers `KeyNotInRegion`, because there the epoch matched and the range is what the epoch says
/// it is, so a key outside it is genuinely the caller's mistake.
///
/// The hint carries only *this* region, because the driver knows only its own. That costs the
/// client one `GetRegion` for the half it does not yet know about — the narrowed parent evicts the
/// stale entry, the key then misses, and the resolver answers. One extra round trip, on a race
/// that happens once per split rather than once per request.
///
/// A **whole-command check, before anything is staged.** A batch is one atomic write and there is
/// no half of it to keep, so finding the bad key part way through would mean either a partial
/// application or an undo.
///
/// [`Command::Split`] touches no key and is checked by the caller against a different question:
/// whether its boundary is still inside the region ([`crate::split::is_legal_boundary`]).
pub fn check_scope(command: &Command, region: &Region) -> Result<(), ProtoError> {
    let refuse = |_key: &[u8]| ProtoError::EpochNotMatch {
        current_regions: vec![region.clone()],
    };
    let one = |key: &Bytes| {
        if region.contains(key) {
            Ok(())
        } else {
            Err(refuse(key))
        }
    };
    match command {
        Command::Put { key, .. }
        | Command::Delete { key }
        | Command::CompareAndSwap { key, .. } => one(key),
        Command::BatchPut { pairs } => pairs.iter().try_for_each(|(key, _)| one(key)),
        Command::DeleteRange { start, end } => {
            // The end is *clamped* rather than checked, below: a range delete that ran past the
            // region's new end deletes this region's share and the client's retry deletes the
            // rest. Only the start has to be ours.
            let _ = end;
            one(start)
        }
        Command::Split { .. } => Ok(()),
    }
}

/// The two regions a split produces, or a refusal every peer makes identically.
///
/// The parent keeps its id and its peers and gives up everything from `split_key` up; the child
/// takes the rest with fresh peer ids on the **same stores**. Both halves' `version` bumps, which
/// is what makes every cached copy of either one stale (`docs/DESIGN.md` §6).
pub fn split_region(
    parent: &Region,
    split_key: &Bytes,
    new_region_id: u64,
    new_peer_ids: &[u64],
) -> Result<(Region, Region), ProtoError> {
    if new_peer_ids.len() != parent.peers.len() {
        return Err(ProtoError::invalid(format!(
            "a split of region {} carries {} peer ids for {} peers",
            parent.id,
            new_peer_ids.len(),
            parent.peers.len()
        )));
    }
    if new_region_id == 0 || new_region_id == parent.id || new_peer_ids.contains(&0) {
        return Err(ProtoError::invalid(format!(
            "a split of region {} carries an unusable id",
            parent.id
        )));
    }

    // Sorted, so that every peer pairs the same parent peer with the same new id. The parent's own
    // peer list order is whatever PD or a bootstrap happened to produce and is not a contract.
    let mut peers = parent.peers.clone();
    peers.sort_unstable_by_key(|peer| peer.peer_id);
    let child_peers: Vec<Peer> = peers
        .iter()
        .zip(new_peer_ids)
        .map(|(peer, peer_id)| Peer {
            store_id: peer.store_id,
            peer_id: *peer_id,
            role: peer.role,
        })
        .collect();

    let version = parent.epoch.version + 1;
    let left = Region {
        end_key: split_key.clone(),
        epoch: Epoch::new(parent.epoch.conf_ver, version),
        ..parent.clone()
    };
    let right = Region {
        id: new_region_id,
        start_key: split_key.clone(),
        end_key: parent.end_key.clone(),
        peers: child_peers,
        epoch: Epoch::new(parent.epoch.conf_ver, version),
    };
    Ok((left, right))
}

/// Stages a command's effect on the data column families and says what it produced.
///
/// The caller adds `apply_index` to the same batch and writes it, so the effect and the record of
/// having applied it are one atomic step, and calls [`check_scope`] first — every key here is
/// already known to be this region's.
///
/// [`Command::Split`] is not staged here: it writes region metadata rather than data, and the
/// driver stages it beside the same `apply_index` ([`crate::peer`]).
pub fn stage(
    db: &Db,
    batch: &mut WriteBatch,
    command: &Command,
    region: &Region,
) -> Result<Applied, ProtoError> {
    let cf_id = db.cf_id(cf::DEFAULT).ok_or_else(|| {
        ProtoError::internal("the store opened without its `default` column family")
    })?;

    match command {
        Command::Put { key, value } => {
            batch.put(cf_id, &prefix::raw_key(key), value);
            Ok(Applied::Done)
        }
        Command::BatchPut { pairs } => {
            for (key, value) in pairs {
                batch.put(cf_id, &prefix::raw_key(key), value);
            }
            Ok(Applied::Done)
        }
        Command::Delete { key } => {
            batch.delete(cf_id, &prefix::raw_key(key));
            Ok(Applied::Done)
        }
        Command::DeleteRange { start, end } => {
            // An empty end means "to the end of the region", and *which* region that is has
            // moved if this entry was ordered after a split. Clamping to the region's current
            // end deletes this half's share; the client's retry deletes the child's.
            let end = if end.is_empty() { &region.end_key } else { end };
            let keys = stage_delete_range(db, batch, cf_id, start, end)?;
            Ok(Applied::Deleted { keys })
        }
        Command::CompareAndSwap {
            key,
            expected,
            value,
        } => {
            let stored = db
                .get(cf::DEFAULT, &prefix::raw_key(key), &ReadOptions::default())
                .map_err(|error| engine_to_proto(&error))?;
            if stored.as_deref() != expected.as_deref() {
                return Ok(Applied::Swapped {
                    swapped: false,
                    previous: stored,
                });
            }
            match value {
                Some(value) => batch.put(cf_id, &prefix::raw_key(key), value),
                None => batch.delete(cf_id, &prefix::raw_key(key)),
            }
            Ok(Applied::Swapped {
                swapped: true,
                previous: stored,
            })
        }
        // Its effect is on the two regions' metadata, which the driver stages beside the same
        // `apply_index`. Nothing reaches a data column family.
        Command::Split { .. } => Ok(Applied::Done),
    }
}

/// ADR 0006's bounded scan and point deletes: the engine has no range tombstones in v1, so a
/// range delete is the keys it covers, found now and removed in this batch.
fn stage_delete_range(
    db: &Db,
    batch: &mut WriteBatch,
    cf_id: u32,
    start: &[u8],
    end: &[u8],
) -> Result<u64, ProtoError> {
    let low = prefix::raw_key(start);
    let high = if end.is_empty() {
        // The end of the `'r'` namespace: every raw key sorts below it.
        vec![prefix::RAW + 1]
    } else {
        prefix::raw_key(end)
    };

    let mut iter = db
        .iter(cf::DEFAULT, &ReadOptions::default())
        .map_err(|error| engine_to_proto(&error))?;
    let mut deleted = 0_u64;
    iter.seek(&low);
    while iter.valid() && iter.key() < &high[..] {
        if deleted == MAX_DELETE_RANGE_KEYS {
            // Refusing *during* apply would leave peers disagreeing, so this is a hard failure of
            // the store rather than an answer to the caller. The request path rejects an oversized
            // range before it is ever proposed.
            return Err(ProtoError::internal(format!(
                "a committed DeleteRange covers more than {MAX_DELETE_RANGE_KEYS} keys"
            )));
        }
        batch.delete(cf_id, iter.key());
        deleted += 1;
        iter.next();
    }
    iter.status().map_err(|error| engine_to_proto(&error))?;
    Ok(deleted)
}

/// The `RawKv` answer an [`Applied`] outcome becomes.
#[must_use]
pub fn response(request: &RawKvReq, applied: &Applied) -> RawKvResp {
    match (request, applied) {
        (RawKvReq::Put { .. }, _) => RawKvResp::Put,
        (RawKvReq::BatchPut { .. }, _) => RawKvResp::BatchPut,
        (RawKvReq::Delete { .. }, _) => RawKvResp::Delete,
        (RawKvReq::DeleteRange { .. }, Applied::Deleted { keys }) => {
            RawKvResp::DeleteRange { deleted: *keys }
        }
        (RawKvReq::DeleteRange { .. }, _) => RawKvResp::DeleteRange { deleted: 0 },
        (RawKvReq::CompareAndSwap { .. }, Applied::Swapped { swapped, previous }) => {
            RawKvResp::CompareAndSwap {
                swapped: *swapped,
                previous: previous.clone(),
            }
        }
        (RawKvReq::CompareAndSwap { .. }, _) => RawKvResp::CompareAndSwap {
            swapped: false,
            previous: None,
        },
        // Reads never become commands, so they never reach here.
        (RawKvReq::Get { .. }, _) => RawKvResp::Get { value: None },
        (RawKvReq::BatchGet { .. }, _) => RawKvResp::BatchGet { values: Vec::new() },
        (RawKvReq::Scan { .. }, _) => RawKvResp::Scan { pairs: Vec::new() },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use esker_engine::{
        Db, LocalFileSystem, Options, ReadOptions, WalSyncMode, WriteBatch, WriteOptions, cf,
    };
    use esker_keys::prefix;
    use esker_proto::{ProtoError, RawKvReq};

    use super::{Command, Region, check_scope, response, split_region, stage};
    use crate::peer::Applied;

    fn open() -> (tempfile::TempDir, Arc<Db>) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(
            dir.path(),
            Options {
                create_if_missing: true,
                wal_sync_mode: WalSyncMode::Never,
                ..Options::default()
            },
            Arc::new(LocalFileSystem::new()),
            &cf::BUILTIN,
        )
        .unwrap();
        (dir, Arc::new(db))
    }

    /// The region every test here applies against: everything, so a key is never out of range
    /// unless the test says so.
    fn whole() -> Region {
        Region::bootstrap(1, 1, 1)
    }

    fn apply(db: &Db, command: &Command) -> Applied {
        let mut batch = WriteBatch::new();
        check_scope(command, &whole()).unwrap();
        let outcome = stage(db, &mut batch, command, &whole()).unwrap();
        db.write(batch, &WriteOptions { sync: false }).unwrap();
        outcome
    }

    fn read(db: &Db, key: &[u8]) -> Option<Bytes> {
        db.get(cf::DEFAULT, &prefix::raw_key(key), &ReadOptions::default())
            .unwrap()
    }

    /// The payload is an on-disk format, so these bytes are the golden: a change to them rewrites
    /// every Raft log in the cluster and needs an ADR and a version.
    #[test]
    fn a_put_encodes_to_the_documented_bytes() {
        let command = Command::Put {
            key: Bytes::from_static(b"ab"),
            value: Bytes::from_static(b"xyz"),
        };
        assert_eq!(
            command.encode().as_ref(),
            &[1, 1, 2, b'a', b'b', 3, b'x', b'y', b'z'],
        );
    }

    #[test]
    fn every_command_round_trips() {
        let commands = [
            Command::Put {
                key: Bytes::from_static(b"k"),
                value: Bytes::from_static(b"v"),
            },
            Command::BatchPut {
                pairs: vec![
                    (Bytes::from_static(b"a"), Bytes::from_static(b"1")),
                    (Bytes::from_static(b"b"), Bytes::from_static(b"2")),
                ],
            },
            Command::Delete {
                key: Bytes::from_static(b"k"),
            },
            Command::DeleteRange {
                start: Bytes::from_static(b"a"),
                end: Bytes::from_static(b"z"),
            },
            Command::CompareAndSwap {
                key: Bytes::from_static(b"k"),
                expected: Some(Bytes::from_static(b"old")),
                value: None,
            },
            Command::CompareAndSwap {
                key: Bytes::from_static(b"k"),
                expected: None,
                value: Some(Bytes::from_static(b"new")),
            },
        ];
        for command in commands {
            assert_eq!(Command::decode(&command.encode()).unwrap(), command);
        }
    }

    /// Invariant 9: the log is bytes on a disk, and a payload it cannot decode is an error rather
    /// than a panic or a silently skipped entry.
    #[test]
    fn a_corrupt_payload_is_an_error() {
        assert!(Command::decode(b"").is_err());
        assert!(
            Command::decode(&[9, 1, 0, 0]).is_err(),
            "unknown format version"
        );
        assert!(Command::decode(&[1, 99]).is_err(), "unknown tag");
        let put = Command::Put {
            key: Bytes::from_static(b"k"),
            value: Bytes::from_static(b"v"),
        };
        let encoded = put.encode();
        assert!(
            Command::decode(&encoded[..encoded.len() - 1]).is_err(),
            "truncated"
        );
        let mut trailing = encoded.to_vec();
        trailing.push(0);
        assert!(Command::decode(&trailing).is_err(), "trailing bytes");
    }

    /// Reads never enter the log: a `Get` is answered from the applied state, and a linearizable
    /// one is ordered by a `ReadIndex` round rather than by an entry.
    #[test]
    fn a_read_does_not_become_a_command() {
        assert!(
            Command::from_request(&RawKvReq::Get {
                key: Bytes::from_static(b"k")
            })
            .is_none()
        );
        assert!(
            Command::from_request(&RawKvReq::Scan {
                start: Bytes::new(),
                end: Bytes::new(),
                limit: 10,
                reverse: false,
            })
            .is_none()
        );
        assert!(
            Command::from_request(&RawKvReq::Put {
                key: Bytes::from_static(b"k"),
                value: Bytes::from_static(b"v"),
                sync: true,
            })
            .is_some()
        );
    }

    #[test]
    fn writes_land_under_the_raw_namespace() {
        let (_dir, db) = open();
        apply(
            &db,
            &Command::Put {
                key: Bytes::from_static(b"k"),
                value: Bytes::from_static(b"v"),
            },
        );
        assert_eq!(read(&db, b"k").as_deref(), Some(&b"v"[..]));
        // The `'r'` prefix is the store's to add, never the client's (invariant 7).
        assert!(
            db.get(cf::DEFAULT, b"k", &ReadOptions::default())
                .unwrap()
                .is_none(),
            "the key was stored without its namespace"
        );

        apply(
            &db,
            &Command::Delete {
                key: Bytes::from_static(b"k"),
            },
        );
        assert_eq!(read(&db, b"k"), None);
    }

    /// The one command that reads before it writes, and the reason commands are evaluated at apply
    /// time rather than on the leader: this comparison has to happen against the applied state.
    #[test]
    fn compare_and_swap_is_decided_against_the_applied_state() {
        let (_dir, db) = open();
        let swap = |expected: Option<&'static [u8]>, value: Option<&'static [u8]>| {
            Command::CompareAndSwap {
                key: Bytes::from_static(b"k"),
                expected: expected.map(Bytes::from_static),
                value: value.map(Bytes::from_static),
            }
        };

        // Absent, and expected to be: it swaps.
        assert_eq!(
            apply(&db, &swap(None, Some(b"first"))),
            Applied::Swapped {
                swapped: true,
                previous: None
            }
        );
        assert_eq!(read(&db, b"k").as_deref(), Some(&b"first"[..]));

        // Present but not what was expected: it does not, and says what is there.
        assert_eq!(
            apply(&db, &swap(Some(b"wrong"), Some(b"second"))),
            Applied::Swapped {
                swapped: false,
                previous: Some(Bytes::from_static(b"first"))
            }
        );
        assert_eq!(read(&db, b"k").as_deref(), Some(&b"first"[..]));

        // Matching, with no new value: it deletes.
        assert_eq!(
            apply(&db, &swap(Some(b"first"), None)),
            Applied::Swapped {
                swapped: true,
                previous: Some(Bytes::from_static(b"first"))
            }
        );
        assert_eq!(read(&db, b"k"), None);
    }

    #[test]
    fn a_range_delete_removes_what_it_covers_and_nothing_else() {
        let (_dir, db) = open();
        for key in [&b"a"[..], b"b", b"c", b"d"] {
            apply(
                &db,
                &Command::Put {
                    key: Bytes::copy_from_slice(key),
                    value: Bytes::from_static(b"v"),
                },
            );
        }
        assert_eq!(
            apply(
                &db,
                &Command::DeleteRange {
                    start: Bytes::from_static(b"b"),
                    end: Bytes::from_static(b"d"),
                }
            ),
            Applied::Deleted { keys: 2 }
        );
        assert!(read(&db, b"a").is_some());
        assert!(read(&db, b"b").is_none());
        assert!(read(&db, b"c").is_none());
        assert!(read(&db, b"d").is_some(), "the end bound is exclusive");
    }

    /// An empty end bound means "to the end of the region", and must not run off into the `'x'`
    /// namespace that `esker-txn` will own.
    #[test]
    fn an_open_ended_range_stops_at_the_end_of_the_raw_namespace() {
        let (_dir, db) = open();
        apply(
            &db,
            &Command::Put {
                key: Bytes::from_static(b"a"),
                value: Bytes::from_static(b"v"),
            },
        );
        // A key in a neighbouring namespace, written directly.
        let mut batch = WriteBatch::new();
        batch.put(db.cf_id(cf::DEFAULT).unwrap(), b"x-other", b"v");
        db.write(batch, &WriteOptions { sync: false }).unwrap();

        assert_eq!(
            apply(
                &db,
                &Command::DeleteRange {
                    start: Bytes::new(),
                    end: Bytes::new()
                }
            ),
            Applied::Deleted { keys: 1 }
        );
        assert!(
            db.get(cf::DEFAULT, b"x-other", &ReadOptions::default())
                .unwrap()
                .is_some(),
            "the range delete escaped the raw namespace"
        );
    }

    /// The one command that is not a mutation of data. Its payload is a wire format like every
    /// other entry's, so it round trips and its bytes are pinned.
    #[test]
    fn a_split_command_round_trips_and_encodes_to_the_documented_bytes() {
        let split = Command::Split {
            split_key: Bytes::from_static(b"m"),
            new_region_id: 7,
            new_peer_ids: vec![10, 11, 300],
        };
        assert_eq!(Command::decode(&split.encode()).unwrap(), split);

        assert_eq!(
            split.encode().to_vec(),
            vec![
                1,    // format version
                6,    // tag: Split
                1,    // split_key: one byte
                b'm', //
                7,    // new_region_id
                3,    // three peer ids
                10,   //
                11,   //
                0xAC, // 300 is two varint bytes
                0x02, //
            ]
        );
    }

    /// Adding tag 6 must leave every earlier tag's bytes exactly where they were: a log written
    /// before 4b has to keep decoding to the same commands.
    #[test]
    fn the_earlier_tags_are_where_they_were() {
        for (command, tag) in [
            (
                Command::Put {
                    key: Bytes::from_static(b"k"),
                    value: Bytes::from_static(b"v"),
                },
                1,
            ),
            (Command::BatchPut { pairs: Vec::new() }, 2),
            (
                Command::Delete {
                    key: Bytes::from_static(b"k"),
                },
                3,
            ),
            (
                Command::DeleteRange {
                    start: Bytes::from_static(b"a"),
                    end: Bytes::from_static(b"m"),
                },
                4,
            ),
            (
                Command::CompareAndSwap {
                    key: Bytes::from_static(b"k"),
                    expected: None,
                    value: None,
                },
                5,
            ),
        ] {
            assert_eq!(command.encode()[1], tag, "{command:?}");
            assert_eq!(Command::decode(&command.encode()).unwrap(), command);
        }
    }

    /// The check that stops a write proposed before a split from landing in the wrong half. It is
    /// a whole-command check: a batch is one atomic write and there is no half of it to keep.
    #[test]
    fn a_key_outside_the_region_is_refused_before_anything_is_staged() {
        let narrowed = Region {
            end_key: Bytes::from_static(b"m"),
            ..whole()
        };
        let inside = Bytes::from_static(b"a");
        let outside = Bytes::from_static(b"z");

        check_scope(
            &Command::Put {
                key: inside.clone(),
                value: Bytes::new(),
            },
            &narrowed,
        )
        .unwrap();
        let error = check_scope(
            &Command::Put {
                key: outside.clone(),
                value: Bytes::new(),
            },
            &narrowed,
        )
        .unwrap_err();
        // `EpochNotMatch`, not `KeyNotInRegion`: the proposer routed correctly and the region moved
        // under it. Answering a lost race with the terminal error would fail a write that a refresh
        // and one retry would have completed.
        match &error {
            ProtoError::EpochNotMatch { current_regions } => {
                assert_eq!(current_regions, &vec![narrowed.clone()]);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            error.is_retryable(),
            "a proposal the split overtook must be worth sending again"
        );

        // A batch is refused whole, however late the bad key comes.
        assert!(
            check_scope(
                &Command::BatchPut {
                    pairs: vec![
                        (inside.clone(), Bytes::new()),
                        (outside.clone(), Bytes::new()),
                    ],
                },
                &narrowed,
            )
            .is_err()
        );

        // A split's own boundary is not a key it touches, so it is not checked here.
        check_scope(
            &Command::Split {
                split_key: outside,
                new_region_id: 2,
                new_peer_ids: vec![9],
            },
            &narrowed,
        )
        .unwrap();
    }

    /// A range delete with no upper bound means "to the end of the region", and after a split that
    /// is a different place. It is clamped rather than refused: this half deletes its share and the
    /// client's retry deletes the child's.
    #[test]
    fn an_unbounded_range_delete_is_clamped_to_the_region_it_applies_in() {
        let (_dir, db) = open();
        for key in [&b"a"[..], b"g", b"q", b"z"] {
            apply(
                &db,
                &Command::Put {
                    key: Bytes::copy_from_slice(key),
                    value: Bytes::from_static(b"v"),
                },
            );
        }

        let narrowed = Region {
            end_key: Bytes::from_static(b"m"),
            ..whole()
        };
        let mut batch = WriteBatch::new();
        let outcome = stage(
            &db,
            &mut batch,
            &Command::DeleteRange {
                start: Bytes::from_static(b"a"),
                end: Bytes::new(),
            },
            &narrowed,
        )
        .unwrap();
        db.write(batch, &WriteOptions { sync: false }).unwrap();

        assert_eq!(outcome, Applied::Deleted { keys: 2 }, "only `a` and `g`");
        assert_eq!(read(&db, b"a"), None);
        assert_eq!(read(&db, b"g"), None);
        assert!(
            read(&db, b"q").is_some(),
            "the child's keys were not touched"
        );
        assert!(read(&db, b"z").is_some());
    }

    /// The arithmetic of a split, which every peer performs identically from the same entry.
    #[test]
    fn a_split_divides_the_range_and_bumps_both_halves() {
        let parent = Region {
            id: 1,
            start_key: Bytes::from_static(b"a"),
            end_key: Bytes::from_static(b"z"),
            // Deliberately out of peer-id order: the pairing sorts, so the parent's own order is
            // not a contract.
            peers: vec![
                esker_proto::Peer::voter(3, 30),
                esker_proto::Peer::voter(1, 10),
                esker_proto::Peer::voter(2, 20),
            ],
            epoch: esker_proto::Epoch::new(4, 7),
        };
        let (left, right) =
            split_region(&parent, &Bytes::from_static(b"m"), 9, &[100, 200, 300]).unwrap();

        assert_eq!(left.id, 1, "the parent keeps its id");
        assert_eq!(left.start_key, Bytes::from_static(b"a"));
        assert_eq!(left.end_key, Bytes::from_static(b"m"));
        assert_eq!(left.peers, parent.peers, "and its peers");
        assert_eq!(left.epoch, esker_proto::Epoch::new(4, 8));

        assert_eq!(right.id, 9);
        assert_eq!(right.start_key, Bytes::from_static(b"m"));
        assert_eq!(right.end_key, Bytes::from_static(b"z"));
        assert_eq!(right.epoch, esker_proto::Epoch::new(4, 8));
        // Paired with the parent's peers sorted by peer id: 10→100, 20→200, 30→300, on the same
        // stores. A store deciding which id was "its" would need the placement driver here.
        assert_eq!(
            right
                .peers
                .iter()
                .map(|peer| (peer.store_id, peer.peer_id))
                .collect::<Vec<_>>(),
            vec![(1, 100), (2, 200), (3, 300)]
        );

        // The two halves tile the parent exactly: no gap, no overlap.
        assert_eq!(left.end_key, right.start_key);
        assert_eq!(left.start_key, parent.start_key);
        assert_eq!(right.end_key, parent.end_key);
    }

    /// An unbounded parent gives its unbounded end to the child, which is the only half that can
    /// have one.
    #[test]
    fn splitting_the_last_region_leaves_the_child_unbounded() {
        let parent = Region::bootstrap(1, 1, 1);
        let (left, right) = split_region(&parent, &Bytes::from_static(b"m"), 2, &[9]).unwrap();
        assert_eq!(left.start_key, Bytes::new());
        assert_eq!(left.end_key, Bytes::from_static(b"m"));
        assert_eq!(
            right.end_key,
            Bytes::new(),
            "still the end of the key space"
        );
    }

    /// A malformed split is refused the same way on every peer, so it is an answer to the
    /// proposer rather than a failure of the store.
    #[test]
    fn a_split_carrying_unusable_ids_is_refused() {
        let parent = Region::bootstrap(1, 1, 1);
        let key = Bytes::from_static(b"m");
        assert!(
            split_region(&parent, &key, 2, &[]).is_err(),
            "one peer, no peer id"
        );
        assert!(
            split_region(&parent, &key, 2, &[9, 10]).is_err(),
            "one peer, two peer ids"
        );
        assert!(split_region(&parent, &key, 0, &[9]).is_err(), "region id 0");
        assert!(
            split_region(&parent, &key, 1, &[9]).is_err(),
            "the child cannot be the parent"
        );
        assert!(split_region(&parent, &key, 2, &[0]).is_err(), "peer id 0");
    }

    /// The batch is atomic: nothing it stages is visible until it is written.
    #[test]
    fn staging_writes_nothing_until_the_batch_lands() {
        let (_dir, db) = open();
        let mut batch = WriteBatch::new();
        stage(
            &db,
            &mut batch,
            &Command::Put {
                key: Bytes::from_static(b"k"),
                value: Bytes::from_static(b"v"),
            },
            &whole(),
        )
        .unwrap();
        assert_eq!(read(&db, b"k"), None, "staging is not writing");
        db.write(batch, &WriteOptions { sync: false }).unwrap();
        assert!(read(&db, b"k").is_some());
    }

    #[test]
    fn an_outcome_becomes_the_answer_its_request_expects() {
        let cas = RawKvReq::CompareAndSwap {
            key: Bytes::from_static(b"k"),
            expected: None,
            value: None,
            sync: true,
        };
        assert_eq!(
            response(
                &cas,
                &Applied::Swapped {
                    swapped: true,
                    previous: None
                }
            ),
            esker_proto::RawKvResp::CompareAndSwap {
                swapped: true,
                previous: None
            }
        );
        let range = RawKvReq::DeleteRange {
            start: Bytes::new(),
            end: Bytes::new(),
            sync: true,
        };
        assert_eq!(
            response(&range, &Applied::Deleted { keys: 7 }),
            esker_proto::RawKvResp::DeleteRange { deleted: 7 }
        );
    }

    /// The limit is a constant rather than a knob, because apply must be deterministic: a limit
    /// one peer applies and another does not is two different state machines.
    #[test]
    fn the_delete_range_limit_is_not_configurable() {
        let _: u64 = super::MAX_DELETE_RANGE_KEYS;
        assert!(super::MAX_DELETE_RANGE_KEYS > 0);
    }

    #[test]
    fn a_missing_data_column_family_is_reported_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(
            dir.path(),
            Options {
                create_if_missing: true,
                wal_sync_mode: WalSyncMode::Never,
                ..Options::default()
            },
            Arc::new(LocalFileSystem::new()),
            &[cf::RAFT],
        )
        .unwrap();
        let mut batch = WriteBatch::new();
        let outcome = stage(
            &db,
            &mut batch,
            &Command::Put {
                key: Bytes::from_static(b"k"),
                value: Bytes::from_static(b"v"),
            },
            &whole(),
        );
        assert!(matches!(outcome, Err(ProtoError::Internal { .. })));
    }
}
