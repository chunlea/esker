//! PD's Raft log, on the engine's `raft` column family.
//!
//! The placement driver replicates itself with `esker-raft`
//! ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)), which means it needs the same
//! [`LogStorage`] a region's peer needs. This is that, and it is deliberately **not** shared with
//! `esker_store::raft_log`: that one is keyed by region id because a store holds many groups,
//! and PD holds exactly one. Sharing it would mean carrying a region id that is always the same
//! made-up number, and a made-up region id is a thing somebody will one day try to look up.
//!
//! # Layout (*fixed*, format version 1)
//!
//! Two prefixes in the `raft` column family, which is otherwise empty in PD's database. The index
//! is **big-endian** so a scan runs in index order, the same reason everything else here is:
//!
//! ```text
//! 'l' ++ index:u64 BE   →  one log entry
//! 's'                   →  hard state, apply index, membership, truncation point
//! ```
//!
//! This is a **new** key space, not a changed one. A 4a data directory has no `raft` family at
//! all; `Db::open_with` creates it, and every record of [ADR 0010](../../../docs/adr/0010-pd-durable-state.md)
//! keeps its key and its bytes in `default`. There is nothing to migrate.
//!
//! # One record, two writers
//!
//! `'s'` carries the hard state *and* the apply index, because a member needs both to restart —
//! and the persist step and the apply step both write it. The rule that makes that safe is the
//! store's rule, for the store's reason: [`PdLogStorage`] is the single source of truth for both,
//! holds them in memory, and every write emits the whole record, so whichever batch lands last
//! leaves the latest of each on disk.
//!
//! # Caching
//!
//! The bounds, the hard state and the membership live in the struct rather than being read back
//! per call. `RawNode` owns its storage exclusively and only the driver mutates it, so a stale
//! cache is impossible rather than unlikely.

use std::sync::Arc;

use bytes::Bytes;
use esker_engine::{Db, ReadOptions, WriteBatch, WriteOptions, cf};
use esker_proto::{Decoder, Encoder};
use esker_raft::{
    ConfState, Entry, EntryKind, HardState, Index, InitialState, LogStorage, NodeId, RaftError,
    Snapshot, SnapshotMeta, Term,
};

use crate::error::{PdError, Result};
use crate::member::{MAX_MEMBERS, PdMember};

/// Version byte on the state record.
///
/// **Version 2** appends the group id and the address book
/// ([ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)).
/// A version-1 record still reads: it decodes as group id **zero** and no members, which is exactly
/// the state a member written before dynamic membership is in — and zero is the signal to mint an
/// id from the configured list and write it down. Every field version 1 had is byte-identical, and
/// the two new ones are appended, so the upgrade is a decode branch rather than a migration.
const STATE_FORMAT_VERSION: u8 = 2;

/// The version this build still reads, and rewrites as [`STATE_FORMAT_VERSION`] on the next write.
const STATE_FORMAT_VERSION_V1: u8 = 1;

/// Version byte on a log entry's value.
const ENTRY_FORMAT_VERSION: u8 = 1;

/// Prefix of a log entry key.
pub const LOG_ENTRY: u8 = b'l';

/// Prefix of the state record's key.
pub const STATE: u8 = b's';

/// Bytes in a log-entry key: prefix and index.
pub const LOG_KEY_LEN: usize = 1 + 8;

/// `'l' ++ index`, big-endian so a scan runs in index order.
#[must_use]
pub fn log_entry_key(index: Index) -> [u8; LOG_KEY_LEN] {
    let mut key = [0_u8; LOG_KEY_LEN];
    key[0] = LOG_ENTRY;
    key[1..].copy_from_slice(&index.to_be_bytes());
    key
}

/// `'s'`, the one state record.
#[must_use]
pub fn state_key() -> [u8; 1] {
    [STATE]
}

/// Encodes one log entry's value: `version ++ term ++ index ++ kind ++ data`.
///
/// The index is stored even though the key carries it. It costs a varint and buys a consistency
/// check: a value read under the wrong key is caught rather than returned as an entry claiming to
/// be somewhere it is not.
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
    let field = |error: esker_proto::DecodeError| PdError::corrupt("raft entry", error.to_string());

    let version = input.get_u8("entry.version").map_err(field)?;
    if version != ENTRY_FORMAT_VERSION {
        return Err(PdError::corrupt(
            "raft entry",
            format!(
                "entry at index {index} has format version {version}, expected \
                 {ENTRY_FORMAT_VERSION}"
            ),
        ));
    }
    let term = input.get_varint("entry.term").map_err(field)?;
    let stored_index = input.get_varint("entry.index").map_err(field)?;
    if stored_index != index {
        return Err(PdError::corrupt(
            "raft entry",
            format!("entry under index {index} says it is index {stored_index}"),
        ));
    }
    let kind = match input.get_u8("entry.kind").map_err(field)? {
        0 => EntryKind::Normal,
        1 => EntryKind::ConfChange,
        other => {
            return Err(PdError::corrupt(
                "raft entry",
                format!("entry at index {index} has kind {other}, expected 0 or 1"),
            ));
        }
    };
    let data = Bytes::copy_from_slice(input.get_bytes("entry.data").map_err(field)?);
    input.finish().map_err(field)?;
    Ok(Entry {
        term,
        index,
        kind,
        data,
    })
}

/// The `'s'` record: everything a member needs to restart that is not an entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistedState {
    /// Term, vote and commit index — Raft's persistent state.
    pub hard_state: HardState,
    /// The membership as of `truncated_index`, which is what
    /// [`InitialState::conf_state`] is specified to be.
    ///
    /// Not the membership *in force*: the core replays the log's conf-change entries onto this
    /// one, so what it needs is a configuration that predates the entries still held. PD's
    /// membership is static ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)), so today
    /// this never changes after `open` — the field is here because a log that compacts must write
    /// it, and because the day membership stops being static is not the day to discover it was
    /// missing.
    pub conf_state: ConfState,
    /// The highest index the state machine has applied, written with the data it applied.
    pub applied_index: Index,
    /// The last index folded into a snapshot; `0` when nothing has been compacted.
    pub truncated_index: Index,
    /// The term of the entry at `truncated_index`.
    pub truncated_term: Term,
    /// What identifies this group's traffic, minted once and never recomputed
    /// ([ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)).
    ///
    /// **Zero means "not yet minted"**, which is both a fresh database and a record written before
    /// there was such a thing. Zero is not a group id anywhere in this codebase — `derived_group_id`
    /// refuses to return it — so it is free to carry that meaning.
    pub group_id: u64,
    /// Where every member of the group is.
    ///
    /// Here rather than in the state machine because it answers the same question the two fields
    /// above answer: what does this member need **before it has applied anything**. A member that
    /// is catching up has no state machine to read, and it cannot catch up without reaching the
    /// group.
    pub members: Vec<PdMember>,
}

impl PersistedState {
    /// The record's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::new();
        out.put_u8(STATE_FORMAT_VERSION);
        out.put_varint(self.hard_state.term);
        // A vote is optional and `0` is not a usable sentinel, because node ids are the caller's
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
        // Version 2 appends from here. Everything above is byte-identical to version 1.
        out.put_u64(self.group_id);
        out.put_varint(self.members.len() as u64);
        for member in &self.members {
            out.put_varint(member.id);
            out.put_str(&member.address);
        }
        out.finish()
    }

    /// Reads a record written by [`PersistedState::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut input = Decoder::new(bytes);
        let field =
            |error: esker_proto::DecodeError| PdError::corrupt("raft state", error.to_string());

        let version = input.get_u8("state.version").map_err(field)?;
        if version != STATE_FORMAT_VERSION && version != STATE_FORMAT_VERSION_V1 {
            return Err(PdError::corrupt(
                "raft state",
                format!(
                    "format version {version}, expected {STATE_FORMAT_VERSION_V1} or {STATE_FORMAT_VERSION}"
                ),
            ));
        }
        let term = input.get_varint("state.term").map_err(field)?;
        let voted_for = match input.get_u8("state.vote_flag").map_err(field)? {
            0 => None,
            1 => Some(input.get_varint("state.voted_for").map_err(field)?),
            other => {
                return Err(PdError::corrupt(
                    "raft state",
                    format!("vote flag {other}, expected 0 or 1"),
                ));
            }
        };
        let commit = input.get_varint("state.commit").map_err(field)?;
        let applied_index = input.get_varint("state.applied").map_err(field)?;
        let truncated_index = input.get_varint("state.truncated_index").map_err(field)?;
        let truncated_term = input.get_varint("state.truncated_term").map_err(field)?;
        let voters = get_ids(&mut input, "state.voters")?;
        let learners = get_ids(&mut input, "state.learners")?;
        // A version-1 record ends here, and reads as a group that has not minted an id and knows
        // nowhere to send. Both are true of it.
        let (group_id, members) = if version == STATE_FORMAT_VERSION_V1 {
            (0, Vec::new())
        } else {
            let group_id = input.get_u64("state.group_id").map_err(field)?;
            let count = input.get_count("state.members").map_err(field)?;
            let mut members = Vec::with_capacity(count.min(MAX_MEMBERS));
            for _ in 0..count {
                members.push(PdMember::new(
                    input.get_varint("state.member.id").map_err(field)?,
                    input.get_str("state.member.address").map_err(field)?,
                ));
            }
            (group_id, members)
        };
        input.finish().map_err(field)?;

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
            group_id,
            members,
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
    let corrupt =
        |error: esker_proto::DecodeError| PdError::corrupt("raft state", error.to_string());
    let count = input.get_count(field).map_err(corrupt)?;
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        ids.push(input.get_varint(field).map_err(corrupt)?);
    }
    Ok(ids)
}

/// PD's Raft log over one `esker-engine` database.
#[derive(Debug)]
pub struct PdLogStorage {
    db: Arc<Db>,
    cf: u32,
    state: PersistedState,
    /// The highest index the log holds. `truncated_index` when the log is empty.
    last_index: Index,
    /// The snapshot a leader would offer, built by [`PdLogStorage::stage_compact`] and held here
    /// because the core asks for it synchronously and may not wait for I/O.
    ///
    /// `None` on a log that has never compacted, which is [`LogStorage::snapshot`]'s "empty
    /// snapshot" case.
    snapshot: Option<Snapshot>,
}

impl PdLogStorage {
    /// Opens PD's log, bootstrapping it with `conf_state` if it is not there yet.
    ///
    /// Bootstrapping writes the state record over an empty log, which is what makes a fresh
    /// member distinguishable from one whose state record was lost.
    pub fn open(db: Arc<Db>, conf_state: ConfState) -> Result<Self> {
        let cf = db
            .cf_id(cf::RAFT)
            .ok_or_else(|| PdError::internal("the `raft` column family is missing after open"))?;

        let stored = db.get(cf::RAFT, &state_key(), &ReadOptions::default())?;
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
            batch.put(cf, &state_key(), &fresh.encode());
            db.write(batch, &WriteOptions::synced())?;
            fresh
        };

        let mut storage = Self {
            db,
            cf,
            state,
            last_index: 0,
            snapshot: None,
        };
        storage.last_index = storage.scan_last_index()?;
        Ok(storage)
    }

    /// The last index actually present, found by seeking past the end of the entries.
    ///
    /// Done once, at open; afterwards the driver's appends keep it current, which is why the
    /// bounds can live in the struct at all.
    fn scan_last_index(&self) -> Result<Index> {
        let mut iter = self.db.iter(cf::RAFT, &ReadOptions::default())?;
        iter.seek_for_prev(&log_entry_key(Index::MAX));
        if iter.valid() {
            let key = iter.key();
            if key.len() == LOG_KEY_LEN && key[0] == LOG_ENTRY {
                let mut index = [0_u8; 8];
                index.copy_from_slice(&key[1..]);
                return Ok(u64::from_be_bytes(index));
            }
        }
        iter.status()?;
        Ok(self.state.truncated_index)
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

    /// The index the state machine has applied through.
    #[must_use]
    pub fn applied_index(&self) -> Index {
        self.state.applied_index
    }

    /// The index the log begins after.
    #[must_use]
    pub fn truncated_index(&self) -> Index {
        self.state.truncated_index
    }

    /// What identifies this group's traffic, or **zero** if it has not been minted yet.
    #[must_use]
    pub fn group_id(&self) -> u64 {
        self.state.group_id
    }

    /// Where every member of the group is, as this member last recorded it.
    #[must_use]
    pub fn members(&self) -> &[PdMember] {
        &self.state.members
    }

    /// Records the group's id and its address book, staging the write into `batch`.
    ///
    /// Two callers, and they are the two moments a member learns who it is with: `Pd::open`, which
    /// mints the id for a founding group or writes the one a joining member was told, and the
    /// driver, which learns an address from a conf change **at append** — before that `Ready`'s
    /// messages go out, because a configuration is in force from the moment its entry is on disk.
    ///
    /// The id is refused if it would **change**: it is minted once and never recomputed
    /// ([ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)),
    /// and a member that quietly adopted a different one would have rejoined a different group.
    pub fn stage_group(
        &mut self,
        batch: &mut WriteBatch,
        group_id: u64,
        members: &[PdMember],
    ) -> Result<()> {
        if self.state.group_id != 0 && group_id != 0 && group_id != self.state.group_id {
            return Err(PdError::invalid(format!(
                "this placement driver belongs to group {:#018x} and was asked to join \
                 {group_id:#018x}; a group id is minted once and never changes",
                self.state.group_id
            )));
        }
        if group_id != 0 {
            self.state.group_id = group_id;
        }
        self.state.members = members.to_vec();
        self.stage_state(batch);
        Ok(())
    }

    /// Adds the state record to `batch`, in full.
    ///
    /// Every writer of `'s'` goes through this, so the record on disk always carries the latest
    /// hard state *and* the latest apply index rather than whichever the last writer knew about.
    pub fn stage_state(&self, batch: &mut WriteBatch) {
        batch.put(self.cf, &state_key(), &self.state.encode());
    }

    /// Stages the persist half of a `Ready`: the new entries and the hard state.
    ///
    /// Entries these replace are deleted in the same batch. A member whose tail is being rewritten
    /// must not be left holding the old suffix — a later read would find entries past the new end
    /// and the log would look longer than it is.
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
                batch.delete(self.cf, &log_entry_key(index));
            }
            for entry in entries {
                batch.put(self.cf, &log_entry_key(entry.index), &encode_entry(entry));
            }
            self.last_index = entries[entries.len() - 1].index;
        }
        self.stage_state(batch);
    }

    /// Records that the state machine has applied through `index`.
    ///
    /// The caller puts this in the **same** batch as the data it applied, which is the whole
    /// reason PD's records and PD's log share one database: "applied" is then one fact on disk
    /// rather than two that can disagree after a crash.
    pub fn stage_applied(&mut self, batch: &mut WriteBatch, index: Index) {
        self.state.applied_index = self.state.applied_index.max(index);
        self.stage_state(batch);
    }

    /// Throws away every entry at or below `index`, and records the snapshot that replaces them.
    ///
    /// `term` is the term of the entry *at* `index`; `data` is the state machine's contents as of
    /// it. Compacting past what the state machine has applied would be a lie — the log would claim
    /// a state the data does not hold — so it is refused rather than clamped.
    pub fn stage_compact(
        &mut self,
        batch: &mut WriteBatch,
        index: Index,
        term: Term,
        data: Bytes,
    ) -> Result<()> {
        if index > self.state.applied_index {
            return Err(PdError::internal(format!(
                "compaction to {index} is past the applied index {}",
                self.state.applied_index
            )));
        }
        if index <= self.state.truncated_index {
            return Ok(());
        }
        for old in self.state.truncated_index + 1..=index {
            batch.delete(self.cf, &log_entry_key(old));
        }
        self.state.truncated_index = index;
        self.state.truncated_term = term;
        self.snapshot = Some(Snapshot {
            meta: SnapshotMeta {
                index,
                term,
                conf: self.state.conf_state.clone(),
            },
            data,
        });
        self.stage_state(batch);
        Ok(())
    }

    /// Installs a snapshot's log position: the entries it replaces go, and the bounds move to it.
    ///
    /// The caller stages the state machine's own contents into the **same** batch, so that the log
    /// position and the data it stands for become durable together. A snapshot at or below what is
    /// already truncated is stale and changes nothing — the core refuses those too, and refusing
    /// twice is cheaper than reasoning about which one saw it first.
    pub fn stage_snapshot(&mut self, batch: &mut WriteBatch, snapshot: &Snapshot) {
        let index = snapshot.meta.index;
        if index <= self.state.truncated_index {
            return;
        }
        // Every entry the log still holds. A snapshot may be ahead of `last_index` — that is the
        // whole reason a lagging member is sent one — so the range is cleared rather than trusted
        // to end where this member's log does.
        for old in self.state.truncated_index + 1..=self.last_index {
            batch.delete(self.cf, &log_entry_key(old));
        }
        self.state.truncated_index = index;
        self.state.truncated_term = snapshot.meta.term;
        self.state.conf_state = snapshot.meta.conf.clone();
        self.state.applied_index = self.state.applied_index.max(index);
        self.state.hard_state.commit = self.state.hard_state.commit.max(index);
        self.last_index = self.last_index.max(index);
        self.snapshot = Some(snapshot.clone());
        self.stage_state(batch);
    }

    fn read_entry(&self, index: Index) -> std::result::Result<Option<Entry>, RaftError> {
        let bytes = self
            .db
            .get(cf::RAFT, &log_entry_key(index), &ReadOptions::default())
            .map_err(|error| RaftError::Storage(error.to_string()))?;
        match bytes {
            None => Ok(None),
            Some(bytes) => decode_entry(index, &bytes)
                .map(Some)
                .map_err(|error| RaftError::Storage(error.to_string())),
        }
    }
}

impl LogStorage for PdLogStorage {
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
            return Err(RaftError::Compacted(low));
        }
        if high > self.last_index + 1 {
            return Err(RaftError::Unavailable(high));
        }
        let mut out = Vec::new();
        let mut budget = 0_u64;
        for index in low..high {
            let entry = self
                .read_entry(index)?
                .ok_or(RaftError::Unavailable(index))?;
            budget = budget.saturating_add(entry.cost());
            // At least one entry always comes back: a budget that could answer nothing would
            // stall replication on the first oversized proposal.
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
            return Err(RaftError::Compacted(index));
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
        Ok(self.snapshot.clone().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PdLogStorage, PersistedState, decode_entry, encode_entry, log_entry_key, state_key,
    };
    use crate::member::PdMember;
    use bytes::Bytes;
    use esker_engine::{Db, Options, WalSyncMode, WriteBatch, WriteOptions, cf};
    use esker_proto::Encoder;
    use esker_raft::{ConfState, Entry, EntryKind, HardState, LogStorage};
    use std::sync::Arc;

    fn open_db() -> (tempfile::TempDir, Arc<Db>) {
        let dir = tempfile::tempdir().unwrap();
        let options = Options {
            create_if_missing: true,
            wal_sync_mode: WalSyncMode::Never,
            ..Options::default()
        };
        let db = Db::open_with(
            dir.path(),
            options,
            Arc::new(esker_engine::LocalFileSystem::new()),
            &[cf::DEFAULT, cf::RAFT],
        )
        .unwrap();
        (dir, Arc::new(db))
    }

    fn open_log() -> (tempfile::TempDir, Arc<Db>, PdLogStorage) {
        let (dir, db) = open_db();
        let log = PdLogStorage::open(Arc::clone(&db), ConfState::from_voters(vec![1])).unwrap();
        (dir, db, log)
    }

    fn entries(spec: &[(u64, u64)]) -> Vec<Entry> {
        spec.iter()
            .map(|(term, index)| Entry {
                term: *term,
                index: *index,
                kind: EntryKind::Normal,
                data: Bytes::from_static(b"x"),
            })
            .collect()
    }

    fn append(db: &Db, log: &mut PdLogStorage, entries: &[Entry], hard: Option<HardState>) {
        let mut batch = WriteBatch::new();
        log.stage_ready(&mut batch, hard, entries);
        db.write(batch, &WriteOptions::synced()).unwrap();
    }

    /// The key layout is a format. A drift here does not fail to decode — it silently stops
    /// finding what is already on disk.
    #[test]
    fn the_keys_encode_to_the_documented_bytes() {
        assert_eq!(state_key(), [b's']);
        assert_eq!(log_entry_key(1), [b'l', 0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            log_entry_key(u64::MAX),
            [b'l', 255, 255, 255, 255, 255, 255, 255, 255]
        );
    }

    /// Big-endian, so a scan runs in index order rather than in byte-swapped order.
    #[test]
    fn log_keys_sort_by_index() {
        let mut keys: Vec<_> = [300_u64, 1, 2, 256, 257]
            .into_iter()
            .map(log_entry_key)
            .collect();
        keys.sort_unstable();
        let indices: Vec<u64> = keys
            .iter()
            .map(|key| {
                let mut index = [0_u8; 8];
                index.copy_from_slice(&key[1..]);
                u64::from_be_bytes(index)
            })
            .collect();
        assert_eq!(indices, [1, 2, 256, 257, 300]);
    }

    #[test]
    fn an_entry_round_trips_through_its_value_encoding() {
        for kind in [EntryKind::Normal, EntryKind::ConfChange] {
            let entry = Entry {
                term: 7,
                index: 9,
                kind,
                data: Bytes::from_static(b"payload"),
            };
            assert_eq!(decode_entry(9, &encode_entry(&entry)).unwrap(), entry);
        }
    }

    /// The index in the value is a consistency check, not redundancy: a value read under the
    /// wrong key must be an error rather than an entry claiming to be somewhere it is not.
    #[test]
    fn an_entry_read_under_the_wrong_index_is_corruption() {
        let entry = Entry {
            term: 1,
            index: 4,
            kind: EntryKind::Normal,
            data: Bytes::new(),
        };
        assert!(decode_entry(5, &encode_entry(&entry)).is_err());
    }

    #[test]
    fn the_state_record_round_trips() {
        let state = PersistedState {
            hard_state: HardState {
                term: 3,
                voted_for: Some(2),
                commit: 11,
            },
            conf_state: ConfState::from_voters(vec![1, 2, 3]),
            applied_index: 10,
            truncated_index: 4,
            truncated_term: 2,
            group_id: 0x0123_4567_89AB_CDEF,
            members: vec![
                PdMember::new(1, "127.0.0.1:2379"),
                PdMember::new(2, "127.0.0.1:2380"),
            ],
        };
        assert_eq!(PersistedState::decode(&state.encode()).unwrap(), state);

        // And the "no vote" shape, which is a different byte and a different branch.
        let unvoted = PersistedState {
            hard_state: HardState {
                voted_for: None,
                ..state.hard_state
            },
            ..state.clone()
        };
        assert_eq!(PersistedState::decode(&unvoted.encode()).unwrap(), unvoted);

        // And the shape a group that has not minted an id is in.
        let unnamed = PersistedState {
            group_id: 0,
            members: Vec::new(),
            ..state
        };
        assert_eq!(PersistedState::decode(&unnamed.encode()).unwrap(), unnamed);
    }

    /// **A record written before there were group ids still reads**, and reads as what it is: a
    /// member that has not minted one and knows nowhere to send. Everything version 1 wrote is
    /// byte-identical, which is what makes this an appended field rather than a migration
    /// ([ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)).
    #[test]
    fn a_version_one_record_reads_as_a_group_that_has_not_been_named() {
        let state = PersistedState {
            hard_state: HardState {
                term: 3,
                voted_for: Some(2),
                commit: 11,
            },
            conf_state: ConfState::from_voters(vec![1, 2, 3]),
            applied_index: 10,
            truncated_index: 4,
            truncated_term: 2,
            group_id: 7,
            members: vec![PdMember::new(1, "127.0.0.1:2379")],
        };
        // Version 1's bytes are version 2's, minus the two appended fields — which is the claim
        // being made, so it is built by truncation rather than by a second encoder agreeing.
        let v2 = state.encode();
        let appended = {
            let mut out = Encoder::new();
            out.put_u64(state.group_id);
            out.put_varint(1);
            out.put_varint(1);
            out.put_str("127.0.0.1:2379");
            out.finish()
        };
        let mut v1 = v2[..v2.len() - appended.len()].to_vec();
        assert_eq!(&v2[v2.len() - appended.len()..], &appended[..]);
        v1[0] = 1;

        let read = PersistedState::decode(&v1).unwrap();
        assert_eq!(read.hard_state, state.hard_state);
        assert_eq!(read.conf_state, state.conf_state);
        assert_eq!(read.applied_index, 10);
        assert_eq!(read.group_id, 0, "a version-1 record named a group");
        assert!(read.members.is_empty());

        // And the next write rewrites it as version 2, so the branch is taken once per member.
        assert_eq!(read.encode()[0], super::STATE_FORMAT_VERSION);
    }

    #[test]
    fn a_corrupt_state_record_is_an_error_and_never_a_panic() {
        assert!(PersistedState::decode(&[]).is_err());
        assert!(PersistedState::decode(&[99]).is_err());
        let mut trailing = PersistedState::default().encode();
        trailing.push(0);
        assert!(PersistedState::decode(&trailing).is_err());
    }

    #[test]
    fn a_fresh_log_starts_at_one_and_ends_at_zero() {
        let (_dir, _db, log) = open_log();
        assert_eq!(log.first_index().unwrap(), 1);
        assert_eq!(log.last_index().unwrap(), 0);
        assert!(log.snapshot().unwrap().is_empty());
        assert_eq!(
            log.initial_state().unwrap().conf_state,
            ConfState::from_voters(vec![1])
        );
    }

    #[test]
    fn appended_entries_come_back_by_index_and_term() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2), (2, 3)]), None);
        assert_eq!(log.last_index().unwrap(), 3);
        assert_eq!(log.term(3).unwrap(), 2);
        assert_eq!(log.entries(1, 4, u64::MAX).unwrap().len(), 3);
    }

    /// A rewritten tail must remove what it replaced, or a later read finds entries past the
    /// new end and the log looks longer than it is.
    #[test]
    fn a_rewritten_tail_removes_the_entries_it_replaced() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2), (1, 3)]), None);
        append(&db, &mut log, &entries(&[(2, 2)]), None);
        assert_eq!(log.last_index().unwrap(), 2);
        assert_eq!(log.term(2).unwrap(), 2);
        assert!(log.entries(1, 4, u64::MAX).is_err(), "index 3 is gone");
    }

    /// A budget that could answer nothing would stall replication on the first oversized entry.
    #[test]
    fn a_byte_budget_always_yields_at_least_one_entry() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2)]), None);
        assert_eq!(log.entries(1, 3, 0).unwrap().len(), 1);
    }

    /// Both writers of `'s'` emit the whole record, so neither clobbers the other's field.
    #[test]
    fn the_state_record_keeps_the_latest_of_both_fields() {
        let (_dir, db, mut log) = open_log();
        append(
            &db,
            &mut log,
            &entries(&[(1, 1)]),
            Some(HardState {
                term: 4,
                voted_for: Some(1),
                commit: 1,
            }),
        );
        let mut batch = WriteBatch::new();
        log.stage_applied(&mut batch, 1);
        db.write(batch, &WriteOptions::synced()).unwrap();

        let reopened = PdLogStorage::open(Arc::clone(&db), ConfState::default()).unwrap();
        assert_eq!(reopened.state().hard_state.term, 4);
        assert_eq!(reopened.state().applied_index, 1);
        assert_eq!(reopened.last_index().unwrap(), 1);
    }

    /// A restart takes its membership from the record, never from the configuration it is
    /// handed: a member list on a command line may be out of date.
    #[test]
    fn reopening_keeps_the_membership_it_was_bootstrapped_with() {
        let (_dir, db, log) = open_log();
        drop(log);
        let reopened =
            PdLogStorage::open(Arc::clone(&db), ConfState::from_voters(vec![7, 8, 9])).unwrap();
        assert_eq!(
            reopened.initial_state().unwrap().conf_state,
            ConfState::from_voters(vec![1])
        );
    }

    #[test]
    fn compaction_moves_the_bounds_and_offers_the_snapshot() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2), (1, 3)]), None);
        let mut batch = WriteBatch::new();
        log.stage_applied(&mut batch, 3);
        log.stage_compact(&mut batch, 2, 1, Bytes::from_static(b"state"))
            .unwrap();
        db.write(batch, &WriteOptions::synced()).unwrap();

        assert_eq!(log.first_index().unwrap(), 3);
        assert_eq!(log.term(2).unwrap(), 1, "the boundary term survives");
        assert!(log.entries(1, 3, u64::MAX).is_err());
        let snapshot = log.snapshot().unwrap();
        assert_eq!(snapshot.meta.index, 2);
        assert_eq!(snapshot.data, Bytes::from_static(b"state"));

        let reopened = PdLogStorage::open(Arc::clone(&db), ConfState::default()).unwrap();
        assert_eq!(reopened.first_index().unwrap(), 3);
        assert_eq!(reopened.last_index().unwrap(), 3);
    }

    /// A snapshot replaces the log rather than joining it: the entries it covers go, the bounds
    /// move to it, and a member that was behind the boundary is no longer holding a position it
    /// has no data for.
    #[test]
    fn an_installed_snapshot_replaces_the_log_and_moves_the_bounds() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2)]), None);
        let snapshot = esker_raft::Snapshot {
            meta: esker_raft::SnapshotMeta {
                index: 9,
                term: 3,
                conf: ConfState::from_voters(vec![1, 2, 3]),
            },
            data: Bytes::from_static(b"state"),
        };
        let mut batch = WriteBatch::new();
        log.stage_snapshot(&mut batch, &snapshot);
        db.write(batch, &WriteOptions::synced()).unwrap();

        assert_eq!(log.first_index().unwrap(), 10);
        assert_eq!(log.last_index().unwrap(), 9);
        assert_eq!(log.term(9).unwrap(), 3);
        assert_eq!(log.applied_index(), 9);
        assert!(log.entries(1, 3, u64::MAX).is_err(), "the old log is gone");

        let reopened = PdLogStorage::open(Arc::clone(&db), ConfState::default()).unwrap();
        assert_eq!(reopened.first_index().unwrap(), 10);
        assert_eq!(
            reopened.initial_state().unwrap().conf_state,
            ConfState::from_voters(vec![1, 2, 3]),
            "the membership travels with the snapshot"
        );
    }

    /// A snapshot at or below the boundary is stale, and rewinding to it would throw away
    /// entries the member has already applied.
    #[test]
    fn a_stale_snapshot_changes_nothing() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2), (1, 3)]), None);
        let mut batch = WriteBatch::new();
        log.stage_applied(&mut batch, 3);
        log.stage_compact(&mut batch, 3, 1, Bytes::new()).unwrap();
        db.write(batch, &WriteOptions::synced()).unwrap();

        let mut batch = WriteBatch::new();
        log.stage_snapshot(
            &mut batch,
            &esker_raft::Snapshot {
                meta: esker_raft::SnapshotMeta {
                    index: 2,
                    term: 1,
                    conf: ConfState::default(),
                },
                data: Bytes::new(),
            },
        );
        assert!(batch.is_empty(), "a stale snapshot staged a write");
        assert_eq!(log.truncated_index(), 3);
    }

    /// Compacting past what the state machine has applied would claim a state the data does not
    /// hold. Refused, not clamped.
    #[test]
    fn compaction_never_runs_past_the_apply_index() {
        let (_dir, db, mut log) = open_log();
        append(&db, &mut log, &entries(&[(1, 1), (1, 2)]), None);
        let mut batch = WriteBatch::new();
        assert!(
            log.stage_compact(&mut batch, 2, 1, Bytes::new()).is_err(),
            "nothing has applied"
        );
    }
}
