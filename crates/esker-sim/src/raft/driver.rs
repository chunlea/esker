//! The persistence boundary: what survives a kill, and one node's half of the `Ready` contract.
//!
//! `docs/DESIGN.md` §5 states the contract the store must follow and the simulator must test:
//! persist `hard_state` and `entries` — with fsync — **before** sending any message from the
//! same `Ready`. This module is where that order is enforced, and where it can be broken on
//! purpose.
//!
//! # Why a wrapper around `MemStorage`
//!
//! [`PersistedStorage`] holds only what has been made durable. The entries a node has accepted
//! but not yet written live in the core's own unstable log and in the [`DiskWrite`] the driver
//! is holding — both of which die with the process. So a crash is modelled exactly: take the
//! durable storage, throw everything else away, and build a new node over it.
//!
//! Get this wrong — let a node read back something it never fsynced — and every safety property
//! passes vacuously, because the failure the whole exercise is about can no longer happen.

use esker_raft::{
    Entry, HardState, Index, LogStorage, MemStorage, RawNode, Ready, Result as RaftResult,
    Snapshot, Term,
};

use super::checkers::EntryDigest;

/// A [`LogStorage`] that contains exactly what has been fsynced.
///
/// Every write goes through [`PersistedStorage::persist`], which is the fsync: before it
/// returns nothing is durable, and after it returns everything in that `Ready` is.
#[derive(Debug, Clone, Default)]
pub struct PersistedStorage {
    durable: MemStorage,
    writes: u64,
    entries_written: u64,
    compactions: u64,
}

impl PersistedStorage {
    /// An empty storage with `conf` as its bootstrap membership.
    #[must_use]
    pub fn new(conf: esker_raft::ConfState) -> Self {
        Self {
            durable: MemStorage::with_conf_state(conf),
            ..Self::default()
        }
    }

    /// A storage over bytes that were already durable — how a restart rebuilds.
    #[must_use]
    pub fn from_durable(durable: MemStorage) -> Self {
        Self {
            durable,
            ..Self::default()
        }
    }

    /// The bytes that would survive a kill at this instant.
    #[must_use]
    pub fn durable(&self) -> &MemStorage {
        &self.durable
    }

    /// Makes one `Ready`'s state durable. This call *is* the fsync.
    ///
    /// The order within the write does not matter — a real WAL writes them as one record — but
    /// the order relative to sending does, and that is the driver's business, not this type's.
    pub fn persist(&mut self, ready: &Ready) -> RaftResult<()> {
        if let Some(snapshot) = &ready.snapshot {
            self.durable.apply_snapshot(snapshot.clone())?;
        }
        if !ready.entries.is_empty() {
            self.durable.append(&ready.entries)?;
            self.entries_written += ready.entries.len() as u64;
        }
        if let Some(hard_state) = ready.hard_state {
            self.durable.set_hard_state(hard_state);
        }
        self.writes += 1;
        Ok(())
    }

    /// Discards log entries at or below `to_index`, which the state machine has applied and a
    /// snapshot now covers.
    ///
    /// This is the store's compaction, modelled as durable the moment it happens: a real store
    /// deletes the entries only after the snapshot they are folded into is on disk, so a crash
    /// either finds the entries or finds the snapshot, never neither.
    pub fn compact(&mut self, to_index: Index) -> RaftResult<()> {
        self.durable.compact(to_index)?;
        self.compactions += 1;
        Ok(())
    }

    /// How many times the log has been compacted.
    #[must_use]
    pub fn compactions(&self) -> u64 {
        self.compactions
    }

    /// How many `Ready`s have been made durable.
    #[must_use]
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// How many entries have been made durable, counting a re-written index twice.
    #[must_use]
    pub fn entries_written(&self) -> u64 {
        self.entries_written
    }
}

impl LogStorage for PersistedStorage {
    fn initial_state(&self) -> RaftResult<esker_raft::InitialState> {
        self.durable.initial_state()
    }

    fn entries(&self, low: Index, high: Index, max_bytes: u64) -> RaftResult<Vec<Entry>> {
        self.durable.entries(low, high, max_bytes)
    }

    fn term(&self, index: Index) -> RaftResult<Term> {
        self.durable.term(index)
    }

    fn first_index(&self) -> RaftResult<Index> {
        self.durable.first_index()
    }

    fn last_index(&self) -> RaftResult<Index> {
        self.durable.last_index()
    }

    fn snapshot(&self) -> RaftResult<Snapshot> {
        self.durable.snapshot()
    }
}

/// One `Ready` handed to the disk and not yet durable.
///
/// Nothing in it has happened: no message has been sent, no entry applied, and the core has
/// not been told to move on. That is the whole point — a slow disk stalls everything the
/// `Ready` contained, which is what makes a crash during the write interesting.
#[derive(Debug, Clone)]
pub struct DiskWrite {
    /// What the core asked for.
    pub ready: Ready,
    /// The event at which the write completes.
    pub due: u64,
    /// How many events the disk was told to hold it for; `0` is a fast disk.
    pub held: u64,
    /// Whether its messages were sent before the write completed. Only ever true when the
    /// driver was told to violate the contract on purpose.
    pub sent_early: bool,
}

/// One node in the simulated cluster: the core, its disk, and its state machine.
#[derive(Debug)]
pub struct NodeSlot {
    /// Which node.
    pub id: esker_raft::NodeId,
    /// The core, or `None` while the node is dead.
    pub node: Option<RawNode<PersistedStorage>>,
    /// The durable bytes, held here while the node is dead so a restart can be built from
    /// exactly them.
    pub durable: Option<MemStorage>,
    /// The write the disk is chewing on.
    pub pending: Option<DiskWrite>,
    /// Everything the state machine has consumed, in order. Durable: a real store writes the
    /// applied index atomically with the data (`docs/DESIGN.md` §6), so a restart does not
    /// replay what it already applied.
    pub applied: Vec<EntryDigest>,
    /// The index it has applied up to.
    pub applied_index: Index,
    /// The node's log as the checkers see it: everything durable, plus whatever is on its way
    /// to the disk right now.
    pub log: Vec<EntryDigest>,
    /// How many times it has been restarted, so its RNG stream differs each life.
    pub restarts: u64,
    /// The last index its snapshot covers.
    pub compacted_through: Index,
    /// The term of the entry at [`NodeSlot::compacted_through`], from the snapshot's metadata.
    pub snapshot_term: Term,
}

impl NodeSlot {
    /// A live node.
    pub fn new(id: esker_raft::NodeId, node: RawNode<PersistedStorage>) -> Self {
        let mut slot = Self {
            id,
            node: Some(node),
            durable: None,
            pending: None,
            applied: Vec::new(),
            applied_index: 0,
            log: Vec::new(),
            restarts: 0,
            compacted_through: 0,
            snapshot_term: 0,
        };
        slot.refresh();
        slot
    }

    /// Whether the node is running.
    #[must_use]
    pub fn online(&self) -> bool {
        self.node.is_some()
    }

    /// The term it is in — from the core while it lives, from the durable `HardState` while it
    /// is dead, because that is what a restart would find.
    #[must_use]
    pub fn term(&self) -> Term {
        self.node.as_ref().map_or_else(
            || {
                self.durable
                    .as_ref()
                    .map_or(0, |durable| durable.hard_state().term)
            },
            RawNode::term,
        )
    }

    /// Its commit index, under the same rule.
    #[must_use]
    pub fn commit(&self) -> Index {
        self.node.as_ref().map_or_else(
            || {
                self.durable
                    .as_ref()
                    .map_or(0, |durable| durable.hard_state().commit)
            },
            RawNode::commit_index,
        )
    }

    /// Whether it believes it leads its term. A dead node leads nothing.
    #[must_use]
    pub fn is_leader(&self) -> bool {
        self.node
            .as_ref()
            .is_some_and(|node| node.role() == esker_raft::Role::Leader)
    }

    /// Whether everything the core has decided is visible in [`NodeSlot::log`].
    ///
    /// True when no write is outstanding *and* the core has nothing left to hand over: then its
    /// unstable tail is empty and the durable log is the whole log. While a slow disk holds a
    /// write, the core keeps accepting entries that no public accessor exposes, and a checker
    /// that read the commit index against a log missing them would report a violation that is
    /// not there. (A `RawNode` accessor for the log including its unstable tail would close the
    /// window entirely; noted for the core lane.)
    #[must_use]
    pub fn settled(&self) -> bool {
        self.pending.is_none() && self.node.as_ref().is_none_or(|node| !node.has_ready())
    }

    /// Kills the node: the core, its unstable log and any write still on its way to the disk
    /// all vanish. What is left is what was fsynced.
    pub fn crash(&mut self) {
        let durable = match self.node.take() {
            Some(node) => node.storage().durable().clone(),
            None => return,
        };
        self.pending = None;
        self.durable = Some(durable);
        self.refresh();
    }

    /// Takes the durable bytes to build a replacement core from.
    pub fn take_durable(&mut self) -> Option<MemStorage> {
        self.durable.take()
    }

    /// Installs a replacement core after a restart.
    pub fn revive(&mut self, node: RawNode<PersistedStorage>) {
        self.node = Some(node);
        self.durable = None;
        self.restarts += 1;
        self.refresh();
    }

    /// Records that the state machine adopted a snapshot: everything at or below
    /// `through` is now in its state, however it got there.
    ///
    /// The entries it already applied stay in [`NodeSlot::applied`] — they are still what it
    /// did — and the checker allows the gap the snapshot covers.
    pub fn install_snapshot(&mut self, through: Index) {
        self.applied_index = self.applied_index.max(through);
    }

    /// Records that the state machine consumed `entries`.
    pub fn apply(&mut self, entries: &[Entry]) {
        for entry in entries {
            if entry.index <= self.applied_index {
                continue;
            }
            self.applied
                .push(EntryDigest::of(entry.index, entry.term, &entry.data));
            self.applied_index = entry.index;
        }
    }

    /// Rebuilds the log the checkers see: everything durable, plus the write in flight.
    ///
    /// The in-flight write is included because those entries really are in the node's log — the
    /// core can read them out of its unstable tail — even though a crash would lose them. What
    /// is *not* included is anything the core has appended since it last offered a `Ready`,
    /// which no accessor exposes and which will appear the moment it does.
    pub fn refresh(&mut self) {
        self.log.clear();
        let storage: Option<&MemStorage> = match (&self.node, &self.durable) {
            (Some(node), _) => Some(node.storage().durable()),
            (None, Some(durable)) => Some(durable),
            (None, None) => None,
        };
        let Some(storage) = storage else {
            self.compacted_through = 0;
            self.snapshot_term = 0;
            return;
        };
        let first = storage.first_index().unwrap_or(1);
        let last = storage.last_index().unwrap_or(0);
        self.compacted_through = first.saturating_sub(1);
        self.snapshot_term = storage
            .snapshot()
            .map(|snapshot| snapshot.meta.term)
            .unwrap_or_default();
        if last >= first
            && let Ok(entries) = storage.entries(first, last + 1, u64::MAX)
        {
            self.log.extend(
                entries
                    .iter()
                    .map(|entry| EntryDigest::of(entry.index, entry.term, &entry.data)),
            );
        }
        if let Some(write) = &self.pending {
            for entry in &write.ready.entries {
                if entry.index <= self.compacted_through {
                    continue;
                }
                let at = usize::try_from(entry.index - self.compacted_through - 1).unwrap_or(0);
                self.log.truncate(at);
                self.log
                    .push(EntryDigest::of(entry.index, entry.term, &entry.data));
            }
        }
    }

    /// The `HardState` on disk right now, for a report.
    #[must_use]
    pub fn durable_hard_state(&self) -> HardState {
        match (&self.node, &self.durable) {
            (Some(node), _) => node.storage().durable().hard_state(),
            (None, Some(durable)) => durable.hard_state(),
            (None, None) => HardState::default(),
        }
    }
}
