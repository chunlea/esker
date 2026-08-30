//! Balance, driven round after round against a model of a cluster that does as it is told.
//!
//! `prompts/04-multiraft-pd.md` 4d asks for regions and leaders to "spread out within a bounded
//! time". These tests are that, in the small and deterministically: a cluster is set up badly
//! out of balance, PD is fed heartbeats round after round, every operator it answers with is
//! applied to the model, and the run has to reach a spread of at most one — and then **stop**.
//!
//! Stopping is the harder half. A balancer that converges and then keeps moving things is worse
//! than one that never converges, because the churn is invisible in a snapshot of the counts.
//! So each test keeps going after it settles and asserts that PD asks for nothing at all —
//! [`QUIET_ROUNDS`] of them inline, and the full thousand the brief asks for behind
//! `--ignored`, because a hundred regions for a thousand rounds is a hundred thousand
//! heartbeats and a minute and a half of CI.
//!
//! A hundred rounds is not a weaker check than a thousand for this property: the per-region
//! cooldown is five rounds, so anything that oscillates does it twenty times over before the
//! inline run is done.
//!
//! # In memory, on purpose
//!
//! These tests drive tens of thousands of heartbeats and every one of them is a durable write.
//! On a real disk a thousand rounds over a hundred regions is eight minutes of `fsync` for a
//! property that has nothing to do with durability, so PD is opened on
//! `esker_engine::memfs`. What is *about* durability — `tests/crash_kill.rs` — uses a real
//! disk and a real signal.
//!
//! # The model is a store that always says yes
//!
//! Deliberately: PD's rules are what is under test, not a store's ability to apply a
//! configuration change. A store that applied operators unreliably would test the operator state
//! machine instead, which `esker-pd`'s unit tests already do against a clock a test sets by hand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Rounds of quiet a test insists on after the cluster settles.
const QUIET_ROUNDS: usize = 100;

/// Consecutive quiet rounds that count as settled. Longer than the balance cooldown in rounds
/// (300 s at one region-heartbeat interval each), so a cluster waiting out a cooldown is not
/// mistaken for a finished one.
const QUIET_RUN: usize = 10;

/// What `--ignored` runs instead: the thousand rounds `prompts/04-multiraft-pd.md` 4d asks for.
const SOAK_ROUNDS: usize = 1_000;

use std::collections::BTreeMap;
use std::sync::Arc;

use esker_engine::memfs::MemFileSystem;
use esker_pd::clock::TestClock;
use esker_pd::{Clock, Pd, PdOptions, RegionBeat, StoreBeat, StoreStats};
use esker_proto::{Epoch, Operator, Peer, Region};

/// One region, as the model holds it.
#[derive(Debug, Clone)]
struct Shard {
    peers: Vec<Peer>,
    leader_peer_id: u64,
    epoch: Epoch,
}

impl Shard {
    fn region(&self, id: u64) -> Region {
        Region {
            id,
            start_key: bytes::Bytes::new(),
            end_key: bytes::Bytes::new(),
            peers: self.peers.clone(),
            epoch: self.epoch,
        }
    }
}

/// A cluster that applies whatever PD asks for, immediately and correctly.
#[derive(Debug)]
struct Model {
    shards: BTreeMap<u64, Shard>,
    stores: Vec<u64>,
}

impl Model {
    /// `layout[i]` regions whose single replica is on store `i + 1`.
    fn single_replica(layout: &[usize]) -> Self {
        let mut shards = BTreeMap::new();
        let mut next = 1;
        for (index, count) in layout.iter().enumerate() {
            let store_id = u64::try_from(index).unwrap() + 1;
            for _ in 0..*count {
                shards.insert(
                    next,
                    Shard {
                        // Peer ids well clear of the ones PD's allocator will mint.
                        peers: vec![Peer::voter(store_id, next * 100 + store_id)],
                        // A region's only replica leads it — which is what a real cluster looks
                        // like, and the case that caught the rule refusing to move a leader's
                        // replica at all.
                        leader_peer_id: next * 100 + store_id,
                        epoch: Epoch::INITIAL,
                    },
                );
                next += 1;
            }
        }
        Self {
            shards,
            stores: (1..=u64::try_from(layout.len()).unwrap()).collect(),
        }
    }

    /// Every region replicated on every store, with `layout[i]` of them led from store `i + 1`.
    fn three_replicas(layout: &[usize]) -> Self {
        let stores: Vec<u64> = (1..=u64::try_from(layout.len()).unwrap()).collect();
        let mut shards = BTreeMap::new();
        let mut next = 1;
        for (index, count) in layout.iter().enumerate() {
            let leader_store = u64::try_from(index).unwrap() + 1;
            for _ in 0..*count {
                let peers: Vec<Peer> = stores
                    .iter()
                    .map(|store_id| Peer::voter(*store_id, next * 100 + store_id))
                    .collect();
                shards.insert(
                    next,
                    Shard {
                        leader_peer_id: next * 100 + leader_store,
                        peers,
                        epoch: Epoch::INITIAL,
                    },
                );
                next += 1;
            }
        }
        Self { shards, stores }
    }

    fn regions_on(&self, store_id: u64) -> u64 {
        self.shards
            .values()
            .filter(|shard| shard.peers.iter().any(|peer| peer.store_id == store_id))
            .count() as u64
    }

    fn leaders_on(&self, store_id: u64) -> u64 {
        self.shards
            .values()
            .filter(|shard| {
                shard
                    .peers
                    .iter()
                    .any(|peer| peer.peer_id == shard.leader_peer_id && peer.store_id == store_id)
            })
            .count() as u64
    }

    fn region_counts(&self) -> Vec<u64> {
        self.stores.iter().map(|id| self.regions_on(*id)).collect()
    }

    fn leader_counts(&self) -> Vec<u64> {
        self.stores.iter().map(|id| self.leaders_on(*id)).collect()
    }

    /// Applies an operator the way a store that never fails would.
    fn apply(&mut self, operator: &Operator) {
        match operator {
            Operator::AddPeer {
                region_id,
                store_id,
                peer_id,
                ..
            } => {
                let shard = self.shards.get_mut(region_id).expect("a region PD named");
                shard.peers.push(Peer::voter(*store_id, *peer_id));
                shard.epoch.conf_ver += 1;
            }
            Operator::RemovePeer {
                region_id, peer_id, ..
            } => {
                let shard = self.shards.get_mut(region_id).expect("a region PD named");
                shard.peers.retain(|peer| peer.peer_id != *peer_id);
                shard.epoch.conf_ver += 1;
                if shard.leader_peer_id == *peer_id {
                    // Raft would elect somebody; the lowest id is as good as any and keeps the
                    // model deterministic.
                    shard.leader_peer_id = shard
                        .peers
                        .iter()
                        .map(|peer| peer.peer_id)
                        .min()
                        .unwrap_or(0);
                }
            }
            Operator::TransferLeader {
                region_id,
                to_peer_id,
                ..
            } => {
                // Leadership moves without a configuration change, so the epoch does not move.
                self.shards
                    .get_mut(region_id)
                    .expect("a region PD named")
                    .leader_peer_id = *to_peer_id;
            }
        }
    }
}

/// PD, a clock, and a model, wired together.
struct Harness {
    _dir: tempfile::TempDir,
    clock: Arc<TestClock>,
    pd: Arc<Pd>,
    model: Model,
    operators: usize,
}

impl Harness {
    fn start(model: Model, target_replicas: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(TestClock::new(1_700_000_000_000));
        let pd = Pd::open(
            dir.path(),
            PdOptions {
                target_replicas,
                filesystem: Some(Arc::new(MemFileSystem::new())),
                ..PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
            },
        )
        .unwrap();
        for store_id in &model.stores {
            pd.bootstrap(*store_id, &format!("127.0.0.1:{store_id}"))
                .unwrap();
        }
        Self {
            _dir: dir,
            clock,
            pd,
            model,
            operators: 0,
        }
    }

    /// One round: every store reports, then every region's leader does, and every operator PD
    /// answers with is applied. Returns how many operators this round produced.
    fn round(&mut self) -> usize {
        // A round is a region-heartbeat interval (`docs/DESIGN.md` §14), so a region's balance
        // cooldown expires after a few of them rather than never.
        self.clock.advance(60_000);
        for store_id in &self.model.stores {
            self.pd
                .store_heartbeat(&StoreBeat {
                    store_id: *store_id,
                    stats: StoreStats {
                        region_count: self.model.regions_on(*store_id),
                        leader_count: self.model.leaders_on(*store_id),
                        capacity: 1 << 40,
                        available: 1 << 39,
                        applied_bytes: 0,
                    },
                })
                .unwrap();
        }

        let ids: Vec<u64> = self.model.shards.keys().copied().collect();
        let mut issued = 0;
        for id in ids {
            let shard = self.model.shards[&id].clone();
            let beat = self
                .pd
                .region_heartbeat(&RegionBeat {
                    region: shard.region(id),
                    leader_peer_id: shard.leader_peer_id,
                    term: 4,
                    approximate_size: 0,
                    applied_index: 0,
                })
                .unwrap();
            if let Some(operator) = beat.operator {
                if std::env::var("ESKER_TRACE_BALANCE").is_ok() {
                    println!("  {operator:?}");
                }
                self.model.apply(&operator);
                issued += 1;
                self.operators += 1;
            }
        }
        issued
    }

    /// Rounds until PD has asked for nothing for [`QUIET_RUN`] rounds running.
    ///
    /// "Settled" has to mean *PD has nothing left to ask*, not "the counts look even". A move
    /// is two or three operators, and while one is half done the region sits on both stores —
    /// so a snapshot of the counts can look balanced with dozens of moves outstanding. The
    /// quiet run has to be longer than the balance cooldown, or a cluster merely waiting out
    /// its cooldown would be mistaken for a finished one.
    fn settle(&mut self, limit: usize) -> usize {
        let mut quiet = 0;
        for round in 1..=limit {
            if self.round() == 0 {
                quiet += 1;
                if quiet >= QUIET_RUN {
                    return round;
                }
            } else {
                quiet = 0;
            }
        }
        panic!(
            "not settled after {limit} rounds: regions {:?}, leaders {:?}",
            self.model.region_counts(),
            self.model.leader_counts()
        );
    }

    /// Replicas across the cluster, which must equal the number of regions once every move has
    /// finished. A half-done move shows up here and nowhere else.
    fn replicas(&self) -> u64 {
        self.model.region_counts().iter().sum()
    }
}

fn region_spread(model: &Model) -> u64 {
    let counts = model.region_counts();
    counts.iter().max().copied().unwrap_or(0) - counts.iter().min().copied().unwrap_or(0)
}

fn leader_spread(model: &Model) -> u64 {
    let counts = model.leader_counts();
    counts.iter().max().copied().unwrap_or(0) - counts.iter().min().copied().unwrap_or(0)
}

/// The brief's case: a hundred regions split 60/30/10 across three stores, converging to ±1.
#[test]
fn a_lopsided_cluster_spreads_its_regions_and_then_stops() {
    regions_spread_and_stop(QUIET_ROUNDS);
}

/// The soak: the full thousand rounds of quiet.
#[test]
#[ignore = "a hundred thousand heartbeats; run it deliberately"]
fn a_lopsided_cluster_stays_settled_for_a_thousand_rounds() {
    regions_spread_and_stop(SOAK_ROUNDS);
}

fn regions_spread_and_stop(quiet: usize) {
    let mut harness = Harness::start(Model::single_replica(&[60, 30, 10]), 1);
    assert_eq!(harness.model.region_counts(), vec![60, 30, 10]);

    let rounds = harness.settle(200);
    let counts = harness.model.region_counts();
    assert!(region_spread(&harness.model) <= 1, "settled at {counts:?}");
    assert_eq!(
        harness.replicas(),
        100,
        "settled with moves half done: {counts:?}"
    );
    println!(
        "regions balanced to {counts:?} in {rounds} rounds, {} operators",
        harness.operators
    );

    // And then it stops. A balancer that converges and keeps moving is worse than one that
    // never converges, because the churn does not show up in a snapshot of the counts.
    let settled = harness.operators;
    for round in 0..quiet {
        assert_eq!(
            harness.round(),
            0,
            "round {round} after convergence asked for an operator; counts {:?}",
            harness.model.region_counts()
        );
    }
    assert_eq!(harness.operators, settled);
    assert_eq!(harness.model.region_counts(), counts, "the counts moved");
}

/// The same, for leadership: every region on every store, and the office spread 60/30/10.
#[test]
fn lopsided_leadership_spreads_and_then_stops() {
    leaders_spread_and_stop(QUIET_ROUNDS);
}

/// The soak, for leadership.
#[test]
#[ignore = "a hundred thousand heartbeats; run it deliberately"]
fn lopsided_leadership_stays_settled_for_a_thousand_rounds() {
    leaders_spread_and_stop(SOAK_ROUNDS);
}

fn leaders_spread_and_stop(quiet: usize) {
    let mut harness = Harness::start(Model::three_replicas(&[60, 30, 10]), 3);
    assert_eq!(harness.model.leader_counts(), vec![60, 30, 10]);

    let rounds = harness.settle(200);
    let counts = harness.model.leader_counts();
    assert!(leader_spread(&harness.model) <= 1, "settled at {counts:?}");
    assert_eq!(counts.iter().sum::<u64>(), 100, "a leader was lost");
    assert_eq!(
        harness.model.region_counts(),
        vec![100, 100, 100],
        "leader balance moved a replica"
    );
    println!(
        "leaders balanced to {counts:?} in {rounds} rounds, {} operators",
        harness.operators
    );

    for round in 0..quiet {
        assert_eq!(
            harness.round(),
            0,
            "round {round} after convergence asked for an operator; leaders {:?}",
            harness.model.leader_counts()
        );
    }
    assert_eq!(harness.model.leader_counts(), counts);
}

/// The in-flight cap bounds how many moves are started at once — and a move already begun is
/// never blocked by it, because a region stranded on two stores is exactly what the cap exists
/// to avoid.
#[test]
fn no_more_moves_are_started_than_the_cap_allows() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let cap = 2;
    let pd = Pd::open(
        dir.path(),
        PdOptions {
            target_replicas: 1,
            max_balance_operators: cap,
            filesystem: Some(Arc::new(MemFileSystem::new())),
            ..PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
        },
    )
    .unwrap();
    let model = Model::single_replica(&[60, 30, 10]);
    for store_id in &model.stores {
        pd.bootstrap(*store_id, &format!("127.0.0.1:{store_id}"))
            .unwrap();
    }
    let mut harness = Harness {
        _dir: dir,
        clock,
        pd,
        model,
        operators: 0,
    };

    // One round of heartbeats with nothing applied: every region asks, and only the cap's
    // worth of moves may start.
    harness.clock.advance(60_000);
    for store_id in &harness.model.stores {
        harness
            .pd
            .store_heartbeat(&StoreBeat {
                store_id: *store_id,
                stats: StoreStats {
                    region_count: harness.model.regions_on(*store_id),
                    leader_count: harness.model.leaders_on(*store_id),
                    ..StoreStats::default()
                },
            })
            .unwrap();
    }
    let mut started = 0;
    for id in harness.model.shards.keys().copied().collect::<Vec<_>>() {
        let shard = harness.model.shards[&id].clone();
        let beat = harness
            .pd
            .region_heartbeat(&RegionBeat {
                region: shard.region(id),
                leader_peer_id: shard.leader_peer_id,
                term: 4,
                approximate_size: 0,
                applied_index: 0,
            })
            .unwrap();
        if beat.operator.is_some() {
            started += 1;
        }
    }
    assert_eq!(started, cap, "the cap did not bound the moves started");

    // And it still converges, just more slowly — the cap delays moves, it does not forbid them.
    harness.settle(400);
    assert!(region_spread(&harness.model) <= 1);
    assert_eq!(harness.replicas(), 100);
}

/// A cluster that is already balanced is not touched at all — the property every "and then
/// stops" assertion above rests on, checked from the other direction.
#[test]
fn a_balanced_cluster_is_never_touched() {
    let mut harness = Harness::start(Model::single_replica(&[34, 33, 33]), 1);
    for round in 0..200 {
        assert_eq!(harness.round(), 0, "round {round} moved something");
    }
    assert_eq!(harness.model.region_counts(), vec![34, 33, 33]);
}

/// Balance can be switched off, and repair still runs. An operator wanting a cluster left
/// exactly as it is should not have to choose between that and losing replica repair.
#[test]
fn balance_can_be_turned_off_without_turning_off_repair() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(TestClock::new(1_700_000_000_000));
    let pd = Pd::open(
        dir.path(),
        PdOptions {
            balance: false,
            target_replicas: 1,
            filesystem: Some(Arc::new(MemFileSystem::new())),
            ..PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn Clock>)
        },
    )
    .unwrap();
    let model = Model::single_replica(&[60, 30, 10]);
    for store_id in &model.stores {
        pd.bootstrap(*store_id, &format!("127.0.0.1:{store_id}"))
            .unwrap();
    }

    // Wildly unbalanced, and PD asks for nothing.
    for _ in 0..5 {
        clock.advance(60_000);
        for store_id in &model.stores {
            pd.store_heartbeat(&StoreBeat {
                store_id: *store_id,
                stats: StoreStats {
                    region_count: model.regions_on(*store_id),
                    leader_count: model.leaders_on(*store_id),
                    ..StoreStats::default()
                },
            })
            .unwrap();
        }
        for id in model.shards.keys().copied().collect::<Vec<_>>() {
            let shard = model.shards[&id].clone();
            let beat = pd
                .region_heartbeat(&RegionBeat {
                    region: shard.region(id),
                    leader_peer_id: shard.leader_peer_id,
                    term: 4,
                    approximate_size: 0,
                    applied_index: 0,
                })
                .unwrap();
            assert_eq!(beat.operator, None, "balance is off");
        }
    }

    // Now store 3 dies, and repair still happens.
    clock.advance(esker_pd::pd::MAX_STORE_DOWN_TIME_MS + 1);
    for store_id in [1, 2] {
        pd.store_heartbeat(&StoreBeat {
            store_id,
            stats: StoreStats::default(),
        })
        .unwrap();
    }
    let doomed = *model
        .shards
        .iter()
        .find(|(_, shard)| shard.peers[0].store_id == 3)
        .expect("a region on store 3")
        .0;
    let shard = model.shards[&doomed].clone();
    let beat = pd
        .region_heartbeat(&RegionBeat {
            region: shard.region(doomed),
            leader_peer_id: shard.leader_peer_id,
            term: 4,
            approximate_size: 0,
            applied_index: 0,
        })
        .unwrap();
    assert!(
        matches!(beat.operator, Some(Operator::AddPeer { .. })),
        "repair runs with balance off, got {:?}",
        beat.operator
    );
}
