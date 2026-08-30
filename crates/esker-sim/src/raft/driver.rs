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
    ConfChange, ConfState, Entry, EntryKind, HardState, Index, LogStorage, MemStorage, NodeId,
    RawNode, Ready, Result as RaftResult, Snapshot, Term,
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
    pub fn new(conf: ConfState) -> Self {
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

    /// Records the membership alongside the log, which is what makes it survive a restart:
    /// `RawNode::new` reads the configuration out of storage and does not replay the log's
    /// conf-change entries for itself.
    pub fn set_conf_state(&mut self, conf: ConfState) {
        self.durable.set_conf_state(conf);
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
    pub id: NodeId,
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
    /// The membership this node started with, and the base every recomputation starts from.
    pub bootstrap: ConfState,
    /// The membership the *driver* has derived, independently of the core's own tracker.
    ///
    /// A real store keeps this: `RawNode::new` reads the configuration out of storage and does
    /// not replay the log's conf-change entries, so a driver that does not persist it loses
    /// every membership change across a restart (`docs/DESIGN.md` §5, dissertation §4.1). Two
    /// independent derivations of one fact is also a check worth having, and the checkers make
    /// it one.
    pub config: ConfState,
    /// The index of the last conf-change entry folded into [`NodeSlot::config`].
    pub config_index: Index,
    /// The configuration in force before the first entry of [`NodeSlot::lineage`]: the
    /// snapshot's, when the log starts after one, and the bootstrap otherwise.
    pub base_config: ConfState,
    /// The configuration after each conf-change entry in this node's log, in log order, each
    /// beside the entry that produced it.
    ///
    /// Reported whole rather than as a running count, because the single-server rule is about
    /// *adjacent configurations in one log*, and a node's log can be truncated and rebuilt
    /// between two observations. Comparing across observations — or across nodes — compares
    /// across branches, where two configurations really can differ by more than one server
    /// without anything being wrong. Within this vector there are no branches.
    pub lineage: Vec<(EntryDigest, ConfState)>,
    /// The configuration a restart would recover: derived from the *durable* log alone, and
    /// written back to storage so it really is what a restart finds.
    pub durable_config: ConfState,
    /// How the driver derives the configuration. Ordinarily faithfully.
    pub conf_fault: ConfFault,
    /// Whether this node has ever run. A spare waiting to be added has not; a node that
    /// crashed before it had written anything *has*, which is why this is recorded rather than
    /// inferred from an empty log.
    pub started: bool,
    /// Whether this node's log has ever been truncated.
    ///
    /// It is what disqualifies a node from the cross-check between the driver's configuration
    /// and the core's. The two derive the same answer from the same appends — but the core
    /// folds changes forward and *reverts* them when a truncation takes the entry away, while
    /// the driver refolds the log from its base. Those are the same thing only if the revert is
    /// exact, which is the core's business to assert, not this harness's to assume. Before a
    /// truncation there is nothing to assume, so that is where the comparison is made.
    pub truncated: bool,
}

/// A way of applying a membership change wrongly, on purpose.
///
/// Membership is the part of Raft where a driver's own bookkeeping can diverge from the core's
/// without anything obviously breaking — the log still matches, entries still commit, and the
/// cluster carries on with two different ideas of who its members are. These exist so the
/// checks that catch that can be shown red.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfFault {
    /// Apply every change, as a driver should.
    #[default]
    None,
    /// Ignore the first change of each recomputation — a store that recorded the entry but
    /// forgot to act on it.
    SkipFirst,
    /// Apply the change *and* add the next node id as a voter — a store that moved two servers
    /// at once, which is what single-server change exists to forbid.
    MoveTwo,
}

impl NodeSlot {
    /// A live node.
    pub fn new(id: NodeId, node: RawNode<PersistedStorage>, bootstrap: ConfState) -> Self {
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
            config: bootstrap.clone(),
            base_config: bootstrap.clone(),
            lineage: Vec::new(),
            durable_config: bootstrap.clone(),
            bootstrap,
            config_index: 0,
            conf_fault: ConfFault::None,
            started: true,
            truncated: false,
        };
        slot.refresh();
        slot
    }

    /// A node that exists but has not been started: it has an inbox on the network and a node
    /// id, and nothing else. This is what a server waiting to be added to the group looks like.
    pub fn spare(id: NodeId, bootstrap: ConfState) -> Self {
        Self {
            id,
            node: None,
            durable: Some(MemStorage::default()),
            pending: None,
            applied: Vec::new(),
            applied_index: 0,
            log: Vec::new(),
            restarts: 0,
            compacted_through: 0,
            snapshot_term: 0,
            config: ConfState::default(),
            base_config: ConfState::default(),
            lineage: Vec::new(),
            durable_config: ConfState::default(),
            bootstrap,
            config_index: 0,
            conf_fault: ConfFault::None,
            started: false,
            truncated: false,
        }
    }

    /// Whether the driver's configuration and the core's were derived the same way, and so can
    /// be compared at all.
    #[must_use]
    pub fn comparable_config(&self) -> bool {
        self.restarts == 0 && self.compacted_through == 0 && !self.truncated
    }

    /// Whether this node has ever run.
    #[must_use]
    pub fn started(&self) -> bool {
        self.started
    }

    /// Writes the configuration a restart must recover into storage.
    ///
    /// `RawNode::new` reads the membership out of storage and does not replay conf-change
    /// entries for itself, so a driver that never does this loses every change across a
    /// restart. It rides with the batch that made those entries durable, which is what
    /// "membership rides with the log" means in practice (dissertation §4.1).
    pub fn persist_config(&mut self) {
        let durable = self.durable_config.clone();
        if let Some(node) = self.node.as_mut() {
            node.storage_mut().set_conf_state(durable);
        }
    }

    /// Folds applied entries into the snapshot, with the configuration *as of that index*.
    ///
    /// The order matters and is easy to get backwards. A snapshot's metadata carries the
    /// membership its prefix ends with, so it must be the configuration as of the compaction
    /// point — not the node's current one, which may already include changes from entries the
    /// snapshot does not cover. Writing the current one leaves a snapshot claiming a
    /// configuration from its own future, and every node that later restores from it derives a
    /// different membership from the same log. Found exactly that way.
    pub fn compact(&mut self, through: Index) -> bool {
        let as_of = self.config_as_of(through);
        let Some(node) = self.node.as_mut() else {
            return false;
        };
        node.storage_mut().set_conf_state(as_of);
        let compacted = node.storage_mut().compact(through).is_ok();
        if compacted {
            self.refresh();
            self.persist_config();
        }
        compacted
    }

    /// The configuration in force at `index`: the base, plus every conf-change entry at or
    /// below it.
    fn config_as_of(&self, index: Index) -> ConfState {
        let mut config = self.base_config.clone();
        for (entry, after) in &self.lineage {
            if entry.index > index {
                break;
            }
            config = after.clone();
        }
        config
    }

    /// Folds the conf-change entries of `entries` onto `base`, wrongly if a fault says so.
    fn derive(
        &self,
        base: &ConfState,
        base_index: Index,
        entries: &[Entry],
    ) -> (ConfState, Index, Vec<(EntryDigest, ConfState)>) {
        let mut config = base.clone();
        let mut config_index = base_index;
        let mut lineage = Vec::new();
        let mut skipped = false;
        for entry in entries {
            if entry.kind != EntryKind::ConfChange {
                continue;
            }
            let Ok(change) = ConfChange::decode(&entry.data) else {
                continue;
            };
            if self.conf_fault == ConfFault::SkipFirst && !skipped {
                skipped = true;
                continue;
            }
            change.apply_to(&mut config);
            if self.conf_fault == ConfFault::MoveTwo {
                // A second server, moved in the same step. Single-server change exists precisely
                // so that two consecutive configurations always share a quorum, and this is what
                // breaking that looks like from outside.
                let extra = config.voters.iter().max().copied().unwrap_or(0) + 1;
                config.voters.push(extra);
                config.normalize();
            }
            config_index = entry.index;
            lineage.push((
                EntryDigest::of(entry.index, entry.term, &entry.data),
                config.clone(),
            ));
        }
        (config, config_index, lineage)
    }

    /// The storage behind this node, alive or dead.
    fn storage(&self) -> Option<&MemStorage> {
        match (&self.node, &self.durable) {
            (Some(node), _) => Some(node.storage().durable()),
            (None, Some(durable)) => Some(durable),
            (None, None) => None,
        }
    }

    /// The configuration the driver has derived. A cloning accessor, so a caller can hold it
    /// while the slot is borrowed elsewhere.
    #[must_use]
    pub fn config_of(&self) -> ConfState {
        self.config.clone()
    }

    /// What the *core* believes the membership is, for the cross-check.
    #[must_use]
    pub fn core_config(&self) -> ConfState {
        self.node
            .as_ref()
            .map_or_else(|| self.config.clone(), RawNode::conf_state)
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
        self.started = true;
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
        // A snapshot replaces the log wholesale, which is the strongest form of the truncation
        // that disqualifies this node from the configuration cross-check.
        self.truncated = true;
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

    /// Rebuilds the log the checkers see: the whole of it, durable prefix and unstable tail.
    ///
    /// A live node is asked with [`RawNode::log_entries`], which returns the log as the *core*
    /// sees it. That accessor is what removed a gate this harness used to need: with only
    /// `storage` to read, the observed log lagged the commit index behind a slow disk, and two
    /// of the four properties had to be skipped while a node had a write outstanding. A dead
    /// node has no core to ask, and its durable bytes are its whole log by definition.
    pub fn refresh(&mut self) {
        self.log.clear();
        let Some(storage) = self.storage() else {
            self.compacted_through = 0;
            self.snapshot_term = 0;
            return;
        };
        let first = storage.first_index().unwrap_or(1);
        let last = storage.last_index().unwrap_or(0);
        let compacted_through = first.saturating_sub(1);
        let snapshot_term = storage
            .snapshot()
            .map(|snapshot| snapshot.meta.term)
            .unwrap_or_default();
        let snapshot = storage.snapshot().unwrap_or_default();
        let (snapshot_index, snapshot_conf) = (snapshot.meta.index, snapshot.meta.conf.clone());
        let durable: Vec<Entry> = if last >= first {
            storage
                .entries(first, last + 1, u64::MAX)
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let durable_digests: Vec<EntryDigest> = durable
            .iter()
            .map(|entry| EntryDigest::of(entry.index, entry.term, &entry.data))
            .collect();

        let full = self
            .node
            .as_ref()
            .and_then(|node| Self::full_entries(node, compacted_through));

        let fresh: Vec<EntryDigest> = match &full {
            Some(entries) => entries
                .iter()
                .map(|entry| EntryDigest::of(entry.index, entry.term, &entry.data))
                .collect(),
            None => durable_digests,
        };
        // A log that is not an extension of what it was has been truncated: some index now
        // holds a different entry, or the tail is simply gone.
        // A snapshot on its way to the disk has already replaced the core's log even though
        // storage still shows the old one, so the two derivations are already using different
        // bases: the node is out of the comparison from the moment the core accepts it.
        self.truncated |= self
            .pending
            .as_ref()
            .is_some_and(|write| write.ready.snapshot.is_some());
        self.truncated |= fresh.len() < self.log.len()
            || self
                .log
                .iter()
                .zip(fresh.iter())
                .any(|(before, after)| before != after);
        self.log = fresh;
        self.compacted_through = compacted_through;
        self.snapshot_term = snapshot_term;

        // Two derivations, because they answer two questions. The *current* one includes the
        // unstable tail, because a membership change takes effect when its entry is appended
        // (dissertation §4.1) — so that is what the core's own view is compared against, and it
        // has to be recomputed whenever the log changes, not only when a write lands. The
        // *durable* one is what a restart would recover, and it is what gets written to storage.
        let (base, base_index) = if snapshot_index > 0 {
            (snapshot_conf, snapshot_index)
        } else {
            (self.bootstrap.clone(), 0)
        };
        let (durable_config, _, _) = self.derive(&base, base_index, &durable);
        let (config, config_index, lineage) = match &full {
            Some(entries) => self.derive(&base, base_index, entries),
            None => self.derive(&base, base_index, &durable),
        };
        self.durable_config = durable_config;
        self.config = config;
        self.config_index = config_index;
        self.lineage = lineage;
        self.base_config = base;
    }

    /// The core's whole log, or `None` if there is no core to ask.
    fn full_entries(
        node: &RawNode<PersistedStorage>,
        compacted_through: Index,
    ) -> Option<Vec<Entry>> {
        let last = node.status().last_index;
        let first = compacted_through + 1;
        if last < first {
            return Some(Vec::new());
        }
        node.log_entries(first, last + 1).ok()
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
