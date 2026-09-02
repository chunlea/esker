//! Balance never touches a region that is mid-repair.
//!
//! The mechanism is `548dd62` / `docs/DESIGN.md` §7. Repair is urgent and balance is an
//! optimisation, so balance is subordinate to it everywhere — but "mid-repair" is a bigger set of
//! states than the one repair *starts* from, and that is what the fix was about. A region holding
//! a plain `Learner` is a move in progress whoever started it: the promotion that finishes it is
//! proposed by the **leader**, on the learner's `matched`, because a region heartbeat comes only
//! from a leader and the placement driver can therefore never see a learner's progress. A leader
//! with a leadership transfer in flight refuses proposals. So a `TransferLeader` against a region
//! whose learner has not been promoted blocks the very `AddVoter` that would finish the repair,
//! and the two then wait for each other until the operator times out.
//!
//! # What this model explores that the fix's test does not
//!
//! `esker-store`'s `promotion.rs` caught this as an intermittent failure and reproduced it by
//! turning on debug logging, which widened the window enough to make the race deterministic. That
//! is one ordering. This model runs a five-store cluster through a seeded schedule of stores
//! going down and coming back, repairs started and finished, columnar learners appearing, and
//! balance moves landing — and asks the policy about every region after every step.
//!
//! The state it is really hunting is the one the narrow definition could not see: **a learner on
//! a live store**. Reached here by adding a learner while a store is down and then bringing that
//! store back before the promotion runs, and also by a balance move of the policy's own that
//! landed and has not been promoted yet. In both, no store is down, so a rule that counts down
//! stores and nothing else says the region is settled.
//!
//! # The ground truth is the model's, not the policy's
//!
//! [`World`] remembers that it added a learner and that it took a store down. The checker reads
//! that memory. It never asks the policy whether a region is mid-repair, because a policy that is
//! wrong about it would then be checked against its own mistake.

use esker_base::rng::Pcg32;

/// What a peer is for.
///
/// Mirrors `esker_proto::PeerRole` without depending on it: this crate sits below the layers it
/// models, and a binding converts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Votes and counts toward a quorum.
    Voter,
    /// A replica catching up, on its way to being a voter. **A region holding one is mid-repair.**
    Learner,
    /// A columnar replica, which [ADR 0022] says is never promoted. Not a repair: counting it
    /// would freeze every region holding one out of balance for ever.
    ///
    /// [ADR 0022]: ../../../../docs/adr/0022-the-columnar-replica.md
    ColumnarLearner,
}

/// One replica of a region, as the placement driver knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerView {
    /// Cluster-unique peer id.
    pub peer_id: u64,
    /// The store it lives on.
    pub store_id: u64,
    /// What it is for.
    pub role: Role,
}

/// One region, as the placement driver knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionView {
    /// Cluster-unique region id.
    pub region_id: u64,
    /// The membership half of the epoch, bumped by every conf change.
    pub conf_ver: u64,
    /// The range half of the epoch, bumped by every split.
    pub version: u64,
    /// The peer the placement driver believes leads it, or zero for "no opinion".
    pub leader_peer_id: u64,
    /// Its replicas.
    pub peers: Vec<PeerView>,
}

/// One store, as the placement driver knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreView {
    /// Cluster-unique store id.
    pub store_id: u64,
    /// When its last heartbeat arrived, on the model's clock. Liveness is derived from this and
    /// nothing else, exactly as `esker_pd::schedule::is_down` does it.
    pub last_heartbeat_ms: u64,
    /// Regions with a peer on this store.
    pub region_count: u64,
    /// Regions this store leads.
    pub leader_count: u64,
}

/// Everything a balance decision is taken against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterView {
    /// Every store the placement driver knows about.
    pub stores: Vec<StoreView>,
    /// The placement driver's clock, now.
    pub now_ms: u64,
    /// Silence after which a store is down.
    pub max_store_down_time_ms: u64,
    /// Voters a region should have.
    pub target_replicas: usize,
}

/// A move balance wants, stripped to what the checker needs to name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Hand a region's leadership to a peer on a quieter store.
    TransferLeader {
        /// The region whose office moves.
        region_id: u64,
        /// The peer that should take office.
        to_peer_id: u64,
    },
    /// Put a new replica on a quieter store — the first half of a move.
    AddPeer {
        /// The region gaining a replica.
        region_id: u64,
        /// Where it goes.
        store_id: u64,
    },
    /// Drop the replica on the busiest store — the second half.
    RemovePeer {
        /// The region losing a replica.
        region_id: u64,
        /// Which replica.
        peer_id: u64,
    },
}

/// The decision under test: what balance wants for one region, if anything.
///
/// `crates/esker-pd/tests/sim_balance.rs` implements this by calling the real
/// `esker_pd::balance::balance_for`. That binding is the only thing here that proves anything
/// about the placement driver.
pub trait BalancePolicy {
    /// What this policy would do about `region`, given `cluster`.
    fn plan(&self, region: &RegionView, cluster: &ClusterView) -> Option<Move>;
}

/// Why the model says a region is mid-repair. Model state, never derived from the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidRepair {
    /// A peer of this region sits on a store the model has taken down.
    PeerOnDownStore {
        /// The store that is down.
        store_id: u64,
    },
    /// The model added a plain learner to this region and has not promoted it.
    ///
    /// The state the narrow definition could not see, and the reason this model exists.
    UnpromotedLearner {
        /// The learner still waiting for its promotion.
        peer_id: u64,
        /// Whether every store this region sits on is up. When it is, a rule that counts down
        /// stores and nothing else calls the region settled.
        all_stores_live: bool,
    },
}

/// What went wrong, with the seed to reproduce it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// Balance planned a move against a region the model knows is mid-repair.
    BalancedMidRepair {
        /// The run's seed.
        seed: u64,
        /// The round it happened in.
        round: u64,
        /// The region.
        region_id: u64,
        /// Why the model says it was mid-repair.
        why: MidRepair,
        /// What the policy wanted to do to it anyway.
        planned: Move,
    },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self::BalancedMidRepair {
            seed,
            round,
            region_id,
            why,
            planned,
        } = self;
        write!(
            formatter,
            "seed {seed}, round {round}: balance planned {planned:?} against region {region_id}, \
             which is mid-repair ({why:?}). DESIGN.md 7: balance never touches a region that is \
             mid-repair, and a plain learner is one of the two states that means"
        )
    }
}

impl std::error::Error for Violation {}

/// What a clean run did, so a test can say the model reached the states it claims to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Report {
    /// Rounds run.
    pub rounds: u64,
    /// Moves the policy planned and the model applied.
    pub moves_applied: u64,
    /// Times the model asked the policy about a mid-repair region and was told `None`.
    pub declined_mid_repair: u64,
    /// Of those, the ones where **every** store the region sits on was up — the state a rule
    /// that counts down stores cannot see. A run that never reaches this proves nothing.
    pub declined_learner_all_live: u64,
    /// Learners the model added.
    pub learners_added: u64,
    /// Learners the model promoted.
    pub learners_promoted: u64,
    /// Stores the model took down.
    pub stores_downed: u64,
}

/// How long a store must be silent before the model's cluster calls it down.
const MAX_STORE_DOWN_MS: u64 = 30_000;

/// One round of the model's clock.
const ROUND_MS: u64 = 5_000;

/// Voters a region should have.
const TARGET_REPLICAS: usize = 3;

/// The model's cluster, and the memory the checker reads.
#[derive(Debug)]
pub struct World {
    stores: Vec<Store>,
    regions: Vec<Region>,
    now_ms: u64,
    next_peer_id: u64,
    rng: Pcg32,
    report: Report,
}

#[derive(Debug)]
struct Store {
    id: u64,
    last_heartbeat_ms: u64,
    /// Model state. `last_heartbeat_ms` is what the policy sees; this is what the checker reads.
    down: bool,
}

#[derive(Debug)]
struct Region {
    view: RegionView,
    /// The learner the model has added and not promoted, if any.
    ///
    /// Set whenever a plain learner joins — repair's or balance's, because the invariant does not
    /// distinguish them: "a replica that has been added and not yet promoted ... is a move in
    /// progress whoever started it".
    unpromoted: Option<u64>,
}

impl World {
    /// A five-store cluster with eight regions, placed deliberately lopsidedly so that balance
    /// has something it wants to do from the first round.
    ///
    /// A balanced cluster tests nothing here: every rule in `esker_pd::balance` is gated on a
    /// spread of at least two, so a cluster that is already even answers `None` for reasons that
    /// have nothing to do with repair.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        // Store 1 holds a peer of every region, store 5 holds one. Twenty-four peers over five
        // stores, with a spread far wider than `REGION_SPREAD_THRESHOLD`.
        const PLACEMENT: [[u64; 3]; 8] = [
            [1, 2, 3],
            [1, 2, 3],
            [1, 2, 4],
            [1, 2, 4],
            [1, 3, 4],
            [1, 3, 4],
            [1, 2, 3],
            [1, 4, 5],
        ];

        let mut world = Self {
            stores: (1..=5)
                .map(|id| Store {
                    id,
                    last_heartbeat_ms: 0,
                    down: false,
                })
                .collect(),
            regions: Vec::new(),
            now_ms: 0,
            next_peer_id: 1,
            rng: Pcg32::new(seed, 0xba1a),
            report: Report::default(),
        };

        for (index, stores) in PLACEMENT.iter().enumerate() {
            let peers: Vec<PeerView> = stores
                .iter()
                .map(|&store_id| {
                    let peer_id = world.next_peer_id;
                    world.next_peer_id += 1;
                    PeerView {
                        peer_id,
                        store_id,
                        role: Role::Voter,
                    }
                })
                .collect();
            world.regions.push(Region {
                view: RegionView {
                    region_id: index as u64 + 1,
                    conf_ver: 1,
                    version: 1,
                    // The placement driver has an opinion about every leader, or `leader_balance`
                    // returns `None` before it reaches anything this model is about.
                    leader_peer_id: peers[0].peer_id,
                    peers,
                },
                unpromoted: None,
            });
        }
        world
    }

    /// The cluster as a policy sees it.
    #[must_use]
    pub fn cluster(&self) -> ClusterView {
        ClusterView {
            stores: self
                .stores
                .iter()
                .map(|store| StoreView {
                    store_id: store.id,
                    last_heartbeat_ms: store.last_heartbeat_ms,
                    region_count: 0,
                    leader_count: 0,
                })
                .collect(),
            now_ms: self.now_ms,
            max_store_down_time_ms: MAX_STORE_DOWN_MS,
            target_replicas: TARGET_REPLICAS,
        }
        .with_counts(&self.regions)
    }

    /// Every region, as a policy sees them.
    #[must_use]
    pub fn regions(&self) -> Vec<RegionView> {
        self.regions
            .iter()
            .map(|region| region.view.clone())
            .collect()
    }

    /// Why the model says `region_id` is mid-repair, if it is.
    ///
    /// **Read from the model's own memory.** `unpromoted` is set by the step that added the
    /// learner and cleared by the step that promoted it; `down` is set by the step that took the
    /// store down. Neither is re-derived from the peer list, so a policy that reads the peer list
    /// wrongly cannot make this agree with it.
    #[must_use]
    pub fn mid_repair(&self, region_id: u64) -> Option<MidRepair> {
        let region = self
            .regions
            .iter()
            .find(|r| r.view.region_id == region_id)?;
        if let Some(peer_id) = region.unpromoted {
            return Some(MidRepair::UnpromotedLearner {
                peer_id,
                all_stores_live: !region
                    .view
                    .peers
                    .iter()
                    .any(|peer| self.is_down(peer.store_id)),
            });
        }
        region
            .view
            .peers
            .iter()
            .find(|peer| self.is_down(peer.store_id))
            .map(|peer| MidRepair::PeerOnDownStore {
                store_id: peer.store_id,
            })
    }

    fn is_down(&self, store_id: u64) -> bool {
        self.stores
            .iter()
            .find(|store| store.id == store_id)
            .is_some_and(|store| store.down)
    }
}

impl ClusterView {
    fn with_counts(mut self, regions: &[Region]) -> Self {
        for store in &mut self.stores {
            store.region_count = 0;
            store.leader_count = 0;
        }
        for region in regions {
            for peer in &region.view.peers {
                if let Some(store) = self
                    .stores
                    .iter_mut()
                    .find(|store| store.store_id == peer.store_id)
                {
                    store.region_count += 1;
                    if peer.peer_id == region.view.leader_peer_id {
                        store.leader_count += 1;
                    }
                }
            }
        }
        self
    }
}

/// One perturbation of the world, drawn from the run's seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Time passes; every live store beats.
    Beat,
    /// A store stops beating.
    StoreDown(u64),
    /// A store beats again. **The step that reaches the interesting state**: a region repaired
    /// while this store was down still holds an unpromoted learner, and now no store is down.
    StoreUp(u64),
    /// Repair puts a replacement beside a peer on a down store, as a learner.
    AddLearner {
        /// Which region.
        region_id: u64,
        /// Where the replacement goes.
        store_id: u64,
    },
    /// The leader promotes a caught-up learner.
    Promote(u64),
    /// A columnar replica appears. Never a repair (ADR 0022).
    AddColumnarLearner {
        /// Which region.
        region_id: u64,
        /// Where it goes.
        store_id: u64,
    },
}

/// Runs `rounds` rounds against `policy`, checking after every step.
///
/// # Errors
///
/// [`Violation::BalancedMidRepair`] the first time the policy plans a move against a region the
/// model knows is mid-repair.
pub fn run<P: BalancePolicy>(seed: u64, rounds: u64, policy: &P) -> Result<Report, Violation> {
    let mut world = World::new(seed);
    for round in 0..rounds {
        let step = world.draw();
        world.apply(step);
        world.check(seed, round, policy)?;
        world.settle(policy);
        world.check(seed, round, policy)?;
    }
    world.report.rounds = rounds;
    Ok(world.report)
}

impl World {
    /// The next perturbation. Weighted so that repairs are started often and finished late,
    /// because the window between the two is the whole subject.
    fn draw(&mut self) -> Step {
        let live: Vec<u64> = self
            .stores
            .iter()
            .filter(|store| !store.down)
            .map(|store| store.id)
            .collect();
        let down: Vec<u64> = self
            .stores
            .iter()
            .filter(|store| store.down)
            .map(|store| store.id)
            .collect();
        let pending: Vec<u64> = self
            .regions
            .iter()
            .filter(|region| region.unpromoted.is_some())
            .map(|region| region.view.region_id)
            .collect();

        match self.rng.below(100) {
            // A store comes back while a repair it caused is still open. The state the narrow
            // definition cannot see, so it is drawn first and often.
            0..=24 if !down.is_empty() => Step::StoreUp(down[self.pick(down.len())]),
            // Two down at once would take a three-voter region below quorum, which is a
            // different subject.
            25..=39 if down.len() < 2 && live.len() > 3 => {
                Step::StoreDown(live[self.pick(live.len())])
            }
            40..=64 => {
                let count = self.regions.len();
                let index = self.pick(count);
                let region_id = self.regions[index].view.region_id;
                let store_id = live[self.pick(live.len())];
                Step::AddLearner {
                    region_id,
                    store_id,
                }
            }
            65..=79 if !pending.is_empty() => Step::Promote(pending[self.pick(pending.len())]),
            80..=87 => {
                let count = self.regions.len();
                let index = self.pick(count);
                let region_id = self.regions[index].view.region_id;
                let store_id = live[self.pick(live.len())];
                Step::AddColumnarLearner {
                    region_id,
                    store_id,
                }
            }
            _ => Step::Beat,
        }
    }

    fn pick(&mut self, len: usize) -> usize {
        if len == 0 {
            0
        } else {
            self.rng.below(u32::try_from(len).unwrap_or(u32::MAX)) as usize
        }
    }

    fn apply(&mut self, step: Step) {
        match step {
            Step::Beat => {
                self.now_ms += ROUND_MS;
                for store in &mut self.stores {
                    if !store.down {
                        store.last_heartbeat_ms = self.now_ms;
                    }
                }
            }
            Step::StoreDown(store_id) => {
                if let Some(store) = self.store_mut(store_id) {
                    store.down = true;
                }
                // Silence long enough that `is_down` agrees with the model.
                self.now_ms += MAX_STORE_DOWN_MS + ROUND_MS;
                for store in &mut self.stores {
                    if !store.down {
                        store.last_heartbeat_ms = self.now_ms;
                    }
                }
                self.report.stores_downed += 1;
            }
            Step::StoreUp(store_id) => {
                let now = self.now_ms;
                if let Some(store) = self.store_mut(store_id) {
                    store.down = false;
                    store.last_heartbeat_ms = now;
                }
            }
            Step::AddLearner {
                region_id,
                store_id,
            } => self.add_learner(region_id, store_id, Role::Learner),
            Step::AddColumnarLearner {
                region_id,
                store_id,
            } => self.add_learner(region_id, store_id, Role::ColumnarLearner),
            Step::Promote(region_id) => {
                let Some(region) = self.region_mut(region_id) else {
                    return;
                };
                let Some(peer_id) = region.unpromoted.take() else {
                    return;
                };
                if let Some(peer) = region
                    .view
                    .peers
                    .iter_mut()
                    .find(|peer| peer.peer_id == peer_id)
                {
                    peer.role = Role::Voter;
                }
                region.view.conf_ver += 1;
                self.report.learners_promoted += 1;
                self.shed_dead_voter(region_id);
            }
        }
    }

    fn add_learner(&mut self, region_id: u64, store_id: u64, role: Role) {
        let peer_id = self.next_peer_id;
        let Some(region) = self.region_mut(region_id) else {
            return;
        };
        if region
            .view
            .peers
            .iter()
            .any(|peer| peer.store_id == store_id)
        {
            // A store holds at most one peer of a region.
            return;
        }
        if role == Role::Learner && region.unpromoted.is_some() {
            // One repair at a time per region, which is what `at most one operator in flight`
            // already gives (`docs/DESIGN.md` §7).
            return;
        }
        region.view.peers.push(PeerView {
            peer_id,
            store_id,
            role,
        });
        region.view.conf_ver += 1;
        if role == Role::Learner {
            region.unpromoted = Some(peer_id);
        }
        self.next_peer_id += 1;
        if role == Role::Learner {
            self.report.learners_added += 1;
        }
    }

    /// Once a promotion has landed, repair sheds the peer on the down store — the second half of
    /// the membership change, and what returns the region to its settled shape.
    fn shed_dead_voter(&mut self, region_id: u64) {
        let dead: Vec<u64> = self
            .regions
            .iter()
            .find(|region| region.view.region_id == region_id)
            .map(|region| {
                region
                    .view
                    .peers
                    .iter()
                    .filter(|peer| peer.role == Role::Voter && self.is_down(peer.store_id))
                    .map(|peer| peer.peer_id)
                    .collect()
            })
            .unwrap_or_default();
        let voters = self
            .regions
            .iter()
            .find(|region| region.view.region_id == region_id)
            .map_or(0, |region| {
                region
                    .view
                    .peers
                    .iter()
                    .filter(|peer| peer.role == Role::Voter)
                    .count()
            });
        if voters <= TARGET_REPLICAS {
            return;
        }
        let Some(&peer_id) = dead.first() else {
            return;
        };
        self.remove_peer(region_id, peer_id);
    }

    fn remove_peer(&mut self, region_id: u64, peer_id: u64) {
        let Some(region) = self.region_mut(region_id) else {
            return;
        };
        region.view.peers.retain(|peer| peer.peer_id != peer_id);
        region.view.conf_ver += 1;
        if region.view.leader_peer_id == peer_id {
            region.view.leader_peer_id = region.view.peers.first().map_or(0, |peer| peer.peer_id);
        }
        if region.unpromoted == Some(peer_id) {
            region.unpromoted = None;
        }
    }

    fn store_mut(&mut self, store_id: u64) -> Option<&mut Store> {
        self.stores.iter_mut().find(|store| store.id == store_id)
    }

    fn region_mut(&mut self, region_id: u64) -> Option<&mut Region> {
        self.regions
            .iter_mut()
            .find(|region| region.view.region_id == region_id)
    }

    /// Asks the policy about every region and checks the answer against the model's own memory.
    fn check<P: BalancePolicy>(
        &mut self,
        seed: u64,
        round: u64,
        policy: &P,
    ) -> Result<(), Violation> {
        let cluster = self.cluster();
        for index in 0..self.regions.len() {
            let view = self.regions[index].view.clone();
            let planned = policy.plan(&view, &cluster);
            let Some(why) = self.mid_repair(view.region_id) else {
                continue;
            };
            if let Some(planned) = planned {
                return Err(Violation::BalancedMidRepair {
                    seed,
                    round,
                    region_id: view.region_id,
                    why,
                    planned,
                });
            }
            self.report.declined_mid_repair += 1;
            if matches!(
                why,
                MidRepair::UnpromotedLearner {
                    all_stores_live: true,
                    ..
                }
            ) {
                self.report.declined_learner_all_live += 1;
            }
        }
        Ok(())
    }

    /// Applies one move the policy wants for a region the model agrees is settled.
    ///
    /// Without this the cluster never converges and the same imbalance is re-planned for ever,
    /// which is a narrower run than it looks: the states worth reaching are the ones a *landed*
    /// balance move produces — an added learner nobody has promoted, on a live store, with the
    /// counts already changed.
    fn settle<P: BalancePolicy>(&mut self, policy: &P) {
        let cluster = self.cluster();
        let settled: Vec<RegionView> = self
            .regions
            .iter()
            .filter(|region| self.mid_repair(region.view.region_id).is_none())
            .map(|region| region.view.clone())
            .collect();
        for view in settled {
            let Some(planned) = policy.plan(&view, &cluster) else {
                continue;
            };
            match planned {
                Move::AddPeer {
                    region_id,
                    store_id,
                } => self.add_learner(region_id, store_id, Role::Learner),
                Move::RemovePeer { region_id, peer_id } => self.remove_peer(region_id, peer_id),
                Move::TransferLeader {
                    region_id,
                    to_peer_id,
                } => {
                    if let Some(region) = self.region_mut(region_id) {
                        region.view.leader_peer_id = to_peer_id;
                    }
                }
            }
            self.report.moves_applied += 1;
            // One move per round, because that is what `max_balance_operators` and the
            // per-region cooldown give a real placement driver.
            break;
        }
    }
}
