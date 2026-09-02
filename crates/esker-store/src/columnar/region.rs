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
//! complete one — so **opening a table's target rebuilds it** from the region's own committed
//! state and then extends it from the log (the plan's "reuse then convert", applied at every open
//! rather than only after a snapshot). A columnar copy is therefore complete by construction at
//! every point a reader can observe it, at the cost of a walk per open. Making that incremental
//! needs the manifest to carry the applied index its runs are complete to, which is the follow-up
//! this note exists to name.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use esker_columnar::Schema;
use esker_engine::fs::FileSystem;
use esker_engine::{Db, ReadOptions, cf};
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
    options: ColumnarOptions,
    tables: Mutex<Tables>,
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
    pub fn new(fs: Arc<dyn FileSystem>, dir: impl AsRef<Path>, options: ColumnarOptions) -> Self {
        Self {
            fs,
            dir: dir.as_ref().to_path_buf(),
            options,
            tables: Mutex::new(Tables::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Tables> {
        self.tables.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ingests the versions one committed transaction made visible.
    ///
    /// Called from the apply path **after** the batch is durable, so every version it reads is one
    /// this region has actually committed — the same records a row read would resolve, rather than
    /// a prediction made from the command.
    pub fn commit(&self, db: &Db, commit_ts: u64, keys: &[Bytes]) -> Result<()> {
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
        for key in keys {
            let Some((tenant, table_id)) = row_of(key)? else {
                continue;
            };
            let built = self.ensure(db, &mut tables, tenant, table_id)?;
            if built {
                converted.insert((tenant, table_id));
            }
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

    /// Whether this store can build a decoder for `(tenant, table_id)` without asking anyone.
    ///
    /// True when the record has already been fetched, and when this store's own engine holds it —
    /// which is the same read [`ensure`](Self::ensure) would do, so a `true` here is a promise the
    /// next call can keep. False is not "there is no columnar copy": it is "this store cannot say",
    /// and the answer to it is [`Store::ensure_schema`](crate::server::Store), not a refusal.
    ///
    /// An error reading the engine answers `false` as well. The caller's next move is a fetch,
    /// which either succeeds or leaves the table refused; turning a transient read failure into a
    /// hard error here would fail a fragment that a row scan could have answered.
    #[must_use]
    pub fn knows_schema(&self, db: &Db, tenant: u64, table_id: u64) -> bool {
        if self.lock().fetched.contains_key(&(tenant, table_id)) {
            return true;
        }
        matches!(published_schema(db, tenant, table_id), Ok(Some(_)))
    }

    /// Installs a columnar record fetched from a store that holds the catalog.
    ///
    /// Takes the **bytes** and decodes them here, so a record that arrived over the wire and one
    /// read from this store's own engine go through one parser
    /// ([`esker_keys::columnar::decode`]) and cannot come to disagree about the format.
    ///
    /// A record older than the one already held is dropped rather than installed. Nothing orders
    /// two fetches — a slow answer from one store can land after a fast one from another — and
    /// installing an older schema over a newer one would make the copy refuse rows it had already
    /// decoded. `schema_version` is monotonic per table and is exactly the comparison
    /// `esker_keys::columnar::Published` documents itself for.
    ///
    /// Any table whose copy is open is **closed**, so the next read rebuilds it under the new
    /// schema rather than extending a copy built under the old one.
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
        // The rebuild, and the whole of it: dropping the manifest makes every run an orphan, and
        // `RunSet::open` sweeps orphans. See this module's header for why a partial copy is not
        // something a reader could be allowed to see.
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
        let decoder = decoder_of(&published)?;
        let mut apply = ColumnarApply::open(
            Arc::clone(&self.fs),
            &dir,
            Arc::new(decoder),
            self.options.clone(),
        )?;
        let rows = convert(db, &mut apply, tenant, table_id)?;
        apply.seal()?;
        tracing::info!(
            tenant,
            table_id,
            rows,
            replicas,
            schema_version = published.schema_version,
            dir = %dir.display(),
            "built a columnar copy of a table from the region's own committed state"
        );
        tables.open.insert((tenant, table_id), apply);
        Ok(true)
    }
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
/// The same read [`published_schema`] does and deliberately without the decode: what travels is
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
fn convert(db: &Db, apply: &mut ColumnarApply, tenant: u64, table_id: u64) -> Result<usize> {
    let (start, end) = esker_keys::row::table_row_range(tenant, table_id);
    let low = esker_txn::key::prefix(&start);
    let high = esker_txn::key::prefix(&end);
    let snapshot = EngineSnapshot::new(db);
    let mut rows = 0;

    let mut iter = db
        .iter(cf::WRITE, &ReadOptions::default())
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
    Ok(rows)
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
