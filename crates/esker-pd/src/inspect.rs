//! A **read-only** view of a stopped placement driver's files, for `esker pd inspect`.
//!
//! Its own type rather than a flag on [`Pd`](crate::pd::Pd), because opening a placement driver
//! now *does something*: a member of one campaigns inside `Pd::open` and wins with a quorum of
//! itself, which appends a `TakeOffice` entry and a hard state ([`crate::driver`]). That is right
//! for a process that is about to serve and wrong for a tool that is about to print — an inspector
//! must not create or move what it was asked to look at, and a typo in a path should be an error
//! rather than a database that looks like a wiped cluster.
//!
//! So this opens the files, reads them, and writes nothing:
//!
//! * **no column family is created.** `Db::open_with` creates any family it is *named*, so this
//!   names none and asks afterwards. A directory written before PD had a Raft log has no `raft`
//!   family, and the honest report of that is "no consensus state", not a family created by the
//!   act of looking.
//! * **no driver is started**, so nothing campaigns, nothing applies and nothing is proposed.
//! * **nothing that is only in memory can be shown.** The in-flight operator set dies with the
//!   process by decision (ADR 0013), and so does which member currently leads — `esker pd members`
//!   asks a *running* group for that, and this reports the durable state it left behind: the term
//!   it reached, who it last voted for, and how far it had applied.

use std::path::Path;
use std::sync::Arc;

use esker_engine::{Db, FileSystem, LocalFileSystem, Options, ReadOptions, cf};

use crate::error::{PdError, Result};
use crate::keys;
use crate::raft_log::{PersistedState, state_key};
use crate::record::{
    AllocRecord, ClusterRecord, ColumnarRecord, HistoryRecord, OperatorEvent, RegionRecord,
    StoreRecord, TsoRecord,
};
use crate::routing;

/// What a stopped placement driver left on disk.
#[derive(Debug)]
pub struct PdInspector {
    db: Arc<Db>,
    /// `None` on a directory written before PD had a Raft log at all.
    raft: Option<PersistedState>,
}

impl PdInspector {
    /// Opens `path` without creating anything.
    ///
    /// The engine options are the caller's, minus the one decision this type exists to make:
    /// `create_if_missing` is forced off, because an inspector that created a database would
    /// answer "empty cluster" to a mistyped path.
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        let options = Options {
            create_if_missing: false,
            ..options
        };
        // No families named, so none is created: what is there is what is reported.
        let db = Db::open_with(
            path,
            options,
            Arc::new(LocalFileSystem::new()) as Arc<dyn FileSystem>,
            &[],
        )?;
        let raft = match db.cf_id(cf::RAFT) {
            None => None,
            Some(_) => db
                .get(cf::RAFT, &state_key(), &ReadOptions::default())?
                .map(|bytes| PersistedState::decode(&bytes))
                .transpose()?,
        };
        Ok(Self {
            db: Arc::new(db),
            raft,
        })
    }

    /// The database, for a caller that wants to read something this type does not name.
    #[must_use]
    pub fn db(&self) -> &Arc<Db> {
        &self.db
    }

    /// The cluster record, or `None` on a placement driver nothing has bootstrapped.
    pub fn cluster(&self) -> Result<Option<ClusterRecord>> {
        self.read(&keys::cluster_key())?
            .map(|bytes| ClusterRecord::decode(&bytes))
            .transpose()
    }

    /// The oracle's high-water mark. Every timestamp ever handed out is strictly below it.
    pub fn tso_high_water_ms(&self) -> Result<u64> {
        Ok(self
            .read(&keys::tso_key())?
            .map(|bytes| TsoRecord::decode(&bytes))
            .transpose()?
            .map_or(0, |record| record.high_water_ms))
    }

    /// The last id reserved. The next one this cluster hands out is above it.
    pub fn allocated_end(&self) -> Result<u64> {
        Ok(self
            .read(&keys::alloc_key())?
            .map(|bytes| AllocRecord::decode(&bytes))
            .transpose()?
            .map_or(0, |record| record.allocated_end))
    }

    /// Every region, in id order.
    pub fn regions(&self) -> Result<Vec<RegionRecord>> {
        routing::regions(&self.db)
    }

    /// Every store, in id order.
    pub fn stores(&self) -> Result<Vec<StoreRecord>> {
        routing::stores(&self.db)
    }

    /// The range index, as `(key, region id)` pairs in key order.
    pub fn range_index(&self) -> Result<Vec<(Vec<u8>, u64)>> {
        routing::range_index(&self.db)
    }

    /// The bounded ring of recent operator events, oldest first.
    pub fn history(&self) -> Result<Vec<OperatorEvent>> {
        Ok(self
            .read(&keys::history_key())?
            .map(|bytes| HistoryRecord::decode(&bytes))
            .transpose()?
            .unwrap_or_default()
            .events)
    }

    /// What the SQL layer last said about columnar placement.
    pub fn columnar(&self) -> Result<ColumnarRecord> {
        Ok(self
            .read(&keys::columnar_key())?
            .map(|bytes| ColumnarRecord::decode(&bytes))
            .transpose()?
            .unwrap_or_default())
    }

    /// The durable half of this member's consensus state, or `None` if it has none.
    ///
    /// Not who leads — that is a live fact and `esker pd members` is what asks a running group for
    /// it. This is what the member would resume from: the term it reached, the vote it cast in
    /// that term, how far it committed and how far it applied.
    #[must_use]
    pub fn raft(&self) -> Option<&PersistedState> {
        self.raft.as_ref()
    }

    fn read(&self, key: &[u8]) -> Result<Option<bytes::Bytes>> {
        self.db
            .get(cf::DEFAULT, key, &ReadOptions::default())
            .map_err(PdError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::PdInspector;
    use crate::clock::TestClock;
    use crate::pd::{Pd, PdOptions};
    use crate::{Clock, keys};
    use esker_engine::{Options, ReadOptions, cf};
    use std::sync::Arc;

    fn written(dir: &std::path::Path) -> u64 {
        let pd = Pd::open(
            dir,
            PdOptions::with_clock(Arc::new(TestClock::new(1_700_000_000_000)) as Arc<dyn Clock>),
        )
        .unwrap();
        let cluster_id = pd.bootstrap(1, "127.0.0.1:20160").unwrap().cluster_id;
        pd.alloc_id(3).unwrap();
        pd.tso(1).unwrap();
        cluster_id
    }

    #[test]
    fn it_reads_what_a_stopped_placement_driver_left() {
        let dir = tempfile::tempdir().unwrap();
        let cluster_id = written(dir.path());

        let look = PdInspector::open(dir.path(), Options::default()).unwrap();
        assert_eq!(
            look.cluster().unwrap().map(|cluster| cluster.cluster_id),
            Some(cluster_id)
        );
        assert!(look.tso_high_water_ms().unwrap() > 0);
        assert!(look.allocated_end().unwrap() >= 5);
        assert_eq!(look.regions().unwrap().len(), 1);
        assert_eq!(look.stores().unwrap().len(), 1);
        assert_eq!(look.range_index().unwrap().len(), 1);
        let raft = look.raft().expect("a raft state record");
        assert!(raft.applied_index > 0, "nothing had applied");
        assert_eq!(raft.hard_state.voted_for, Some(1), "it voted for itself");
    }

    /// **The reason this type exists.** Opening a placement driver campaigns, and a campaign is a
    /// write; an inspector must leave the bytes exactly as it found them.
    #[test]
    fn looking_at_a_placement_driver_does_not_change_it() {
        let dir = tempfile::tempdir().unwrap();
        written(dir.path());

        let before = PdInspector::open(dir.path(), Options::default()).unwrap();
        let (term, applied) = {
            let raft = before.raft().expect("a raft state record");
            (raft.hard_state.term, raft.applied_index)
        };
        drop(before);

        for _ in 0..3 {
            let look = PdInspector::open(dir.path(), Options::default()).unwrap();
            let raft = look.raft().expect("a raft state record");
            assert_eq!(raft.hard_state.term, term, "looking moved the term");
            assert_eq!(raft.applied_index, applied, "looking applied an entry");
        }
    }

    /// A mistyped path is an error, not an empty database that reads like a wiped cluster.
    #[test]
    fn a_directory_that_is_not_a_placement_driver_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(PdInspector::open(dir.path().join("nowhere"), Options::default()).is_err());
    }

    /// A directory written before PD had a Raft log has no `raft` family, and looking at it must
    /// not create one: the honest report is "no consensus state".
    #[test]
    fn a_directory_with_no_raft_family_reports_none_and_gains_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = esker_engine::Db::open_with(
            dir.path(),
            Options {
                create_if_missing: true,
                ..Options::default()
            },
            Arc::new(esker_engine::LocalFileSystem::new()),
            &[cf::DEFAULT],
        )
        .unwrap();
        let mut batch = esker_engine::WriteBatch::new();
        let id = db.cf_id(cf::DEFAULT).unwrap();
        batch.put(
            id,
            &keys::tso_key(),
            &crate::record::TsoRecord { high_water_ms: 7 }.encode(),
        );
        db.write(batch, &esker_engine::WriteOptions::synced())
            .unwrap();
        drop(db);

        let look = PdInspector::open(dir.path(), Options::default()).unwrap();
        assert!(look.raft().is_none(), "a raft family appeared from nowhere");
        assert_eq!(look.tso_high_water_ms().unwrap(), 7);
        assert!(
            look.db().cf_id(cf::RAFT).is_none(),
            "looking created the raft column family"
        );
        // And the records are still readable through the plain engine afterwards.
        assert!(
            look.db()
                .get(cf::DEFAULT, &keys::tso_key(), &ReadOptions::default())
                .unwrap()
                .is_some()
        );
    }
}
