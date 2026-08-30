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
//! *backwards* — see [`upsert_region`].

use std::sync::Arc;

use esker_engine::{Db, ReadOptions, WriteBatch, cf};

use crate::error::Result;
use crate::keys;
use crate::record::{RegionRecord, StoreRecord};

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
/// Recorded and exposed in 4a; **nothing acts on it**. Replica repair — adding a peer
/// elsewhere when a store stays down — is 4c, and building the operator that does it before
/// the sub-phase that owns it is exactly what `CLAUDE.md` says not to do.
pub fn down_stores(stores: &[StoreRecord], now_ms: u64, max_down_ms: u64) -> Vec<u64> {
    stores
        .iter()
        .filter(|store| now_ms.saturating_sub(store.last_heartbeat_ms) > max_down_ms)
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
