//! The Raft log, on the engine's `raft` column family.
//!
//! `esker-raft` reads its log through [`LogStorage`] and never writes: writes leave through
//! `Ready` and this layer performs them, which is what makes the persist-before-send rule
//! observable at all (`docs/adr/0008-raft-determinism-and-the-driver-contract.md`).
//!
//! # Layout (*fixed*, version 1)
//!
//! Three prefixes share the `raft` column family (`docs/DESIGN.md` §6). Ids and indices are
//! **big-endian** so that a range scan over one region's entries runs in index order — the whole
//! point of putting them in an ordered store:
//!
//! ```text
//! 'l' ++ region_id:u64 BE ++ index:u64 BE   →  one log entry
//! 's' ++ region_id:u64 BE                   →  hard state, apply index, configuration
//! 'm' ++ region_id:u64 BE                   →  region metadata (phase 4)
//! ```
//!
//! # The state record holds two things, and two writers touch it
//!
//! `'s'` carries the hard state *and* the apply index, because `docs/DESIGN.md` §6 says so and
//! because a peer needs both to restart. That means the persist step and the apply step both
//! write this key, and neither may clobber the other's field. The rule that makes it safe is that
//! [`RaftLogStorage`] is the single source of truth for both — it holds them in memory, and every
//! write emits the whole record — so whichever batch lands last leaves the latest of each on disk.
//!
//! # Caching
//!
//! The bounds (`first_index`, `last_index`), the hard state and the configuration are held in the
//! struct rather than read back per call, because `RawNode` owns its storage exclusively and only
//! the driver mutates it. There is no lock because there is no second writer; a stale cache is
//! impossible rather than unlikely.

use std::sync::Arc;

use bytes::Bytes;
use esker_engine::{Db, ReadOptions, WriteBatch, WriteOptions, cf};
use esker_proto::{Decoder, Encoder};
use esker_raft::{
    ConfState, Entry, EntryKind, HardState, Index, InitialState, LogStorage, NodeId, RaftError,
    Snapshot, Term,
};

use crate::error::{Result, StoreError};
use crate::raft_cf;

/// Version byte on the state record. A change to any field's meaning bumps it.
const STATE_FORMAT_VERSION: u8 = 1;

/// Version byte on a log entry's value.
const ENTRY_FORMAT_VERSION: u8 = 1;

/// Bytes in a log-entry key: prefix, region, index.
pub const LOG_KEY_LEN: usize = 1 + 8 + 8;

/// Bytes in a state or metadata key: prefix, region.
pub const REGION_KEY_LEN: usize = 1 + 8;

/// `'l' ++ region_id ++ index`, big-endian so a scan runs in index order.
#[must_use]
pub fn log_entry_key(region_id: u64, index: Index) -> [u8; LOG_KEY_LEN] {
    let mut key = [0_u8; LOG_KEY_LEN];
    key[0] = raft_cf::LOG_ENTRY;
    key[1..9].copy_from_slice(&region_id.to_be_bytes());
    key[9..].copy_from_slice(&index.to_be_bytes());
    key
}

/// `'s' ++ region_id`.
#[must_use]
pub fn state_key(region_id: u64) -> [u8; REGION_KEY_LEN] {
    let mut key = [0_u8; REGION_KEY_LEN];
    key[0] = raft_cf::STATE;
    key[1..].copy_from_slice(&region_id.to_be_bytes());
    key
}

/// `'m' ++ region_id`.
#[must_use]
pub fn metadata_key(region_id: u64) -> [u8; REGION_KEY_LEN] {
    let mut key = [0_u8; REGION_KEY_LEN];
    key[0] = raft_cf::METADATA;
    key[1..].copy_from_slice(&region_id.to_be_bytes());
    key
}

/// Encodes one log entry's value: `version ++ term ++ index ++ kind ++ data`.
///
/// The index is stored even though the key already carries it. It costs a varint and buys a
/// consistency check: a value read under the wrong key is caught rather than returned as an entry
/// that claims to be somewhere it is not.
#[must_use]
pub fn encode_entry(entry: &Entry) -> Vec<u8> {
    let mut out = Encoder::with_capacity(entry.data.len() + 16);
    out.put_u8(ENTRY_FORMAT_VERSION);
    out.put_varint(entry.term);
    out.put_varint(entry.index);
    out.put_u8(match entry.kind {
        EntryKind::Normal => 0,
        EntryKind::ConfChange => 1,
    });
    out.put_bytes(&entry.data);
    out.finish()
}

/// Decodes a log entry read under the key for `index`.
///
/// Bytes off a disk are never trusted (`CLAUDE.md` invariant 2): a wrong version, an unknown kind,
/// a disagreeing index and trailing bytes are all errors.
pub fn decode_entry(index: Index, bytes: &[u8]) -> Result<Entry> {
    let mut input = Decoder::new(bytes);
    let version = input
        .get_u8("entry.version")
        .map_err(|error| corrupt(&error))?;
    if version != ENTRY_FORMAT_VERSION {
        return Err(StoreError::Bootstrap(format!(
            "raft log entry at index {index} has format version {version}, expected {ENTRY_FORMAT_VERSION}"
        )));
    }
    let term = input
        .get_varint("entry.term")
        .map_err(|error| corrupt(&error))?;
    let stored_index = input
        .get_varint("entry.index")
        .map_err(|error| corrupt(&error))?;
    if stored_index != index {
        return Err(StoreError::Bootstrap(format!(
            "raft log entry stored under index {index} claims to be index {stored_index}"
        )));
    }
    let kind = match input
        .get_u8("entry.kind")
        .map_err(|error| corrupt(&error))?
    {
        0 => EntryKind::Normal,
        1 => EntryKind::ConfChange,
        other => {
            return Err(StoreError::Bootstrap(format!(
                "raft log entry at index {index} has unknown kind {other}"
            )));
        }
    };
    let data = Bytes::copy_from_slice(
        input
            .get_bytes("entry.data")
            .map_err(|error| corrupt(&error))?,
    );
    input.finish().map_err(|error| corrupt(&error))?;
    Ok(Entry {
        term,
        index,
        kind,
        data,
    })
}

fn corrupt(error: &esker_proto::DecodeError) -> StoreError {
    StoreError::Bootstrap(format!("corrupt raft record: {error}"))
}

/// Everything under the `'s'` key: what a peer needs to resume.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedState {
    /// Term, vote and commit index — Raft's persistent state.
    pub hard_state: HardState,
    /// The membership as of `truncated_index` — the index the log begins after — which is what
    /// [`esker_raft::InitialState::conf_state`] is specified to be.
    ///
    /// Not the membership *in force*, which is the tempting thing to keep: the core replays the
    /// log's conf-change entries onto this one, so what it needs is a configuration that predates
    /// the entries it still holds. Writing the current one instead would apply every change twice
    /// and leave the core unable to tell which part of its membership is still revertible.
    ///
    /// Nothing writes this after `open`, and nothing has to while the log starts at index 1: the
    /// membership the region was bootstrapped with *is* the membership as of index 0, and the
    /// entries say the rest. That stops being true the moment the log is truncated — see the
    /// `TODO(phase-4)` on [`RaftLogStorage::snapshot`].
    pub conf_state: ConfState,
    /// The highest index the state machine has applied, written with the data it applied.
    pub applied_index: Index,
    /// The last index folded into a snapshot; `0` when nothing has been compacted.
    pub truncated_index: Index,
    /// The term of the entry at `truncated_index`.
    pub truncated_term: Term,
}

impl PersistedState {
    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.put_u8(STATE_FORMAT_VERSION);
        out.put_varint(self.hard_state.term);
        // A vote is optional, and `0` is not a usable sentinel because node ids are the caller's
        // to choose. A flag byte says which of the two shapes follows.
        match self.hard_state.voted_for {
            None => out.put_u8(0),
            Some(node) => {
                out.put_u8(1);
                out.put_varint(node);
            }
        }
        out.put_varint(self.hard_state.commit);
        out.put_varint(self.applied_index);
        out.put_varint(self.truncated_index);
        out.put_varint(self.truncated_term);
        put_ids(&mut out, &self.conf_state.voters);
        put_ids(&mut out, &self.conf_state.learners);
        out.finish()
    }

    /// Reads a record written by [`PersistedState::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut input = Decoder::new(bytes);
        let version = input
            .get_u8("state.version")
            .map_err(|error| corrupt(&error))?;
        if version != STATE_FORMAT_VERSION {
            return Err(StoreError::Bootstrap(format!(
                "raft state record has format version {version}, expected {STATE_FORMAT_VERSION}"
            )));
        }
        let term = input
            .get_varint("state.term")
            .map_err(|error| corrupt(&error))?;
        let voted_for = match input
            .get_u8("state.vote_flag")
            .map_err(|error| corrupt(&error))?
        {
            0 => None,
            1 => Some(
                input
                    .get_varint("state.voted_for")
                    .map_err(|error| corrupt(&error))?,
            ),
            other => {
                return Err(StoreError::Bootstrap(format!(
                    "raft state record has vote flag {other}, expected 0 or 1"
                )));
            }
        };
        let commit = input
            .get_varint("state.commit")
            .map_err(|error| corrupt(&error))?;
        let applied_index = input
            .get_varint("state.applied")
            .map_err(|error| corrupt(&error))?;
        let truncated_index = input
            .get_varint("state.truncated_index")
            .map_err(|error| corrupt(&error))?;
        let truncated_term = input
            .get_varint("state.truncated_term")
            .map_err(|error| corrupt(&error))?;
        let voters = get_ids(&mut input, "state.voters")?;
        let learners = get_ids(&mut input, "state.learners")?;
        input.finish().map_err(|error| corrupt(&error))?;

        let mut conf_state = ConfState { voters, learners };
        conf_state.normalize();
        Ok(Self {
            hard_state: HardState {
                term,
                voted_for,
                commit,
            },
            conf_state,
            applied_index,
            truncated_index,
            truncated_term,
        })
    }
}

fn put_ids(out: &mut Encoder, ids: &[NodeId]) {
    out.put_varint(ids.len() as u64);
    for id in ids {
        out.put_varint(*id);
    }
}

fn get_ids(input: &mut Decoder<'_>, field: &'static str) -> Result<Vec<NodeId>> {
    let count = input.get_count(field).map_err(|error| corrupt(&error))?;
    let mut ids = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        ids.push(input.get_varint(field).map_err(|error| corrupt(&error))?);
    }
    Ok(ids)
}

/// One region's Raft log, over the `raft` column family.
#[derive(Debug)]
pub struct RaftLogStorage {
    db: Arc<Db>,
    cf: u32,
    region_id: u64,
    state: PersistedState,
    /// The highest index the log holds. `truncated_index` when the log is empty.
    last_index: Index,
}

impl RaftLogStorage {
    /// Opens the log for `region_id`, bootstrapping it with `conf_state` if it is not there yet.
    ///
    /// Bootstrapping writes the state record with an empty log, which is what makes a fresh peer
    /// distinguishable from one whose state record was lost.
    pub fn open(db: Arc<Db>, region_id: u64, conf_state: ConfState) -> Result<Self> {
        let cf = db
            .cf_id(cf::RAFT)
            .ok_or_else(|| StoreError::Bootstrap("the `raft` column family is missing".into()))?;

        let stored = db.get(cf::RAFT, &state_key(region_id), &ReadOptions::default())?;
        let state = if let Some(bytes) = stored {
            PersistedState::decode(&bytes)?
        } else {
            let mut conf_state = conf_state;
            conf_state.normalize();
            let fresh = PersistedState {
                conf_state,
                ..PersistedState::default()
            };
            let mut batch = WriteBatch::new();
            batch.put(cf, &state_key(region_id), &fresh.encode());
            db.write(batch, &WriteOptions { sync: true })?;
            fresh
        };

        let mut storage = Self {
            db,
            cf,
            region_id,
            state,
            last_index: 0,
        };
        storage.last_index = storage.scan_last_index()?;
        Ok(storage)
    }

    /// The last index actually present, found by seeking to the end of this region's entries.
    ///
    /// Done once, at open. Afterwards the driver's appends keep it current, which is why the
    /// bounds can live in the struct at all.
    fn scan_last_index(&self) -> Result<Index> {
        let mut iter = self.db.iter(cf::RAFT, &ReadOptions::default())?;
        // The key just past this region's last possible entry.
        let end = log_entry_key(self.region_id, Index::MAX);
        iter.seek_for_prev(&end);
        if iter.valid() {
            let key = iter.key();
            if key.len() == LOG_KEY_LEN
                && key[0] == raft_cf::LOG_ENTRY
                && key[1..9] == self.region_id.to_be_bytes()
            {
                let mut index = [0_u8; 8];
                index.copy_from_slice(&key[9..]);
                return Ok(u64::from_be_bytes(index));
            }
        }
        iter.status()?;
        Ok(self.state.truncated_index)
    }

    /// The region this log belongs to.
    #[must_use]
    pub fn region_id(&self) -> u64 {
        self.region_id
    }

    /// The `raft` column family's id.
    #[must_use]
    pub fn cf(&self) -> u32 {
        self.cf
    }

    /// The engine underneath.
    #[must_use]
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// The state record as it stands in memory — which is what will be written next.
    #[must_use]
    pub fn state(&self) -> &PersistedState {
        &self.state
    }

    /// Adds the state record to `batch`, in full.
    ///
    /// Every writer of the `'s'` key goes through this, so the record on disk always carries the
    /// latest hard state *and* the latest apply index rather than whichever the last writer
    /// happened to know about.
    pub fn stage_state(&self, batch: &mut WriteBatch) {
        batch.put(self.cf, &state_key(self.region_id), &self.state.encode());
    }

    /// Stages the persist half of a `Ready`: the new entries and the hard state.
    ///
    /// Entries that these replace are deleted in the same batch. A follower whose tail is being
    /// rewritten must not be left holding the old suffix: a later read would find entries past the
    /// new end and the log would appear longer than it is.
    pub fn stage_ready(
        &mut self,
        batch: &mut WriteBatch,
        hard_state: Option<HardState>,
        entries: &[Entry],
    ) {
        if let Some(hard_state) = hard_state {
            self.state.hard_state = hard_state;
        }
        if let Some(first) = entries.first() {
            for index in first.index..=self.last_index {
                batch.delete(self.cf, &log_entry_key(self.region_id, index));
            }
            for entry in entries {
                batch.put(
                    self.cf,
                    &log_entry_key(self.region_id, entry.index),
                    &encode_entry(entry),
                );
            }
            self.last_index = entries[entries.len() - 1].index;
        }
        self.stage_state(batch);
    }

    /// Records that the state machine has applied through `index`. The caller puts this in the
    /// **same** batch as the data it applied.
    pub fn stage_applied(&mut self, batch: &mut WriteBatch, index: Index) {
        self.state.applied_index = self.state.applied_index.max(index);
        self.stage_state(batch);
    }

    /// The apply index on disk, which is where a restart resumes.
    #[must_use]
    pub fn applied_index(&self) -> Index {
        self.state.applied_index
    }

    fn read_entry(&self, index: Index) -> std::result::Result<Option<Entry>, RaftError> {
        let key = log_entry_key(self.region_id, index);
        let stored = self
            .db
            .get(cf::RAFT, &key, &ReadOptions::default())
            .map_err(|error| RaftError::Storage(error.to_string()))?;
        match stored {
            None => Ok(None),
            Some(bytes) => decode_entry(index, &bytes)
                .map(Some)
                .map_err(|error| RaftError::Storage(error.to_string())),
        }
    }
}

impl LogStorage for RaftLogStorage {
    fn initial_state(&self) -> std::result::Result<InitialState, RaftError> {
        Ok(InitialState {
            hard_state: self.state.hard_state,
            conf_state: self.state.conf_state.clone(),
        })
    }

    fn entries(
        &self,
        low: Index,
        high: Index,
        max_bytes: u64,
    ) -> std::result::Result<Vec<Entry>, RaftError> {
        if low <= self.state.truncated_index {
            return Err(RaftError::Compacted(self.state.truncated_index));
        }
        if high > self.last_index + 1 {
            return Err(RaftError::Unavailable(high.saturating_sub(1)));
        }
        if low >= high {
            return Ok(Vec::new());
        }
        let mut out: Vec<Entry> = Vec::new();
        let mut budget: u64 = 0;
        for index in low..high {
            let entry = self
                .read_entry(index)?
                .ok_or(RaftError::Unavailable(index))?;
            budget = budget.saturating_add(entry.cost());
            // Always at least one, or a single oversized proposal stalls replication rather than
            // merely making one message large.
            if budget > max_bytes && !out.is_empty() {
                break;
            }
            out.push(entry);
        }
        Ok(out)
    }

    fn term(&self, index: Index) -> std::result::Result<Term, RaftError> {
        if index == self.state.truncated_index {
            return Ok(self.state.truncated_term);
        }
        if index < self.state.truncated_index {
            return Err(RaftError::Compacted(self.state.truncated_index));
        }
        if index > self.last_index {
            return Err(RaftError::Unavailable(index));
        }
        self.read_entry(index)?
            .map(|entry| entry.term)
            .ok_or(RaftError::Unavailable(index))
    }

    fn first_index(&self) -> std::result::Result<Index, RaftError> {
        Ok(self.state.truncated_index + 1)
    }

    fn last_index(&self) -> std::result::Result<Index, RaftError> {
        Ok(self.last_index)
    }

    fn snapshot(&self) -> std::result::Result<Snapshot, RaftError> {
        // TODO(phase-4): build a snapshot from `engine.checkpoint(range)` and stream it
        // (`docs/DESIGN.md` §6). A single region that never compacts its log never needs one, so
        // 3e reports "nothing compacted" rather than pretending.
        //
        // Whatever truncates the log has to move `PersistedState::conf_state` with it, in the
        // same batch: it is the membership as of the index the log begins after, and the entries
        // that established it are the ones truncation throws away. `SnapshotMeta::conf` is the
        // same value and must come from the same derivation — the core prefers it, precisely
        // because a snapshot names the index its membership is as of.
        Ok(Snapshot::default())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use esker_engine::{Db, LocalFileSystem, Options, WalSyncMode, WriteBatch, WriteOptions, cf};
    use esker_raft::{
        ConfChange, ConfChangeKind, ConfState, Config, Entry, EntryKind, HardState, LogStorage,
        RaftError, RawNode,
    };

    use super::{
        LOG_KEY_LEN, PersistedState, REGION_KEY_LEN, RaftLogStorage, decode_entry, encode_entry,
        log_entry_key, metadata_key, state_key,
    };

    fn open_db() -> (tempfile::TempDir, Arc<Db>) {
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

    fn open_log() -> (tempfile::TempDir, Arc<Db>, RaftLogStorage) {
        let (dir, db) = open_db();
        let log = RaftLogStorage::open(Arc::clone(&db), 7, ConfState::from_voters(vec![1, 2, 3]))
            .unwrap();
        (dir, db, log)
    }

    fn entries(spec: &[(u64, u64)]) -> Vec<Entry> {
        spec.iter()
            .map(|(term, index)| Entry::empty(*term, *index))
            .collect()
    }

    /// Persist a `Ready`'s worth of entries the way the driver does, so the tests below read the
    /// same bytes a real peer would.
    fn append(db: &Db, log: &mut RaftLogStorage, entries: &[Entry], hard: Option<HardState>) {
        let mut batch = WriteBatch::new();
        log.stage_ready(&mut batch, hard, entries);
        db.write(batch, &WriteOptions { sync: false }).unwrap();
    }

    /// The keys are an on-disk format. These bytes are the golden: a change to them is a format
    /// change and needs an ADR and a version (`CLAUDE.md`).
    #[test]
    fn the_raft_cf_keys_encode_to_the_documented_bytes() {
        assert_eq!(
            log_entry_key(0x0102_0304_0506_0708, 0x1112_1314_1516_1718),
            [
                b'l', 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x11, 0x12, 0x13, 0x14, 0x15,
                0x16, 0x17, 0x18,
            ],
        );
        assert_eq!(state_key(1), [b's', 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(metadata_key(1), [b'm', 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(LOG_KEY_LEN, 17);
        assert_eq!(REGION_KEY_LEN, 9);
    }

    /// Big-endian is the whole reason these live in an ordered store: a scan over one region's
    /// entries has to run in index order, and little-endian would interleave them by low byte.
    #[test]
    fn log_keys_sort_by_index_within_a_region() {
        let mut keys: Vec<[u8; LOG_KEY_LEN]> = [300_u64, 1, 256, 2, 65_536]
            .iter()
            .map(|index| log_entry_key(9, *index))
            .collect();
        keys.sort_unstable();
        let order: Vec<u64> = keys
            .iter()
            .map(|key| {
                let mut index = [0_u8; 8];
                index.copy_from_slice(&key[9..]);
                u64::from_be_bytes(index)
            })
            .collect();
        assert_eq!(order, vec![1, 2, 256, 300, 65_536]);

        // And one region's entries never stray into another's.
        assert!(log_entry_key(1, u64::MAX) < log_entry_key(2, 0));
    }

    #[test]
    fn an_entry_round_trips_through_its_value_encoding() {
        for kind in [EntryKind::Normal, EntryKind::ConfChange] {
            let entry = Entry {
                term: 9,
                index: 41,
                kind,
                data: Bytes::from_static(b"payload"),
            };
            assert_eq!(decode_entry(41, &encode_entry(&entry)).unwrap(), entry);
        }
    }

    /// Invariant 2: bytes off a disk are checked, and a disagreement is an error rather than an
    /// entry that quietly claims to be somewhere it is not.
    #[test]
    fn an_entry_read_under_the_wrong_index_is_corruption() {
        let entry = Entry::empty(3, 41);
        let bytes = encode_entry(&entry);
        assert!(decode_entry(42, &bytes).is_err());
        assert!(decode_entry(41, &bytes[..bytes.len() - 1]).is_err());
        assert!(decode_entry(41, b"").is_err());

        let mut wrong_version = bytes.clone();
        wrong_version[0] = 9;
        assert!(decode_entry(41, &wrong_version).is_err());
    }

    #[test]
    fn the_state_record_round_trips() {
        let state = PersistedState {
            hard_state: HardState {
                term: 4,
                voted_for: Some(2),
                commit: 11,
            },
            conf_state: ConfState {
                voters: vec![1, 2, 3],
                learners: vec![4],
            },
            applied_index: 9,
            truncated_index: 5,
            truncated_term: 2,
        };
        assert_eq!(PersistedState::decode(&state.encode()).unwrap(), state);

        // "No vote" is a flag, not a sentinel: node ids are the caller's to choose and 0 is not
        // reserved on the wire.
        let unvoted = PersistedState::default();
        assert_eq!(PersistedState::decode(&unvoted.encode()).unwrap(), unvoted);
    }

    #[test]
    fn a_corrupt_state_record_is_an_error() {
        let bytes = PersistedState::default().encode();
        assert!(PersistedState::decode(&bytes[..bytes.len() - 1]).is_err());
        assert!(PersistedState::decode(b"").is_err());
        let mut bad_flag = bytes.clone();
        bad_flag[2] = 7;
        assert!(PersistedState::decode(&bad_flag).is_err());
    }

    /// An empty log looks the same here as it does in `MemStorage`: nothing to read, and the next
    /// entry goes at index 1.
    #[test]
    fn a_fresh_log_starts_at_one_and_ends_at_zero() {
        let (_dir, _db, log) = open_log();
        assert_eq!(log.first_index().unwrap(), 1);
        assert_eq!(log.last_index().unwrap(), 0);
        assert_eq!(log.term(0).unwrap(), 0);
        assert!(log.entries(1, 1, u64::MAX).unwrap().is_empty());
        assert_eq!(
            log.initial_state().unwrap().conf_state.voters,
            vec![1, 2, 3]
        );
    }

    #[test]
    fn appended_entries_come_back_by_index_and_term() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2), (2, 3)]), None);

        assert_eq!(log.last_index().unwrap(), 3);
        assert_eq!(log.term(2).unwrap(), 1);
        assert_eq!(log.term(3).unwrap(), 2);
        assert_eq!(
            log.entries(2, 4, u64::MAX).unwrap(),
            entries(&[(1, 2), (2, 3)])
        );
        assert!(matches!(log.term(4), Err(RaftError::Unavailable(4))));
        assert!(matches!(
            log.entries(1, 5, u64::MAX),
            Err(RaftError::Unavailable(4))
        ));
    }

    /// What a follower does whenever a new leader replaces the tail its predecessor left. The old
    /// suffix has to *go*: a later read that found it would see a log longer than the one this
    /// peer actually has.
    #[test]
    fn a_rewritten_tail_removes_the_entries_it_replaced() {
        let (_dir, db, mut log) = open_log();
        append(
            &db,
            &mut log,
            &entries(&[(1, 1), (1, 2), (1, 3), (1, 4)]),
            None,
        );
        append(&db, &mut log, &entries(&[(2, 2)]), None);

        assert_eq!(log.last_index().unwrap(), 2);
        assert_eq!(log.term(2).unwrap(), 2);
        assert!(matches!(log.term(3), Err(RaftError::Unavailable(3))));
        assert!(
            db.get(
                cf::RAFT,
                &log_entry_key(7, 4),
                &esker_engine::ReadOptions::default()
            )
            .unwrap()
            .is_none(),
            "the replaced suffix is still on disk"
        );
    }

    /// A budget that could return nothing would stall replication on the first oversized proposal
    /// rather than merely making one message large.
    #[test]
    fn a_byte_budget_always_yields_at_least_one_entry() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2), (1, 3)]), None);
        assert_eq!(log.entries(1, 4, 0).unwrap().len(), 1);
        assert_eq!(log.entries(1, 4, u64::MAX).unwrap().len(), 3);
    }

    /// The hard state and the apply index share one key, and two different writers touch it. The
    /// rule that makes that safe is that both go through this type, which holds the latest of each
    /// and writes the whole record — so neither field is ever rolled back by the other's writer.
    #[test]
    fn the_state_record_keeps_the_latest_of_both_fields() {
        let (_dir, db, mut log) = open_log();

        // The persist step: a new hard state, with the apply index still where it was.
        append(
            &db,
            &mut log,
            &entries(&[(1, 1), (1, 2)]),
            Some(HardState {
                term: 3,
                voted_for: Some(2),
                commit: 2,
            }),
        );

        // The apply step: the data's batch also carries the apply index — and, through
        // `stage_applied`, the hard state the persist step had just recorded.
        let mut batch = WriteBatch::new();
        batch.put(log.cf(), b"anything", b"value");
        log.stage_applied(&mut batch, 2);
        db.write(batch, &WriteOptions { sync: false }).unwrap();

        let reopened =
            RaftLogStorage::open(Arc::clone(&db), 7, ConfState::from_voters(vec![9])).unwrap();
        assert_eq!(
            reopened.state().hard_state.term,
            3,
            "apply clobbered the hard state"
        );
        assert_eq!(reopened.state().hard_state.voted_for, Some(2));
        assert_eq!(reopened.applied_index(), 2);
        // And the configuration came from storage, not from what `open` was told.
        assert_eq!(
            reopened.initial_state().unwrap().conf_state.voters,
            vec![1, 2, 3]
        );
    }

    /// **What a restart recovers the membership from.** The store keeps the configuration as of
    /// the index the log begins after, and the core replays the log's conf-change entries onto it
    /// — so a peer comes back with the membership its own log establishes, without this layer
    /// keeping a second copy of it in step with the entries.
    ///
    /// Before that rule this record was documented as the membership as of the *last* entry, and
    /// nothing here ever wrote it: a peer restarted with the configuration it was bootstrapped
    /// with and lost every change that had been made since. It recovers them now because they are
    /// in the log, which is where they always were.
    #[test]
    fn a_restart_recovers_the_membership_from_the_log() {
        let (_dir, db, mut log) = open_log();
        let mut ready = entries(&[(1, 1)]);
        ready.push(Entry::conf_change(
            1,
            2,
            &ConfChange::new(ConfChangeKind::AddVoter, 4),
        ));
        ready.push(Entry::empty(1, 3));
        append(
            &db,
            &mut log,
            &ready,
            Some(HardState {
                term: 1,
                voted_for: None,
                commit: 3,
            }),
        );

        let reopened = RaftLogStorage::open(Arc::clone(&db), 7, ConfState::default()).unwrap();
        assert_eq!(
            reopened.initial_state().unwrap().conf_state.voters,
            vec![1, 2, 3],
            "what is on disk is the anchor: the membership as of index 0"
        );

        let node = RawNode::new(Config::new(1, vec![1, 2, 3], 41), reopened).unwrap();
        assert_eq!(
            node.status().conf.voters,
            vec![1, 2, 3, 4],
            "and index 2 is what makes 4 a member of it"
        );
    }

    /// Reopening finds the log where it was left, including its last index — which is scanned once
    /// rather than trusted from the state record, because the entries and the record are written
    /// in the same batch but a torn tail could still leave the record ahead.
    #[test]
    fn reopening_recovers_the_log_bounds() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2), (4, 3)]), None);

        let reopened = RaftLogStorage::open(Arc::clone(&db), 7, ConfState::default()).unwrap();
        assert_eq!(reopened.last_index().unwrap(), 3);
        assert_eq!(reopened.term(3).unwrap(), 4);
    }

    /// Two regions share the column family. One's entries must not be visible as the other's, and
    /// the last-index scan must not walk off the end of its own region.
    #[test]
    fn regions_do_not_see_each_others_entries() {
        let (_dir, db) = open_db();
        let mut first =
            RaftLogStorage::open(Arc::clone(&db), 1, ConfState::from_voters(vec![1])).unwrap();
        let mut second =
            RaftLogStorage::open(Arc::clone(&db), 2, ConfState::from_voters(vec![1])).unwrap();
        append(&db, &mut first, &entries(&[(1, 1), (1, 2), (1, 3)]), None);
        append(&db, &mut second, &entries(&[(5, 1)]), None);

        assert_eq!(first.last_index().unwrap(), 3);
        assert_eq!(second.last_index().unwrap(), 1);
        assert_eq!(second.term(1).unwrap(), 5);

        let reopened_second =
            RaftLogStorage::open(Arc::clone(&db), 2, ConfState::default()).unwrap();
        assert_eq!(reopened_second.last_index().unwrap(), 1);
    }

    /// 3e never compacts, so there is nothing to send. Reporting an empty snapshot is what the
    /// core reads as "nothing has been compacted"; pretending otherwise would make a leader send
    /// one that carries no state.
    #[test]
    fn a_log_that_has_never_compacted_offers_no_snapshot() {
        let (_dir, _db, log) = open_log();
        assert!(log.snapshot().unwrap().is_empty());
    }
}
