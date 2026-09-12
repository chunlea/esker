//! The apply target **on a live region**: what makes a store's columnar learner columnar.
//!
//! [`super::ColumnarApply`] is one table's memtable and its runs; this is the part that decides
//! there should be one at all. A peer whose role in the region record is
//! [`PeerRole::ColumnarLearner`](esker_proto::PeerRole::ColumnarLearner) routes every committed
//! row version through here, and [`crate::server::Store`]'s fragment service reads what it built
//! (`docs/plans/phase-8-learner.md` §store unit 3).
//!
//! # A tee, not a replacement — and the reason is the schema
//!
//! ADR 0022 says a columnar replica is a learner *"whose apply writes columns **instead of**
//! rows"*, and this writes both. That is a deliberate, named limit rather than a shortcut, and
//! two facts force it:
//!
//! * **The decoder's schema is a catalog record.** `esker_keys::columnar::Published` is written by
//!   the `ALTER` that asks for a columnar copy, and it lives in the cluster's `'m'` key space —
//!   which this store can only read if it *has* it, which means applying rows. The plan's answer
//!   is a schema **push** (§store, "the schema the decoder is built from"), and no mechanism
//!   pushes one today.
//! * **A committed version is only knowable from the transactional state.** Percolator's value
//!   arrives at *prewrite* and becomes visible at *commit*, so a replica that kept no `write` and
//!   `default` column families could not tell what it had committed. Dropping the row half means
//!   teaching this target to consume the prewrite stream and buffer uncommitted values itself,
//!   which is a design task and not a wiring one.
//!
//! So the storage saving ADR 0022 is *for* is not delivered here. What is delivered is everything
//! that depends on the columnar copy existing on a live region: the apply path, the fragment
//! service, and the differential that proves the two engines agree.
//!
//! # Completeness, which is the property a wrong answer would come from
//!
//! A learner is placed on a region that already has data, and a crash can lose an unsealed
//! memtable. Neither is visible from the runs themselves — a short run looks exactly like a
//! complete one — so an open may never simply trust what it finds on disk.
//!
//! It no longer has to re-read the region to avoid that. The run manifest names the region apply
//! index its runs are complete to ([`super::runs`],
//! [ADR 0038](../../../../docs/adr/0038-a-run-manifest-names-the-index-its-runs-are-complete-to.md)),
//! written in the same atomic rename that makes a run live, so an open **resumes**: it replays the
//! Raft log from that index to the applied index and feeds what those entries committed, which is
//! the same input the tee would have given it. The cost of an open stops being the region's size
//! and becomes the log since the last seal.
//!
//! Three things make it safe, and each is the conservative choice:
//!
//! * the manifest may only ever **understate**. It moves on a seal, to the last *entry* whose
//!   versions are all in a run — never to a buffered row, and never past a partly-fed entry;
//! * the resume decision is made **at one engine snapshot**, and the applied index it trusts is
//!   read at that same snapshot. Reading the index and the data at two instants is how a copy ends
//!   up claiming an entry it never saw, because the apply path writes both while a fragment on
//!   another thread is walking;
//! * anything that does not add up — no manifest, a version-1 manifest, an index below the log's
//!   truncation point, a log entry that will not decode — **falls back to the full walk, loudly**.
//!   The walk is the behaviour that was always correct; the resume is an optimisation, and an
//!   optimisation that is unsure of itself must give way to it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use esker_columnar::Schema;
use esker_engine::fs::FileSystem;
use esker_engine::{Db, ReadOptions, Snapshot, cf};
use esker_keys::prefix::{self, TablePart};
use esker_txn::{Kind, ReadOutcome, TxnSnapshot, WriteRecord};

use super::decode::TableDecoder;
use super::{ColumnarApply, ColumnarOptions};
use crate::error::{Result, StoreError};
use crate::txnkv::EngineSnapshot;

/// One region's columnar copy, and the lock that lets an apply and a fragment share it.
///
/// Held by the peer's driver **and** by the store's request path, which is why the state is
/// behind a `Mutex` rather than owned by either: the driver appends and the fragment service
/// reads, and neither is allowed to wait on the other for longer than it takes to swap a list of
/// file names (see [`ColumnarSlot::table`]).
#[derive(Debug)]
pub struct ColumnarSlot {
    fs: Arc<dyn FileSystem>,
    /// `<data-dir>/columnar/<region-id>`, with one directory per table under it.
    dir: PathBuf,
    /// The region whose log a resume replays. The directory names it too, and a slot that read it
    /// back out of its own path would be one rename away from replaying another region's entries.
    region_id: u64,
    options: ColumnarOptions,
    tables: Mutex<Tables>,
}

/// What the last open of one table's copy had to read.
///
/// Published because "did this open resume or re-walk the region" is the question the whole of
/// [ADR 0038](../../../../docs/adr/0038-a-run-manifest-names-the-index-its-runs-are-complete-to.md)
/// is about, and it is not answerable from the runs: a resumed copy and a rebuilt one are the same
/// files. A test asserts on it and an operator would want it as a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Build {
    /// The apply index the runs were complete to when the copy was opened; `0` for a full walk.
    pub from_index: u64,
    /// The apply index the copy is complete to now.
    pub to_index: u64,
    /// Versions fed into the copy by this open.
    pub versions: usize,
    /// Whether the open replayed the log from `from_index` instead of re-walking the region.
    pub resumed: bool,
}

/// The per-table targets, and what has been looked for and not found.
#[derive(Debug, Default)]
struct Tables {
    open: BTreeMap<(u64, u64), ColumnarApply>,
    /// Tables with no columnar record here, so a per-commit lookup is not paid twice.
    ///
    /// Cleared whenever a **catalog** write commits in this region, which is the only event that
    /// can turn one of these into a table that wants a copy — so the cache is exact rather than
    /// expiring on a timer.
    missing: BTreeSet<(u64, u64)>,
    /// Records fetched from a store that holds the catalog, for the tables this region cannot
    /// read the record of itself.
    ///
    /// **Only ever the tables whose record is not here.** A store that can read `'m'` never
    /// populates this, so on a single-region cluster it is empty and nothing behaves differently
    /// (`crate::server::Store::ensure_schema`,
    /// [ADR 0037](../../../../docs/adr/0037-a-columnar-learner-fetches-the-schema-it-cannot-read.md)).
    ///
    /// Not cleared by a catalog write, because a store holding this map by definition sees no
    /// catalog writes for these tables. What replaces one is a newer fetch, and what asks for a
    /// newer fetch is a decode that refuses — `DecodeOutcome::SchemaBehind`, which is the loud
    /// half of `crate::columnar::decode`'s "a stale schema is lag, not corruption".
    fetched: BTreeMap<(u64, u64), (u8, esker_keys::columnar::Published)>,
    /// What the last open of each table's copy had to read.
    builds: BTreeMap<(u64, u64), Build>,
    /// The highest region apply index this slot has been **told about** — by
    /// [`ColumnarSlot::saw`], by a tee, or by the walk an open did.
    ///
    /// Not per table, and that is the point: an entry that touched another table still says this
    /// slot was present for that index, and a table it did not touch is complete without it. What
    /// this answers is the one question a copy cannot answer for itself — *was I here?*
    seen: u64,
}

/// What a fragment needs to evaluate, taken under the lock and used outside it.
#[derive(Debug, Clone)]
pub struct TableRuns {
    /// The run schema: the table's columns, then `__key`, `__commit_ts`, `__deleted`.
    ///
    /// **The current one**, which the oldest run need not have been written under. Every run is
    /// read as having these columns, which is what lets an `ADD COLUMN` land mid-workload.
    pub schema: Schema,
    /// One per column of `schema`: what a run written before that column existed reads for it.
    pub missing: Vec<esker_columnar::Value>,
    /// The live runs, oldest first. Immutable files: safe to read with the lock released.
    pub paths: Vec<PathBuf>,
    /// `(key, commit_ts, deleted)` slots, for the visibility the read applies.
    pub visibility: (u32, u32, u32),
    /// How many of the schema's columns are the table's own.
    pub data_columns: usize,
}

impl ColumnarSlot {
    /// A slot for one region. Opens nothing: a peer that is not a columnar learner never will.
    #[must_use]
    pub fn new(
        fs: Arc<dyn FileSystem>,
        dir: impl AsRef<Path>,
        region_id: u64,
        options: ColumnarOptions,
    ) -> Self {
        Self {
            fs,
            dir: dir.as_ref().to_path_buf(),
            region_id,
            options,
            tables: Mutex::new(Tables::default()),
        }
    }

    /// What the last open of `(tenant, table_id)`'s copy read, if one has been opened here.
    #[must_use]
    pub fn last_build(&self, tenant: u64, table_id: u64) -> Option<Build> {
        self.lock().builds.get(&(tenant, table_id)).copied()
    }

    fn lock(&self) -> MutexGuard<'_, Tables> {
        self.tables.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ingests the versions one committed transaction made visible.
    ///
    /// Called from the apply path **after** the batch is durable, so every version it reads is one
    /// this region has actually committed — the same records a row read would resolve, rather than
    /// a prediction made from the command.
    ///
    /// `index` is the log index of the entry that committed them, and it is what a later reopen
    /// resumes from: it is recorded once the entry's last key has been fed, never before, because
    /// half an entry is not a point anything can resume at.
    pub fn commit(&self, db: &Db, index: u64, commit_ts: u64, keys: &[Bytes]) -> Result<()> {
        let mut tables = self.lock();

        // A catalog write is the only thing that can add a columnar record or move a schema
        // version, so it is the exact moment to stop trusting either cache.
        if keys.iter().any(|key| is_meta(key)) {
            tables.missing.clear();
            refresh_schemas(db, &mut tables)?;
        }

        // A table whose target was built by this very call has already been converted from the
        // engine, which now includes this batch — feeding these keys again would double them.
        let mut converted: BTreeSet<(u64, u64)> = BTreeSet::new();
        // Every table this entry touched, so the index is recorded on each of them once the whole
        // entry has been fed.
        let mut touched: BTreeSet<(u64, u64)> = BTreeSet::new();
        for key in keys {
            let Some((tenant, table_id)) = row_of(key)? else {
                continue;
            };
            let built = self.ensure(db, &mut tables, tenant, table_id)?;
            if built {
                converted.insert((tenant, table_id));
            }
            touched.insert((tenant, table_id));
            if converted.contains(&(tenant, table_id)) {
                continue;
            }
            let Some(apply) = tables.open.get_mut(&(tenant, table_id)) else {
                continue;
            };
            let snapshot = EngineSnapshot::new(db);
            let Some(version) = snapshot
                .seek_write(key, commit_ts)
                .map_err(|error| bootstrap(&format!("reading a committed version: {error}")))?
            else {
                continue;
            };
            // Not ours: the commit this entry asked for did not take, and something older is the
            // newest thing at or below its timestamp.
            if version.commit_ts != commit_ts {
                continue;
            }
            match committed_of(&snapshot, key, &version.record)? {
                Committed::Row(value) => {
                    apply.apply(&versioned(key, commit_ts), Some(&value))?;
                }
                Committed::Tombstone => apply.apply(&versioned(key, commit_ts), None)?,
                Committed::Nothing => {}
            }
        }

        // The entry is fed. **Now** its index may be claimed — and only for the tables it reached,
        // because a table this entry did not touch is complete to whatever its own last entry was
        // and saying otherwise would let a resume skip an entry that did touch it.
        for table in touched {
            if let Some(apply) = tables.open.get_mut(&table) {
                apply.entry_applied(index)?;
            }
        }
        // Recorded for **every** entry that reaches here, including one that touched no table with
        // a copy: what it establishes is that this slot was present at `index`, not that anything
        // was written.
        tables.seen = tables.seen.max(index);
        Ok(())
    }

    /// Seals every open table's memtable, so what has been applied is on disk.
    ///
    /// Called where the region reaches a natural boundary. A run per seal is the cost, and it is
    /// what makes a fragment see recent writes without reading a memtable that a reader on another
    /// thread cannot be shown safely.
    pub fn seal(&self) -> Result<()> {
        let mut tables = self.lock();
        for apply in tables.open.values_mut() {
            apply.seal()?;
        }
        Ok(())
    }

    /// What a fragment for `(tenant, table_id)` should be evaluated over, or `None` when this
    /// store holds no columnar copy of that table.
    ///
    /// **Builds one if the catalog says the table wants it**, which is what makes a learner
    /// answerable without waiting for a write: placement puts a columnar learner on a region that
    /// already has its data, so the first fragment is often the first thing that needs the copy to
    /// exist. The build is a conversion from the region's own committed state, so what it produces
    /// is complete rather than "everything since I noticed".
    ///
    /// Seals before answering, so the answer includes everything applied. The paths come back **by
    /// value** and the lock is released: a run is an immutable file, so reading one outside the
    /// lock is safe, and holding the lock across an evaluation would stall the region's apply for
    /// the length of a scan.
    pub fn table(&self, db: &Db, tenant: u64, table_id: u64) -> Result<Option<TableRuns>> {
        let mut tables = self.lock();
        // **The read path does not trust the miss cache.** A learner is placed on a region and
        // then catches up, so "this table has no columnar record" is a fact with a short shelf
        // life: the record arrives with the rest of the region's state, in entries this peer had
        // not applied when the first fragment asked. The apply path may remember a miss — it is
        // hot, and a catalog write there clears the cache exactly — but a fragment is rare enough
        // to pay a point read and must never answer `NotColumnar` from a stale "no".
        tables.missing.remove(&(tenant, table_id));
        self.ensure(db, &mut tables, tenant, table_id)?;
        let Some(apply) = tables.open.get_mut(&(tenant, table_id)) else {
            return Ok(None);
        };
        apply.seal()?;
        let runs = apply.runs();
        Ok(Some(TableRuns {
            schema: apply.schema().clone(),
            missing: apply.missing(),
            paths: runs
                .live()
                .iter()
                .map(|number| runs.path_of(*number))
                .collect(),
            visibility: apply.visibility_slots(),
            data_columns: apply.schema().len() - 3,
        }))
    }

    /// Whether this store's **own engine** holds the record for `(tenant, table_id)`.
    ///
    /// The same read the copy's own build would do, so a `true` here is a promise the next call
    /// can keep — and it deliberately does **not** consider a record that was fetched from another
    /// store. A store that can read the record locally re-reads it on every fragment already
    /// (`table` clears the miss cache and `ensure` reads through), so its schema cannot go stale;
    /// a store that cannot has no other way to notice one moving, and answering `true` from its
    /// cache is what would freeze it at the version it first saw.
    ///
    /// An error reading the engine answers `false`. The caller's next move is a fetch, which
    /// either succeeds or leaves the table refused; turning a transient read failure into a hard
    /// error here would fail a fragment that a row scan could have answered.
    #[must_use]
    pub fn reads_schema_locally(&self, db: &Db, tenant: u64, table_id: u64) -> bool {
        matches!(published_schema(db, tenant, table_id), Ok(Some(_)))
    }

    /// Installs a columnar record fetched from a store that holds the catalog.
    ///
    /// Takes the **bytes** and decodes them here, so a record that arrived over the wire and one
    /// read from this store's own engine go through one parser
    /// ([`esker_keys::columnar::decode`]) and cannot come to disagree about the format.
    ///
    /// A record older **or equal** is dropped rather than installed, and the equal case is the one
    /// that matters for cost: a fragment re-fetches every time, and the answer is almost always the
    /// version already held, so this is what keeps a re-fetch from rebuilding a copy that is
    /// already right. An older one is dropped because nothing orders two fetches — a slow answer
    /// from one store can land after a fast one from another — and installing an older schema over
    /// a newer would make the copy refuse rows it had already decoded. `schema_version` is
    /// monotonic per table and is exactly the comparison `esker_keys::columnar::Published`
    /// documents itself for.
    ///
    /// When the version really has moved, any open copy of that table is **closed**, so the next
    /// read rebuilds it under the new schema rather than extending a copy built under the old one.
    pub fn install_record(&self, tenant: u64, table_id: u64, record: &[u8]) -> Result<()> {
        let (replicas, published) = esker_keys::columnar::decode(record)
            .map_err(|error| bootstrap(&format!("a fetched columnar record: {error}")))?;
        let mut tables = self.lock();
        if let Some((_, held)) = tables.fetched.get(&(tenant, table_id))
            && held.schema_version >= published.schema_version
        {
            return Ok(());
        }
        tables
            .fetched
            .insert((tenant, table_id), (replicas, published));
        // The miss cache said "no record here", which has just stopped being the useful answer.
        tables.missing.remove(&(tenant, table_id));
        tables.open.remove(&(tenant, table_id));
        Ok(())
    }

    /// The filesystem the runs live on, for the reader the fragment service opens.
    #[must_use]
    pub fn fs(&self) -> &Arc<dyn FileSystem> {
        &self.fs
    }

    /// Whether this region holds a columnar copy of anything.
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.lock().open.is_empty()
    }

    /// Closes every table's copy, so a fragment refuses until one is rebuilt.
    ///
    /// What the apply path does when it cannot ingest a commit. The runs on disk are left alone —
    /// the next commit reopens the table, which **rebuilds** from the region's own state, so a
    /// closed copy is never a half copy that comes back as if it were whole.
    pub fn close(&self) {
        let mut tables = self.lock();
        tables.open.clear();
        tables.missing.clear();
    }

    /// Records that the peer applied entry `index`, and drops every open copy when that entry did
    /// **not** follow the last one this slot was told about.
    ///
    /// Called for every entry a columnar learner applies, committing or not, which is what makes
    /// the number exact: a gap in it means entries reached the region's state and did not reach
    /// this copy, and a copy missing an entry is a copy that can answer without a row.
    ///
    /// **What opens a gap is a snapshot install.** A replica that falls behind its leader's
    /// compaction boundary is repaired by one, and a snapshot writes committed versions straight
    /// into the column families — no entry applies, so [`ColumnarSlot::commit`] never runs — onto a
    /// log that begins after the snapshot's index. `Store::fetch_snapshot` replaces the region
    /// through `Store::retire_region_now`, which stops the peer and leaves this slot where it was,
    /// so the copy that comes back is the one that was there before, missing everything in between
    /// and with no path back to it: the log cannot replay those entries and nothing re-walks. The
    /// first entry applied after the transfer is the gap, and this is where it is caught.
    ///
    /// The same gap opens when a peer applies entries before its region record calls it a columnar
    /// learner — the tee is skipped for those, so the next entry after the role lands is the first
    /// this slot hears of. Dropping the copies makes the next open re-walk, which is the answer in
    /// both cases.
    ///
    /// **Checked here rather than when a fragment asks**, because by then the evidence is gone: a
    /// single entry teed after the gap would leave the index looking continuous. A gap is a fact
    /// about the moment it happens.
    pub fn saw(&self, index: u64) {
        let mut tables = self.lock();
        if !tables.open.is_empty() && index > tables.seen.saturating_add(1) {
            tracing::warn!(
                region_id = self.region_id,
                seen = tables.seen,
                index,
                "this region's columnar copy was not told of every entry between; re-opening it \
                 from the region rather than answering from it"
            );
            tables.open.clear();
        }
        tables.seen = tables.seen.max(index);
    }

    /// Opens the target for a table if the catalog says it wants one, converting what the region
    /// already holds. Answers whether it built one *now*.
    fn ensure(&self, db: &Db, tables: &mut Tables, tenant: u64, table_id: u64) -> Result<bool> {
        if tables.open.contains_key(&(tenant, table_id))
            || tables.missing.contains(&(tenant, table_id))
        {
            return Ok(false);
        }
        // A fetched record first: this store has one only when it could not read the record
        // itself, so preferring it costs a map lookup on a path that is about to read the engine.
        let local = match tables.fetched.get(&(tenant, table_id)) {
            Some(record) => Some(record.clone()),
            None => published_schema(db, tenant, table_id)?,
        };
        let Some((replicas, published)) = local else {
            tracing::debug!(
                tenant,
                table_id,
                "no columnar record here, so no columnar copy of this table"
            );
            tables.missing.insert((tenant, table_id));
            return Ok(false);
        };
        if replicas == 0 {
            tracing::debug!(tenant, table_id, "this table asks for no columnar copies");
            tables.missing.insert((tenant, table_id));
            return Ok(false);
        }

        let dir = self.dir.join(format!("{tenant}-{table_id}"));
        let decoder = Arc::new(decoder_of(&published)?);

        // One snapshot for the whole decision. The apply path writes the state record and the data
        // in one batch while a fragment on another thread may be in here, so an applied index read
        // at a different instant from the data is an index this copy may not hold.
        let pinned = db.snapshot();
        let state = crate::raft_log::read_state(db, self.region_id, Some(pinned.clone()))?;

        let build = match self.resume(
            db,
            &dir,
            state.as_ref(),
            &pinned,
            &decoder,
            tenant,
            table_id,
        ) {
            Ok(Some(resumed)) => Some(resumed),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    tenant,
                    table_id,
                    region_id = self.region_id,
                    %error,
                    "resuming a columnar copy from its manifest failed; re-walking the region"
                );
                None
            }
        };
        let (apply, build) = match build {
            Some(resumed) => resumed,
            None => self.rebuild(db, &dir, state.as_ref(), &pinned, decoder, tenant, table_id)?,
        };

        tracing::info!(
            tenant,
            table_id,
            versions = build.versions,
            from_index = build.from_index,
            to_index = build.to_index,
            resumed = build.resumed,
            replicas,
            schema_version = published.schema_version,
            dir = %dir.display(),
            "opened a columnar copy of a table"
        );
        tables.builds.insert((tenant, table_id), build);
        tables.open.insert((tenant, table_id), apply);
        // The walk read the region at one instant and the state record named it, so the slot was
        // as present at that index as a tee makes it.
        tables.seen = tables.seen.max(build.to_index);
        Ok(true)
    }

    /// Opens a copy by replaying the log from the index its manifest names, or `None` when the
    /// manifest does not let it.
    ///
    /// The four refusals are the whole of the safety argument, and each answers `None` so the
    /// caller re-walks: no manifest or a version-1 one (`applied` is `0`), no state record for
    /// this region, an applied index below the log's truncation point — the entries that would be
    /// replayed are gone — and a manifest claiming an index the region has not applied, which is a
    /// manifest from somewhere else.
    #[allow(clippy::too_many_arguments)]
    fn resume(
        &self,
        db: &Db,
        dir: &Path,
        state: Option<&crate::raft_log::PersistedState>,
        pinned: &Snapshot,
        decoder: &Arc<TableDecoder>,
        tenant: u64,
        table_id: u64,
    ) -> Result<Option<(ColumnarApply, Build)>> {
        let from = super::runs::applied_index_of(self.fs.as_ref(), dir)?;
        if from == 0 {
            return Ok(None);
        }
        let Some(state) = state else {
            tracing::warn!(
                region_id = self.region_id,
                from,
                "a columnar copy names an apply index but this store has no log for its region"
            );
            return Ok(None);
        };
        if from < state.truncated_index {
            tracing::warn!(
                region_id = self.region_id,
                tenant,
                table_id,
                from,
                truncated_index = state.truncated_index,
                "a columnar copy is older than its region's log; re-walking the region instead"
            );
            return Ok(None);
        }
        if from > state.applied_index {
            tracing::warn!(
                region_id = self.region_id,
                tenant,
                table_id,
                from,
                applied_index = state.applied_index,
                "a columnar copy claims an apply index its region has not reached"
            );
            return Ok(None);
        }

        let mut apply = ColumnarApply::open(
            Arc::clone(&self.fs),
            dir,
            Arc::clone(decoder) as Arc<dyn super::RowDecoder>,
            self.options.clone(),
        )?;
        let versions = replay(
            db,
            pinned,
            &mut apply,
            self.region_id,
            from,
            state.applied_index,
            tenant,
            table_id,
        )?;
        apply.built_to(state.applied_index)?;
        Ok(Some((
            apply,
            Build {
                from_index: from,
                to_index: state.applied_index,
                versions,
                resumed: true,
            },
        )))
    }

    /// This region's `[start_key, end_key)`, read from the region record rather than remembered.
    ///
    /// **Read at rebuild time and never cached on the slot**, because a split moves it: a slot
    /// outlives the range it was created with, and a walk that used a remembered range would
    /// rebuild the parent's old span into the child's copy.
    ///
    /// A region record this store does not have is the whole key space, which is what a walk did
    /// before this existed. That is the conservative direction — too much rather than too little —
    /// and it is unreachable in practice: the slot is created by the peer that hosts the region,
    /// so the record is there before anything can rebuild.
    fn region_bounds(&self, db: &Db) -> Result<(Vec<u8>, Vec<u8>)> {
        let regions = crate::meta::load_regions(db)
            .map_err(|error| bootstrap(&format!("reading this store's region records: {error}")))?;
        Ok(regions
            .into_iter()
            .find(|region| region.id == self.region_id)
            .map(|region| (region.start_key.to_vec(), region.end_key.to_vec()))
            .unwrap_or_default())
    }

    /// Opens a copy by converting everything the region holds, which is what a resume falls back
    /// to and what every open did before ADR 0038.
    ///
    /// Dropping the manifest first is the whole of it: it makes every run an orphan, and
    /// `RunSet::open` sweeps orphans. See this module's header for why a partial copy is not
    /// something a reader could be allowed to see.
    #[allow(clippy::too_many_arguments)]
    fn rebuild(
        &self,
        db: &Db,
        dir: &Path,
        state: Option<&crate::raft_log::PersistedState>,
        pinned: &Snapshot,
        decoder: Arc<TableDecoder>,
        tenant: u64,
        table_id: u64,
    ) -> Result<(ColumnarApply, Build)> {
        let manifest = dir.join(super::runs::RUNS_MANIFEST);
        if self
            .fs
            .exists(&manifest)
            .map_err(|error| bootstrap(&format!("{}: {error}", manifest.display())))?
        {
            self.fs
                .delete(&manifest)
                .map_err(|error| bootstrap(&format!("{}: {error}", manifest.display())))?;
        }
        let mut apply = ColumnarApply::open(
            Arc::clone(&self.fs),
            dir,
            decoder as Arc<dyn super::RowDecoder>,
            self.options.clone(),
        )?;
        let versions = convert(
            db,
            pinned,
            &mut apply,
            tenant,
            table_id,
            &self.region_bounds(db)?,
        )?;
        // The walk read the region at `pinned`, and the state record says which entry that is —
        // read at the same snapshot, so it can name neither more nor less than the walk covered.
        let to = state.map_or(0, |state| state.applied_index);
        apply.built_to(to)?;
        Ok((
            apply,
            Build {
                from_index: 0,
                to_index: to,
                versions,
                resumed: false,
            },
        ))
    }
}

/// Feeds what the entries after `from`, up to and including `to`, committed for one table.
///
/// The same input the tee gets, taken from the same place: the entry names the keys and the commit
/// timestamp, and the *version* is read back out of the `write` column family, so a resumed copy
/// and a teed one are built from one source of truth.
#[allow(clippy::too_many_arguments)]
fn replay(
    db: &Db,
    pinned: &Snapshot,
    apply: &mut ColumnarApply,
    region_id: u64,
    from: u64,
    to: u64,
    tenant: u64,
    table_id: u64,
) -> Result<usize> {
    let snapshot = EngineSnapshot::at(db, pinned.clone());
    let mut versions = 0;
    for index in (from + 1)..=to {
        let Some(entry) = crate::raft_log::read_entry(db, region_id, index, Some(pinned.clone()))?
        else {
            return Err(bootstrap(&format!(
                "region {region_id} has no log entry {index}, which a resume from {from} needs"
            )));
        };
        if entry.kind != esker_raft::EntryKind::Normal || entry.data.is_empty() {
            continue;
        }
        let command = crate::Command::decode(&entry.data)
            .map_err(|error| bootstrap(&format!("the entry at {index}: {error}")))?;
        let Some((commit_ts, keys)) = crate::peer::commits_of(&command) else {
            continue;
        };
        for key in &keys {
            if row_of(key)? != Some((tenant, table_id)) {
                continue;
            }
            let Some(version) = snapshot
                .seek_write(key, commit_ts)
                .map_err(|error| bootstrap(&format!("reading a committed version: {error}")))?
            else {
                continue;
            };
            // The entry proposed this commit; whether it took is what the `write` record says.
            if version.commit_ts != commit_ts {
                continue;
            }
            match committed_of(&snapshot, key, &version.record)? {
                Committed::Row(value) => {
                    apply.apply(&versioned(key, commit_ts), Some(&value))?;
                    versions += 1;
                }
                Committed::Tombstone => {
                    apply.apply(&versioned(key, commit_ts), None)?;
                    versions += 1;
                }
                Committed::Nothing => {}
            }
        }
        apply.entry_applied(index)?;
    }
    Ok(versions)
}

/// Re-reads every open table's published schema, installing one that has moved forward.
///
/// Off the hot path by construction: only a committed **catalog** write gets here, and a schema
/// version moves only when one does.
fn refresh_schemas(db: &Db, tables: &mut Tables) -> Result<()> {
    for ((tenant, table_id), apply) in &mut tables.open {
        let Some((_, published)) = published_schema(db, *tenant, *table_id)? else {
            continue;
        };
        if published.schema_version <= apply.schema_version() {
            continue;
        }
        apply.install_schema(Arc::new(decoder_of(&published)?), published.schema_version)?;
        tracing::info!(
            tenant,
            table_id,
            schema_version = published.schema_version,
            "a columnar copy took a new schema version"
        );
    }
    Ok(())
}

/// The decoder a published record describes.
///
/// The record carries no column **names** — a fragment addresses columns by index, so nothing on
/// either side needs them — and the run schema needs one per column, so they are positional.
fn decoder_of(published: &esker_keys::columnar::Published) -> Result<TableDecoder> {
    let names: Vec<String> = (0..published.columns.len())
        .map(|index| format!("c{index}"))
        .collect();
    let types: Vec<_> = published.columns.iter().map(|(ty, _)| *ty).collect();
    let missing: Vec<_> = published
        .columns
        .iter()
        .map(|(_, value)| value.clone())
        .collect();
    TableDecoder::new(&names, &types, &missing, published.schema_version)
}

/// The columnar record for a table as **bytes**, for a store answering another store's ask.
///
/// The same read the decoder's own lookup does, and deliberately without the decode: what travels is
/// the record as it is stored, so the asking store parses it with the parser it would have used on
/// its own engine ([`esker_proto::schema`] says why the wire does not learn the format).
pub fn published_record(db: &Db, tenant: u64, table_id: u64) -> Result<Option<Bytes>> {
    let key = esker_keys::columnar::key(tenant, table_id);
    let snapshot = EngineSnapshot::new(db);
    match esker_txn::read(&snapshot, &key, u64::MAX)
        .map_err(|error| bootstrap(&format!("reading a columnar record: {error}")))?
    {
        ReadOutcome::Value(bytes) => Ok(Some(bytes)),
        ReadOutcome::NotFound | ReadOutcome::Locked(_) => Ok(None),
    }
}

/// The columnar record for a table, read as a transaction would read it.
///
/// At `u64::MAX`, which is "whatever is committed": the record is written by the `ALTER` that asks
/// for the copy, and a reader that took an older snapshot would decode later rows against an
/// earlier schema. A **locked** record is one whose transaction has not committed, and answering
/// `None` for it is right: the commit that resolves the lock is itself a catalog write, and this
/// is asked again then.
fn published_schema(
    db: &Db,
    tenant: u64,
    table_id: u64,
) -> Result<Option<(u8, esker_keys::columnar::Published)>> {
    let key = esker_keys::columnar::key(tenant, table_id);
    let snapshot = EngineSnapshot::new(db);
    match esker_txn::read(&snapshot, &key, u64::MAX)
        .map_err(|error| bootstrap(&format!("reading a columnar record: {error}")))?
    {
        ReadOutcome::Value(bytes) => {
            let (replicas, published) = esker_keys::columnar::decode(&bytes)
                .map_err(|error| bootstrap(&format!("a columnar record: {error}")))?;
            Ok(Some((replicas, published)))
        }
        ReadOutcome::NotFound | ReadOutcome::Locked(_) => Ok(None),
    }
}

/// Feeds every committed version of a table's rows that the region already holds.
///
/// Walks the `write` column family, which *is* the region's record of what it has committed —
/// the same place a row read resolves from, so a disagreement between the two engines cannot come
/// from disagreeing about what happened.
fn convert(
    db: &Db,
    pinned: &Snapshot,
    apply: &mut ColumnarApply,
    tenant: u64,
    table_id: u64,
    region: &(Vec<u8>, Vec<u8>),
) -> Result<usize> {
    let (table_start, table_end) = esker_keys::row::table_row_range(tenant, table_id);
    // **The table's range intersected with the region's, and the intersection is the point.**
    //
    // This walked `table_row_range` alone, and a store's `write` column family holds the rows of
    // **every region of that table the store hosts** — so each region's copy was fed the others'
    // rows. With four learners on two stores a bare `count(*)` came back 4×: every fragment
    // answered for more than its shard covered and the SQL node added the shards up. The row path
    // never saw it, because a row read goes to the region that owns the key.
    //
    // Everything else here was already region-scoped — the slot is keyed by region, its directory
    // is named for the region, the live tee sees only its own peer's entries — which is exactly
    // what made one unscoped range hard to find.
    let (start, end) = intersect(&table_start, &table_end, region);
    let low = esker_txn::key::prefix(&start);
    let high = esker_txn::key::prefix(&end);
    if start >= end {
        // The region holds none of this table. Not an error and not a missing copy: a store can
        // host a region whose range does not reach the table at all.
        return Ok(0);
    }
    let snapshot = EngineSnapshot::at(db, pinned.clone());
    let mut rows = 0;

    let mut iter = db
        .iter(
            cf::WRITE,
            &ReadOptions {
                snapshot: Some(pinned.clone()),
                ..ReadOptions::default()
            },
        )
        .map_err(|error| bootstrap(&format!("walking the write column family: {error}")))?;
    iter.seek(&low);
    while iter.valid() && iter.key() < high.as_slice() {
        let (user_key, commit_ts) = esker_txn::key::split(iter.key())
            .map_err(|error| bootstrap(&format!("a write key: {error}")))?;
        let record = WriteRecord::decode(iter.value())
            .map_err(|error| bootstrap(&format!("a write record: {error}")))?;
        match committed_of(&snapshot, &user_key, &record)? {
            Committed::Row(value) => {
                apply.apply(&versioned(&user_key, commit_ts), Some(&value))?;
                rows += 1;
            }
            Committed::Tombstone => {
                apply.apply(&versioned(&user_key, commit_ts), None)?;
                rows += 1;
            }
            Committed::Nothing => {}
        }
        iter.next();
    }
    iter.status()
        .map_err(|error| bootstrap(&format!("walking the write column family: {error}")))?;
    // **What the conversion actually saw**, which is the question every disagreement between the
    // two engines turns into: a copy missing a row either never walked its key or walked it and
    // found nothing committed. Run 127i had no way to tell those apart.
    tracing::debug!(
        tenant,
        table_id,
        rows,
        low = %printable(&low),
        high = %printable(&high),
        "converted a table's history from the write column family"
    );
    Ok(rows)
}

/// A key as a log line can carry it.
fn printable(key: &[u8]) -> String {
    key.iter()
        .map(|byte| {
            if byte.is_ascii_graphic() {
                (*byte as char).to_string()
            } else {
                format!("\\x{byte:02x}")
            }
        })
        .collect()
}

/// `[table_start, table_end)` narrowed to the region's own `[start, end)`.
///
/// **An empty bound means opposite things on the two sides**, which is the reading this has to get
/// right: an empty region `start` is the beginning of the key space and constrains nothing, an
/// empty region `end` is the end of it and constrains nothing either. A table range is never
/// empty-bounded, so the result is always concrete.
fn intersect(
    table_start: &[u8],
    table_end: &[u8],
    region: &(Vec<u8>, Vec<u8>),
) -> (Vec<u8>, Vec<u8>) {
    let (region_start, region_end) = region;
    let start = if region_start.as_slice() > table_start {
        region_start.clone()
    } else {
        table_start.to_vec()
    };
    let end = if region_end.is_empty() || region_end.as_slice() > table_end {
        table_end.to_vec()
    } else {
        region_end.clone()
    };
    (start, end)
}

/// What one `write` record is, to the columnar copy.
#[derive(Debug)]
enum Committed {
    /// A version, with the row it wrote.
    Row(Bytes),
    /// A version that deleted the key. A tombstone is a version like any other: a read *before* it
    /// must still find the row, which is why it is stored rather than skipped.
    Tombstone,
    /// Not a version at all — a rollback marker, or a lock-only entry.
    Nothing,
}

/// Reads what a `write` record committed.
fn committed_of(
    snapshot: &EngineSnapshot<'_>,
    user_key: &[u8],
    record: &WriteRecord,
) -> Result<Committed> {
    match record.kind {
        Kind::Put => {
            let value = match &record.short_value {
                Some(inline) => Some(inline.clone()),
                None => snapshot
                    .get_value(user_key, record.start_ts)
                    .map_err(|error| bootstrap(&format!("a committed value: {error}")))?,
            };
            match value {
                Some(value) => Ok(Committed::Row(value)),
                // A `Put` whose value is absent is corruption, and the row path says so too
                // (`TxnSnapshot::get_value`). Refusing here rather than writing a NULL row keeps
                // the two engines from disagreeing quietly.
                None => Err(bootstrap(
                    "a committed Put with no value in the default column family",
                )),
            }
        }
        Kind::Delete => Ok(Committed::Tombstone),
        Kind::Rollback | Kind::Lock => Ok(Committed::Nothing),
    }
}

/// `user_key ++ enc_ts(commit_ts)`, which is what [`ColumnarApply::apply`] splits.
fn versioned(user_key: &[u8], commit_ts: u64) -> Vec<u8> {
    let mut key = esker_txn::key::write(user_key, commit_ts);
    // `key::write` is `'x' ++ user_key ++ enc_ts`; the apply target wants it without the
    // namespace byte, because a version's identity is the user key.
    key.remove(0);
    key
}

/// The table a key's rows belong to, or `None` for anything that is not a row.
fn row_of(key: &[u8]) -> Result<Option<(u64, u64)>> {
    match prefix::split_table(key).map_err(|error| bootstrap(&format!("a table key: {error}")))? {
        Some((tenant, table_id, TablePart::Row)) => Ok(Some((tenant, table_id))),
        _ => Ok(None),
    }
}

/// Whether a key is a catalog record — the one thing that changes what a table wants.
fn is_meta(key: &[u8]) -> bool {
    key.first() == Some(&b'm')
}

fn bootstrap(detail: &str) -> StoreError {
    StoreError::Bootstrap(format!("the columnar copy: {detail}"))
}
