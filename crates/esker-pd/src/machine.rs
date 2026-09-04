//! `apply`: the pure function from a committed [`Command`] to PD's records.
//!
//! This is the **only** writer of the six records of
//! [ADR 0010](../../../docs/adr/0010-pd-durable-state.md). Everything above it decides *what* to
//! ask for; this decides what the bytes become, and it does so identically on every member,
//! because that is what a replicated state machine is.
//!
//! # What "pure" means here, exactly
//!
//! `apply` is a function of `(the state machine's own state, the command)` and nothing else. It
//! reads the engine — which is the state machine's state, and is byte-identical on every member at
//! the same log position — and it reads no clock, no configuration and no network. A `now_ms` it
//! needs arrives as a field of the command, sampled once by the leader
//! ([`crate::command`]).
//!
//! It also never **refuses**. A request PD will not serve is refused by the leader, before the
//! propose; by the time a command is in the log the answer is a decision, not a rejection. The one
//! decision `apply` does make is [`crate::routing::accepts`] — the epoch guard — because there the
//! log's order *is* the answer.
//!
//! # The two counters move by `max`
//!
//! `allocated_end` and `high_water_ms` only ever advance, and writing `max(carried, held)` makes
//! those two applies idempotent in the direction that matters: a stale entry re-proposed by a
//! recovering leader cannot walk a mark backwards. It costs a comparison and removes a class of
//! reasoning about replay.

use std::sync::{Arc, RwLock};

use bytes::Bytes;
use esker_engine::{Db, ReadOptions, WriteBatch, cf};
use esker_proto::{Encoder, Region, StoreInfo};

use crate::clock::Clock;
use crate::command::Command;
use crate::error::{PdError, Result};
use crate::keys;
use crate::record::{
    AllocRecord, ClusterRecord, ColumnarRecord, HistoryRecord, RegionRecord, StoreRecord, TsoRecord,
};
use crate::routing::{self, Upsert};

/// Version byte on a state-machine snapshot.
const SNAPSHOT_VERSION: u8 = 1;

/// What one applied command answers its proposer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Nothing to say beyond "it applied".
    Done,
    /// A `Bootstrap`: the cluster this store belongs to, and the region it is to create.
    Bootstrapped(crate::pd::Bootstrapped),
    /// A `RegionBeat`: whether the epoch guard let it through.
    Upserted(Upsert),
}

/// The records the state machine holds in memory as well as on disk.
///
/// Written **only** by [`Machine::apply`], on the driver thread; read by the leader's decisions,
/// by the service and by the inspector. It is a separate lock from `Pd`'s own state on purpose:
/// a leader's `Allocator` calls a persist that blocks on the driver, so a driver that needed
/// `Pd`'s lock to apply would deadlock against the very call it is unblocking.
#[derive(Debug, Default)]
pub struct AppliedState {
    /// The cluster record, or `None` on a PD that nothing has bootstrapped.
    pub cluster: Option<ClusterRecord>,
    /// The last id reserved. A leader resumes at `allocated_end + 1`.
    pub allocated_end: u64,
    /// The oracle's high-water mark. A leader resumes at `max(clock, high_water_ms)`.
    pub high_water_ms: u64,
    /// The bounded ring of recent operator events.
    pub history: HistoryRecord,
    /// Which key ranges want columnar replicas.
    pub columnar: ColumnarRecord,
}

/// PD's replicated state machine.
#[derive(Debug)]
pub struct Machine {
    db: Arc<Db>,
    cf: u32,
    /// PD's one wall clock, so the driver can stamp a `TakeOffice` without a second one existing.
    ///
    /// **Never read by [`Machine::apply`]** — that is the whole rule of this module. It is here
    /// because the driver has no other route to the clock `PdOptions` was given, and one clock
    /// with a stated single reader is safer than two.
    clock: Arc<dyn Clock>,
    applied: Arc<RwLock<AppliedState>>,
}

impl Machine {
    /// Loads the state machine from what is already on disk.
    ///
    /// The same five reads `Pd::open` did before there was a log, for the same reason: these are
    /// the facts no heartbeat can re-state.
    pub fn load(db: Arc<Db>, cf: u32, clock: Arc<dyn Clock>) -> Result<Self> {
        let read = |key: &[u8]| -> Result<Option<Bytes>> {
            Ok(db.get(cf::DEFAULT, key, &ReadOptions::default())?)
        };
        let state = AppliedState {
            cluster: read(&keys::cluster_key())?
                .map(|bytes| ClusterRecord::decode(&bytes))
                .transpose()?,
            allocated_end: read(&keys::alloc_key())?
                .map(|bytes| AllocRecord::decode(&bytes))
                .transpose()?
                .map_or(0, |record| record.allocated_end),
            high_water_ms: read(&keys::tso_key())?
                .map(|bytes| TsoRecord::decode(&bytes))
                .transpose()?
                .map_or(0, |record| record.high_water_ms),
            history: read(&keys::history_key())?
                .map(|bytes| HistoryRecord::decode(&bytes))
                .transpose()?
                .unwrap_or_default(),
            columnar: read(&keys::columnar_key())?
                .map(|bytes| ColumnarRecord::decode(&bytes))
                .transpose()?
                .unwrap_or_default(),
        };
        Ok(Self {
            db,
            cf,
            clock,
            applied: Arc::new(RwLock::new(state)),
        })
    }

    /// PD's clock. Read by the driver to stamp a `TakeOffice`, and by nothing in [`Machine::apply`].
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The applied records, shared with everything that reads them.
    #[must_use]
    pub fn applied(&self) -> &Arc<RwLock<AppliedState>> {
        &self.applied
    }

    /// Applies one command, staging its writes into `batch`.
    ///
    /// The caller puts the apply index in the **same** batch and writes it with `sync = true`, so
    /// that "this command applied" is one fact on disk rather than two that can disagree after a
    /// crash. The in-memory mirrors are moved here rather than after the write, which is only safe
    /// because a failed write is fatal to the member: a PD whose disk will not take an apply must
    /// stop, not carry on with a state its neighbours do not share.
    pub fn apply(&self, command: &Command, batch: &mut WriteBatch) -> Result<Answer> {
        match command {
            // A barrier, and nothing else. Its whole value is where it sits in the log: applying
            // it means every entry before it has applied, which is what lets a new leader trust
            // the allocator and the oracle it is about to rebuild.
            Command::TakeOffice { .. } => Ok(Answer::Done),

            Command::Bootstrap {
                store_id,
                address,
                base_id,
                cluster_id,
                now_ms,
            } => self.bootstrap(batch, *store_id, address, *base_id, *cluster_id, *now_ms),

            Command::ReserveIds { end } => {
                let mut applied = self.write_lock()?;
                applied.allocated_end = applied.allocated_end.max(*end);
                batch.put(
                    self.cf,
                    &keys::alloc_key(),
                    &AllocRecord {
                        allocated_end: applied.allocated_end,
                    }
                    .encode(),
                );
                Ok(Answer::Done)
            }

            Command::AdvanceTso { mark } => {
                let mut applied = self.write_lock()?;
                applied.high_water_ms = applied.high_water_ms.max(*mark);
                batch.put(
                    self.cf,
                    &keys::tso_key(),
                    &TsoRecord {
                        high_water_ms: applied.high_water_ms,
                    }
                    .encode(),
                );
                Ok(Answer::Done)
            }

            Command::StoreBeat {
                store_id,
                stats,
                now_ms,
            } => {
                let Some(existing) = routing::read_store(&self.db, *store_id)? else {
                    // Unreachable: the leader refuses a beat from a store it has no record of,
                    // and a store record is never deleted, so one that existed at the propose
                    // exists at the apply. Loud rather than silent, because if it ever is
                    // reachable the beat is being dropped and liveness will look wrong.
                    tracing::error!(
                        store_id,
                        "a store heartbeat applied for a store with no record"
                    );
                    return Ok(Answer::Done);
                };
                routing::stage_store(
                    batch,
                    self.cf,
                    &StoreRecord {
                        stats: *stats,
                        last_heartbeat_ms: *now_ms,
                        ..existing
                    },
                );
                Ok(Answer::Done)
            }

            Command::RegionBeat { record } => {
                let previous = routing::read_region(&self.db, record.region.id)?;
                // The epoch guard, and the one decision this function makes. Heartbeats cross on
                // the network whenever a leader changes, so the order PD accepts them in is the
                // order they happened — and under Raft that order is the log's, which is the same
                // on every member.
                if previous
                    .as_ref()
                    .is_some_and(|held| !routing::accepts(held, record.region.epoch, record.term))
                {
                    tracing::debug!(
                        region_id = record.region.id,
                        "dropping a heartbeat older than the record it would replace"
                    );
                    return Ok(Answer::Upserted(Upsert::Stale));
                }
                routing::stage_region(batch, self.cf, record, previous.as_ref());
                Ok(Answer::Upserted(Upsert::Applied))
            }

            Command::Columnar { wishes } => {
                let mut applied = self.write_lock()?;
                applied.columnar = ColumnarRecord {
                    wishes: wishes.clone(),
                };
                batch.put(self.cf, &keys::columnar_key(), &applied.columnar.encode());
                Ok(Answer::Done)
            }

            Command::History { event } => {
                let mut applied = self.write_lock()?;
                applied.history.push(*event);
                batch.put(self.cf, &keys::history_key(), &applied.history.encode());
                Ok(Answer::Done)
            }
        }
    }

    fn bootstrap(
        &self,
        batch: &mut WriteBatch,
        store_id: u64,
        address: &str,
        base_id: u64,
        cluster_id: u64,
        now_ms: u64,
    ) -> Result<Answer> {
        let mut applied = self.write_lock()?;
        let store = self.store_record(store_id, address, now_ms)?;

        // Idempotent by shape, and it has to be: a `Bootstrap` proposed by a leader that then lost
        // office can still commit under its successor, beside the successor's own. Whichever is
        // second finds a cluster and refreshes an address, which is exactly what a restarting
        // store's `Bootstrap` is for anyway.
        if let Some(cluster) = applied.cluster {
            routing::stage_store(batch, self.cf, &store);
            return Ok(Answer::Bootstrapped(crate::pd::Bootstrapped {
                cluster_id: cluster.cluster_id,
                region: None,
            }));
        }

        let region = Region::bootstrap(base_id, store_id, base_id + 1);
        let cluster = ClusterRecord {
            cluster_id,
            first_region_id: region.id,
            created_ms: now_ms,
        };
        batch.put(self.cf, &keys::cluster_key(), &cluster.encode());
        routing::stage_store(batch, self.cf, &store);
        routing::stage_region(
            batch,
            self.cf,
            &RegionRecord::new(region.clone(), now_ms),
            None,
        );
        applied.cluster = Some(cluster);
        tracing::info!(
            cluster_id = cluster.cluster_id,
            region_id = region.id,
            store_id,
            "cluster bootstrapped"
        );
        Ok(Answer::Bootstrapped(crate::pd::Bootstrapped {
            cluster_id: cluster.cluster_id,
            region: Some(region),
        }))
    }

    /// The store record to write for a registering store, keeping the stats it already reported.
    fn store_record(&self, store_id: u64, address: &str, now_ms: u64) -> Result<StoreRecord> {
        let existing = routing::read_store(&self.db, store_id)?;
        Ok(StoreRecord {
            store_id,
            address: address.to_owned(),
            started_ms: now_ms,
            last_heartbeat_ms: now_ms,
            stats: existing.map(|store| store.stats).unwrap_or_default(),
        })
    }

    /// Every key and value the state machine holds, for a snapshot a leader can offer.
    ///
    /// The whole of the `default` column family, as pairs. PD's state is small by construction —
    /// a cluster record, one record per store, two per region, and four singletons — so a
    /// thousand-region cluster is a couple of hundred kilobytes, and a format that streams would
    /// be machinery with nothing to carry. [`Machine::install`] is its inverse.
    pub fn snapshot(&self) -> Result<Bytes> {
        let mut out = Encoder::new();
        out.put_u8(SNAPSHOT_VERSION);
        let mut count = 0_u64;
        let mut pairs = Encoder::new();
        let mut iter = self.db.iter(cf::DEFAULT, &ReadOptions::default())?;
        iter.seek_to_first();
        while iter.valid() {
            pairs.put_bytes(iter.key());
            pairs.put_bytes(iter.value());
            count += 1;
            iter.next();
        }
        iter.status()?;
        out.put_varint(count);
        out.put_bytes(pairs.as_slice());
        Ok(Bytes::from(out.finish()))
    }

    /// Replaces the state machine's contents with a snapshot's.
    ///
    /// The old contents go first, in the **same** batch: a snapshot is a replacement, and merging
    /// it over what is there would leave a region the snapshot does not have still routable.
    pub fn install(&self, data: &[u8], batch: &mut WriteBatch) -> Result<()> {
        const WHAT: &str = "pd snapshot";
        let bad = |error: esker_proto::DecodeError| PdError::corrupt(WHAT, error.to_string());

        let mut input = esker_proto::Decoder::new(data);
        let version = input.get_u8("snapshot.version").map_err(bad)?;
        if version != SNAPSHOT_VERSION {
            return Err(PdError::corrupt(
                WHAT,
                format!("format version {version}, expected {SNAPSHOT_VERSION}"),
            ));
        }
        let count = input.get_count("snapshot.count").map_err(bad)?;
        let body = input.get_bytes("snapshot.pairs").map_err(bad)?;
        input.finish().map_err(bad)?;

        // Everything under PD's prefix, which is everything PD stores.
        batch.delete_range(self.cf, &[keys::PREFIX], &[keys::PREFIX + 1]);
        let mut pairs = esker_proto::Decoder::new(body);
        for _ in 0..count {
            let key = pairs.get_bytes("snapshot.key").map_err(bad)?;
            let value = pairs.get_bytes("snapshot.value").map_err(bad)?;
            batch.put(self.cf, key, value);
        }
        pairs.finish().map_err(bad)?;
        Ok(())
    }

    /// Rereads the in-memory mirrors from the engine, after a snapshot replaced it.
    pub fn reload(&self) -> Result<()> {
        let fresh = Self::load(Arc::clone(&self.db), self.cf, Arc::clone(&self.clock))?;
        let state = fresh
            .applied
            .write()
            .map_err(|_| PdError::internal("pd applied state lock is poisoned"))?;
        let mut mine = self.write_lock()?;
        mine.cluster = state.cluster;
        mine.allocated_end = state.allocated_end;
        mine.high_water_ms = state.high_water_ms;
        mine.history = state.history.clone();
        mine.columnar = state.columnar.clone();
        Ok(())
    }

    fn write_lock(&self) -> Result<std::sync::RwLockWriteGuard<'_, AppliedState>> {
        self.applied
            .write()
            .map_err(|_| PdError::internal("pd applied state lock is poisoned"))
    }
}

/// The stores hosting a region's peers, in peer order, skipping any PD has never heard of.
///
/// Here rather than in [`crate::pd`] because it is a read of the state machine's records and two
/// callers want it: routing a key and paging the table.
pub fn stores_for(db: &Db, region: &Region) -> Result<Vec<StoreInfo>> {
    let mut stores = Vec::with_capacity(region.peers.len());
    for peer in &region.peers {
        if let Some(store) = routing::read_store(db, peer.store_id)? {
            stores.push(StoreInfo::new(store.store_id, store.address));
        }
    }
    Ok(stores)
}

#[cfg(test)]
mod tests {
    use super::{Answer, Machine};
    use crate::clock::TestClock;
    use crate::command::Command;
    use crate::record::{RegionRecord, StoreStats};
    use crate::routing::Upsert;
    use crate::{Clock, keys};
    use esker_engine::{Db, Options, ReadOptions, WalSyncMode, WriteBatch, WriteOptions, cf};
    use esker_proto::{Epoch, Peer, Region};
    use std::sync::Arc;

    /// A machine over a fresh database, with a clock set to `now_ms`.
    fn machine(now_ms: u64) -> (tempfile::TempDir, Machine) {
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
        let cf = db.cf_id(cf::DEFAULT).unwrap();
        let clock = Arc::new(TestClock::new(now_ms)) as Arc<dyn Clock>;
        let machine = Machine::load(Arc::new(db), cf, clock).unwrap();
        (dir, machine)
    }

    /// Applies a command and writes what it staged, the way the driver does.
    fn apply(machine: &Machine, command: &Command) -> Answer {
        let mut batch = WriteBatch::new();
        let answer = machine.apply(command, &mut batch).unwrap();
        machine
            .db
            .write(batch, &WriteOptions::synced())
            .expect("the batch");
        answer
    }

    fn region(id: u64, epoch: Epoch) -> Region {
        Region {
            id,
            start_key: bytes::Bytes::new(),
            end_key: bytes::Bytes::new(),
            epoch,
            peers: vec![Peer::voter(1, id + 100)],
        }
    }

    fn beat(id: u64, epoch: Epoch, term: u64, now_ms: u64) -> Command {
        Command::RegionBeat {
            record: RegionRecord {
                region: region(id, epoch),
                leader_peer_id: id + 100,
                term,
                approximate_size: 1,
                applied_index: 1,
                last_heartbeat_ms: now_ms,
            },
        }
    }

    fn script(now_ms: u64) -> Vec<Command> {
        vec![
            Command::TakeOffice { term: 1, now_ms },
            Command::Bootstrap {
                store_id: 1,
                address: "127.0.0.1:20160".to_owned(),
                base_id: 1,
                cluster_id: 0xABCD,
                now_ms,
            },
            Command::ReserveIds { end: 1_000 },
            Command::AdvanceTso {
                mark: now_ms + 3_000,
            },
            Command::StoreBeat {
                store_id: 1,
                stats: StoreStats {
                    capacity: 100,
                    available: 50,
                    region_count: 1,
                    leader_count: 1,
                    applied_bytes: 7,
                },
                now_ms,
            },
            beat(1, Epoch::new(1, 1), 2, now_ms),
        ]
    }

    /// **The rule of this module, stated as a test.** Two members apply the same log and reach
    /// the same bytes — so the clocks they hold must make no difference at all. A `now_ms` read
    /// inside `apply` rather than taken from the command turns this red, which is the only thing
    /// that would.
    #[test]
    fn apply_is_a_function_of_the_command_and_never_of_the_clock() {
        let (_a_dir, a) = machine(1_700_000_000_000);
        let (_b_dir, b) = machine(1);
        for command in script(1_700_000_000_000) {
            apply(&a, &command);
            apply(&b, &command);
        }
        assert_eq!(
            a.snapshot().unwrap(),
            b.snapshot().unwrap(),
            "two members with different clocks reached different states"
        );
    }

    /// Both counters are monotone, so an entry that applies out of order — or twice — must not
    /// walk one backwards. This is what makes `max` rather than assignment load-bearing.
    #[test]
    fn the_two_counters_only_ever_advance() {
        let (_dir, machine) = machine(1_000);
        apply(&machine, &Command::ReserveIds { end: 1_000 });
        apply(&machine, &Command::ReserveIds { end: 500 });
        apply(&machine, &Command::AdvanceTso { mark: 9_000 });
        apply(&machine, &Command::AdvanceTso { mark: 4_000 });

        let applied = machine.applied().read().unwrap();
        assert_eq!(applied.allocated_end, 1_000);
        assert_eq!(applied.high_water_ms, 9_000);
        drop(applied);

        // And on disk, not only in the mirror: a restart reads the record, not the memory.
        let reloaded = Machine::load(
            Arc::clone(&machine.db),
            machine.cf,
            Arc::new(TestClock::new(0)) as Arc<dyn Clock>,
        )
        .unwrap();
        let applied = reloaded.applied().read().unwrap();
        assert_eq!(applied.allocated_end, 1_000);
        assert_eq!(applied.high_water_ms, 9_000);
    }

    /// A `Bootstrap` proposed by a leader that lost office can still commit beside its
    /// successor's. Whichever is second must find a cluster and refresh an address rather than
    /// mint a second cluster over the first.
    #[test]
    fn a_second_bootstrap_refreshes_the_address_and_mints_nothing() {
        let (_dir, machine) = machine(1_000);
        let first = Command::Bootstrap {
            store_id: 1,
            address: "127.0.0.1:20160".to_owned(),
            base_id: 1,
            cluster_id: 0xAAAA,
            now_ms: 1_000,
        };
        let Answer::Bootstrapped(done) = apply(&machine, &first) else {
            panic!("bootstrap answered something else");
        };
        assert_eq!(done.cluster_id, 0xAAAA);
        assert_eq!(done.region.unwrap().id, 1);

        let second = Command::Bootstrap {
            store_id: 1,
            address: "127.0.0.1:29999".to_owned(),
            base_id: 3,
            cluster_id: 0xBBBB,
            now_ms: 2_000,
        };
        let Answer::Bootstrapped(done) = apply(&machine, &second) else {
            panic!("bootstrap answered something else");
        };
        assert_eq!(done.cluster_id, 0xAAAA, "the cluster id is minted once");
        assert!(done.region.is_none(), "region 1 is created once");
        let store = crate::routing::read_store(&machine.db, 1).unwrap().unwrap();
        assert_eq!(store.address, "127.0.0.1:29999", "the address is refreshed");
    }

    /// The epoch guard is the one decision `apply` makes, and it makes it from the log's order
    /// rather than the network's.
    #[test]
    fn a_beat_older_than_the_record_it_would_replace_is_dropped() {
        let (_dir, machine) = machine(1_000);
        apply(&machine, &script(1_000)[1]);

        assert_eq!(
            apply(&machine, &beat(1, Epoch::new(1, 2), 5, 1_000)),
            Answer::Upserted(Upsert::Applied)
        );
        // Behind on the epoch.
        assert_eq!(
            apply(&machine, &beat(1, Epoch::new(1, 1), 9, 2_000)),
            Answer::Upserted(Upsert::Stale)
        );
        // Level on the epoch and behind on the term: a leader that already lost office.
        assert_eq!(
            apply(&machine, &beat(1, Epoch::new(1, 2), 4, 2_000)),
            Answer::Upserted(Upsert::Stale)
        );
        let held = crate::routing::read_region(&machine.db, 1)
            .unwrap()
            .unwrap();
        assert_eq!(held.term, 5, "a stale beat overwrote the record");
    }

    /// A snapshot is the state machine, so installing one has to leave a member holding exactly
    /// what the sender held — including *not* holding what the sender had removed.
    #[test]
    fn a_snapshot_replaces_a_state_rather_than_merging_into_it() {
        let (_source_dir, source) = machine(1_000);
        for command in script(1_000) {
            apply(&source, &command);
        }
        let data = source.snapshot().unwrap();

        let (_target_dir, target) = machine(7);
        // A region the sender has never heard of, which must be gone afterwards.
        apply(&target, &beat(42, Epoch::new(1, 1), 1, 7));
        assert!(
            crate::routing::read_region(&target.db, 42)
                .unwrap()
                .is_some()
        );

        let mut batch = WriteBatch::new();
        target.install(&data, &mut batch).unwrap();
        target.db.write(batch, &WriteOptions::synced()).unwrap();
        target.reload().unwrap();

        assert_eq!(target.snapshot().unwrap(), data);
        assert!(
            crate::routing::read_region(&target.db, 42)
                .unwrap()
                .is_none(),
            "installing merged instead of replacing"
        );
        let applied = target.applied().read().unwrap();
        assert_eq!(applied.cluster.unwrap().cluster_id, 0xABCD);
        assert_eq!(applied.allocated_end, 1_000);
    }

    /// Bytes that are not a snapshot are an error, never a half-installed state
    /// (`CLAUDE.md` invariants 2 and 9).
    #[test]
    fn a_corrupt_snapshot_is_an_error_and_never_a_panic() {
        let (_dir, machine) = machine(1_000);
        for bad in [&b""[..], &[9][..], &[1, 200][..]] {
            let mut batch = WriteBatch::new();
            assert!(machine.install(bad, &mut batch).is_err(), "{bad:?}");
        }
    }

    /// A beat for a store PD has no record of is refused by the leader before it is proposed, so
    /// the apply path must not invent one — a store record with no address is one a client
    /// cannot be routed to.
    #[test]
    fn a_beat_for_an_unregistered_store_writes_nothing() {
        let (_dir, machine) = machine(1_000);
        apply(
            &machine,
            &Command::StoreBeat {
                store_id: 77,
                stats: StoreStats::default(),
                now_ms: 1_000,
            },
        );
        assert!(
            crate::routing::read_store(&machine.db, 77)
                .unwrap()
                .is_none()
        );
        assert!(
            machine
                .db
                .get(cf::DEFAULT, &keys::store_key(77), &ReadOptions::default())
                .unwrap()
                .is_none()
        );
    }
}
