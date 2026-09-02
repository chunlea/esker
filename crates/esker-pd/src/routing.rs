//! The routing table: where every region lives, and which stores are alive.
//!
//! Reads and writes of PD's region and store records, over its own engine. The lookup
//! `GetRegion(key)` answers is one seek into the range index — the layout that makes that
//! possible, and the two traps in it, are documented in [`crate::keys`].
//!
//! # The table is authoritative here and advisory to clients
//!
//! `docs/DESIGN.md` §7: clients cache regions and invalidate on epoch errors, and *correctness
//! never depends on a client's cache being fresh*. That asymmetry is why this table is allowed
//! to be built from heartbeats that may arrive late, out of order, or not at all: a stale
//! answer costs a client one redirect, because the store it is sent to checks the epoch itself
//! (`CLAUDE.md` invariant 5). It is also why a heartbeat is never allowed to move the table
//! *backwards* — see [`accepts`], which is the guard every heartbeat passes through.

use std::sync::Arc;

use esker_engine::{Db, ReadOptions, WriteBatch, cf};
use esker_proto::{Epoch, Region};

use crate::error::Result;
use crate::keys;
use crate::record::{RegionRecord, StoreRecord, StoreStats};

/// What one store reports about itself, every 10 s (`docs/DESIGN.md` §14).
///
/// The field set is the one `docs/plans/phase-4.md` §3.2 pins for the store lane's
/// `PdClient`, so the two halves map onto each other without a gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreBeat {
    /// Which store is reporting.
    pub store_id: u64,
    /// Its capacity and load.
    pub stats: StoreStats,
}

/// What one region's **leader** reports, every 60 s or on a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionBeat {
    /// The region as its leader currently sees it: range, peers and epoch.
    pub region: Region,
    /// The peer sending this. Zero means the sender does not claim to be the leader.
    pub leader_peer_id: u64,
    /// The Raft term it is leading in. The tiebreaker within one epoch — see [`accepts`].
    pub term: u64,
    /// Approximate bytes of user data. 4b splits on it; 4a only records it.
    pub approximate_size: u64,
    /// The leader's apply index, so 4c can rebuild its operator view from heartbeats alone.
    pub applied_index: u64,
}

/// What an upsert did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    /// The record was written.
    Applied,
    /// The heartbeat was older than what PD holds, and was dropped.
    Stale,
}

/// Whether a heartbeat may replace what PD already holds for a region.
///
/// Heartbeats arrive out of order — a leader changes, the old leader's beat is still in
/// flight, and the two cross on the network. Overwriting a newer record with an older one
/// would leave clients being redirected to a peer that lost office, and *the whole point of
/// the routing table is that it is the best answer PD has*, so the order it accepts them in is
/// the order they happened, not the order they arrived.
///
/// Two rules, in this order:
///
/// * **The epoch decides.** `(conf_ver, version)` are compared per-counter, because they move
///   on different events (`Epoch::is_stale_against`): a heartbeat behind in *either* is stale
///   and is dropped. Epochs only ever advance for a region, so an epoch behind in one counter
///   and ahead in the other is impossible in a correct cluster — and it is dropped too, which
///   is the safe direction to be wrong in.
/// * **Within one epoch, the term decides.** A conf change or a split bumps the epoch, but a
///   plain leader election does not; the term is what "newer" means for a leader. A beat from
///   an older term at the same epoch is a leader that has already lost office, so its hint is
///   dropped rather than allowed to overwrite its successor's.
///
/// A heartbeat at the same epoch and the same term is accepted: it is the same leader
/// reporting again, with fresher stats.
#[must_use]
pub fn accepts(stored: &RegionRecord, epoch: Epoch, term: u64) -> bool {
    if epoch.is_stale_against(stored.region.epoch) {
        return false;
    }
    if epoch == stored.region.epoch {
        return term >= stored.term;
    }
    true
}

/// Adds a store record to `batch`.
pub fn stage_store(batch: &mut WriteBatch, cf: u32, record: &StoreRecord) {
    batch.put(cf, &keys::store_key(record.store_id), &record.encode());
}

/// Adds a region record and its range-index entry to `batch`, dropping the index entry of
/// `previous` when the region's end key has moved.
///
/// Both keys go in **one** batch, so the index can never name a region that is not there, nor
/// miss one that is. When a split moves a region's end key (4b), the old index entry is
/// deleted in the same batch that writes the new one; leaving it would route keys to a region
/// that no longer reaches them.
pub fn stage_region(
    batch: &mut WriteBatch,
    cf: u32,
    record: &RegionRecord,
    previous: Option<&RegionRecord>,
) {
    if let Some(previous) = previous
        && previous.region.end_key != record.region.end_key
    {
        batch.delete(cf, &keys::range_key(&previous.region.end_key));
    }
    batch.put(cf, &keys::region_key(record.region.id), &record.encode());
    batch.put(
        cf,
        &keys::range_key(&record.region.end_key),
        &crate::record::encode_range_entry(record.region.id),
    );
}

/// One region by id.
pub fn read_region(db: &Db, region_id: u64) -> Result<Option<RegionRecord>> {
    let Some(bytes) = db.get(
        cf::DEFAULT,
        &keys::region_key(region_id),
        &ReadOptions::default(),
    )?
    else {
        return Ok(None);
    };
    Ok(Some(RegionRecord::decode(&bytes)?))
}

/// One store by id.
pub fn read_store(db: &Db, store_id: u64) -> Result<Option<StoreRecord>> {
    let Some(bytes) = db.get(
        cf::DEFAULT,
        &keys::store_key(store_id),
        &ReadOptions::default(),
    )?
    else {
        return Ok(None);
    };
    Ok(Some(StoreRecord::decode(&bytes)?))
}

/// The region whose range contains `key`, in one seek.
///
/// `None` means no region covers the key. In a table that is a contiguous partition of the key
/// space — which it is, from bootstrap onwards — that cannot happen; it is returned rather than
/// asserted because a hole in the table is a bug to report, not a reason to panic on a request
/// (`CLAUDE.md` invariant 9).
pub fn lookup(db: &Db, key: &[u8]) -> Result<Option<RegionRecord>> {
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default())?;
    iter.seek(&keys::range_seek_key(key));
    if !iter.valid() {
        iter.status()?;
        return Ok(None);
    }
    if !iter.key().starts_with(&keys::prefix(keys::RANGE)) {
        // Past the end of the index: the key is above every region's end, which only happens
        // if the region running to +infinity is missing.
        return Ok(None);
    }
    let region_id = crate::record::decode_range_entry(iter.value())?;
    iter.status()?;

    let Some(record) = read_region(db, region_id)? else {
        tracing::warn!(region_id, "the range index names a region with no record");
        return Ok(None);
    };
    if !record.region.contains(key) {
        tracing::warn!(
            region_id,
            "the region the index found does not contain the key"
        );
        return Ok(None);
    }
    Ok(Some(record))
}

/// A page of region records in **key** order, starting with the region that contains `start_key`.
///
/// The range index is the key-ordered view — the same one [`lookup`] seeks into — so this is a
/// seek and a walk rather than a sort of [`regions`], which is in id order and would need one.
///
/// An index entry naming a region with no record is **skipped with a warning**, not an error: a
/// hole in the table is a bug to report and not a reason to fail a listing that can still show
/// everything else (`CLAUDE.md` invariant 9, the same call [`lookup`] makes).
pub fn scan_ranges(db: &Db, start_key: &[u8], limit: usize) -> Result<Vec<RegionRecord>> {
    let mut out = Vec::new();
    if limit == 0 {
        return Ok(out);
    }
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default())?;
    let prefix = keys::prefix(keys::RANGE);
    iter.seek(&keys::range_seek_key(start_key));
    while iter.valid() && iter.key().starts_with(&prefix) && out.len() < limit {
        let region_id = crate::record::decode_range_entry(iter.value())?;
        if let Some(record) = read_region(db, region_id)? {
            out.push(record);
        } else {
            tracing::warn!(region_id, "the range index names a region with no record");
        }
        iter.next();
    }
    iter.status()?;
    Ok(out)
}

/// Every region record, in id order.
pub fn regions(db: &Db) -> Result<Vec<RegionRecord>> {
    let mut out = Vec::new();
    scan(db, keys::REGION, |bytes| {
        out.push(RegionRecord::decode(bytes)?);
        Ok(())
    })?;
    Ok(out)
}

/// Every store record, in id order.
pub fn stores(db: &Db) -> Result<Vec<StoreRecord>> {
    let mut out = Vec::new();
    scan(db, keys::STORE, |bytes| {
        out.push(StoreRecord::decode(bytes)?);
        Ok(())
    })?;
    Ok(out)
}

/// Every range-index entry as `(end_key, region_id)`, in key order. For the inspector, and for
/// the tests that check the index against the records it points at.
pub fn range_index(db: &Db) -> Result<Vec<(Vec<u8>, u64)>> {
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default())?;
    let prefix = keys::prefix(keys::RANGE);
    let mut out = Vec::new();
    iter.seek(&prefix);
    while iter.valid() && iter.key().starts_with(&prefix) {
        let end = keys::end_key_in_range_key(iter.key())
            .flatten()
            .unwrap_or_default()
            .to_vec();
        out.push((end, crate::record::decode_range_entry(iter.value())?));
        iter.next();
    }
    iter.status()?;
    Ok(out)
}

fn scan(db: &Db, kind: u8, mut visit: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let mut iter = db.iter(cf::DEFAULT, &ReadOptions::default())?;
    let prefix = keys::prefix(kind);
    iter.seek(&prefix);
    while iter.valid() && iter.key().starts_with(&prefix) {
        visit(iter.value())?;
        iter.next();
    }
    iter.status()?;
    Ok(())
}

/// The stores that have not been heard from for longer than `max_down_ms`.
///
/// The verdict itself is [`crate::schedule::is_down`], so that the repair rule and this
/// reporting view cannot come to disagree about what "down" means — the same reason
/// `ProtoError::is_retryable` lives with the error rather than in the client.
pub fn down_stores(stores: &[StoreRecord], now_ms: u64, max_down_ms: u64) -> Vec<u64> {
    stores
        .iter()
        .filter(|store| crate::schedule::is_down(store, now_ms, max_down_ms))
        .map(|store| store.store_id)
        .collect()
}

/// A handle on PD's database, for the readers above. Exists so callers outside this module do
/// not have to remember which column family PD keeps its state in.
#[derive(Debug, Clone)]
pub struct Table {
    db: Arc<Db>,
}

impl Table {
    /// Reads PD's records out of `db`.
    #[must_use]
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// The database underneath.
    #[must_use]
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// See [`lookup`].
    pub fn lookup(&self, key: &[u8]) -> Result<Option<RegionRecord>> {
        lookup(&self.db, key)
    }

    /// See [`read_region`].
    pub fn region(&self, region_id: u64) -> Result<Option<RegionRecord>> {
        read_region(&self.db, region_id)
    }

    /// See [`read_store`].
    pub fn store(&self, store_id: u64) -> Result<Option<StoreRecord>> {
        read_store(&self.db, store_id)
    }

    /// See [`regions`].
    pub fn regions(&self) -> Result<Vec<RegionRecord>> {
        regions(&self.db)
    }

    /// See [`stores`].
    pub fn stores(&self) -> Result<Vec<StoreRecord>> {
        stores(&self.db)
    }
}
