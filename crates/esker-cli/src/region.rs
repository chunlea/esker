//! `esker region ls / split / transfer-leader` — the operator's view of how a cluster is laid out.
//!
//! Three questions an operator has that no other command answers: *where are my regions*, *cut
//! this one here*, and *move this one's leadership*. The placement driver decides all three for
//! itself in the ordinary course of things (`docs/DESIGN.md` §7); these are for the times when a
//! person knows something the scheduler does not — a migration, a hot range, a store being drained
//! before maintenance.
//!
//! # Routing goes through the placement driver, the work goes to a store
//!
//! PD knows which region covers a key and which stores its peers are on; only the region's
//! **leader** can propose a split or hand over leadership. So every command here resolves through
//! PD and then talks to a store directly, and a store that is not the leader says so rather than
//! forwarding — an operator's tool that quietly did the work somewhere else would make "which
//! store did this" unanswerable, which is the one thing the tool is for.
//!
//! `ls` asks for **pages** of the routing table (`Pd::ScanRegions`) rather than one region at a
//! time. It used to walk with `GetRegion` — ask for `""`, take the region's end key, ask again —
//! which was correct and `O(regions)` round trips.
//!
//! **The walk's check survives the change, and that is the part worth keeping.** A gap or an
//! overlap shows up as a region whose start is not the previous one's end, and it is reported
//! rather than smoothed over. The old shape got that for free, because it asked *by key* and a gap
//! answered "no region"; a page of records has to check it explicitly, so it does — across page
//! boundaries too, which is where a scan that only checked within a page would miss one.
//!
//! # Exit codes
//!
//! `0` it worked · `2` the arguments were wrong · `3` the cluster refused, or could not be reached.

use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use esker_proto::{
    AdminReq, AdminResp, BlockingTransport, LeaderBook, PdReq, PdResp, ProtoError, Redirects,
    Region, Request, TransportConfig,
};

use crate::bytes::escape;

/// Where `--pd` points when nothing says otherwise.
pub(crate) const DEFAULT_PD: &str = "127.0.0.1:2379";

/// One region, as an operator needs to see it: what it is, who leads it, and where its peers'
/// stores can be reached.
///
/// A named type because it is the answer to every question here and three functions return it.
type Located = (Region, Option<u64>, Vec<(u64, String)>);

/// How long one admin call may take. A split proposes a Raft entry and waits for it to apply, so
/// it is not instant; nothing here should hang for ever either.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// What `esker region` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RegionCommand {
    /// Print every region in the cluster, in key order.
    Ls,
    /// Split the region covering `key`, at `key`.
    Split {
        /// Where to cut. It becomes the new region's first key.
        key: Bytes,
    },
    /// Hand a region's leadership to one of its peers.
    TransferLeader {
        /// Which region.
        region_id: u64,
        /// Which peer should take office.
        to_peer_id: u64,
    },
}

/// How `esker region` was configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegionOptions {
    /// The placement driver to route through.
    pub(crate) pd: String,
    /// What to do.
    pub(crate) command: RegionCommand,
    /// Read the key as hex, and print keys as hex.
    pub(crate) hex: bool,
}

impl Default for RegionOptions {
    fn default() -> Self {
        Self {
            pd: DEFAULT_PD.to_owned(),
            command: RegionCommand::Ls,
            hex: false,
        }
    }
}

/// Runs one `esker region` command.
pub(crate) fn run(options: &RegionOptions) -> Result<(), String> {
    let pd: SocketAddr = options
        .pd
        .parse()
        .map_err(|error| format!("`--pd {}` is not an address: {error}", options.pd))?;
    let pd = PdConn::connect(pd)?;

    match &options.command {
        RegionCommand::Ls => list(&pd, options.hex),
        RegionCommand::Split { key } => split(&pd, key, options.hex),
        RegionCommand::TransferLeader {
            region_id,
            to_peer_id,
        } => transfer(&pd, *region_id, *to_peer_id),
    }
}

/// A blocking connection to the placement driver, and the cluster it turned out to serve.
///
/// Blocking on purpose: `esker-cli` is ordinary synchronous code and `CLAUDE.md` keeps async at
/// the network edge, so the runtime lives inside the transport and this file never sees a future.
///
/// # The cluster id is asked for, not assumed
///
/// Every PD call but `Bootstrap` carries the id of the cluster it is meant for, and PD refuses a
/// mismatch outright (`docs/adr/0011-pd-service-and-the-cluster-id.md`) — that refusal is the
/// whole point of the field, because two clusters sharing a `--pd` by accident is a
/// misconfiguration nobody wants smoothed over.
///
/// These three commands used to send a literal `0`, which is the id of no cluster: against a
/// bootstrapped PD every one of them was refused before it did anything. `0` is not a wildcard
/// and PD is right to say so.
///
/// A store learns the id by bootstrapping; `esker region` has no store to register, so it learns
/// it the only other way PD offers — the refusal names the cluster PD serves, so the first call
/// adopts that id and retries, and every call afterwards carries it. One extra round trip per
/// invocation, on a tool a person runs by hand.
pub(crate) struct PdConn {
    /// Who is in the group, and which member this tool believes leads it.
    book: LeaderBook,
    config: TransportConfig,
    /// The live connection and the member it is to, or `None` before the first call and after a
    /// failed one. The **address** is what identifies it: a redirect leaves a healthy connection
    /// to the member that has just said it is the wrong one.
    transport: RefCell<Option<(SocketAddr, Arc<BlockingTransport>)>>,
    /// The cluster PD said it serves, or `0` before it has said.
    cluster_id: Cell<u64>,
}

impl PdConn {
    pub(crate) fn connect(address: SocketAddr) -> Result<Self, String> {
        Self::connect_with(address, TransportConfig::new())
    }

    /// One member, **connected now**, with an explicit transport configuration.
    ///
    /// `request_timeout` bounds the wire handshake as well as every call on the connection, so a
    /// caller that must not wait the default thirty seconds for a socket which accepts and then
    /// says nothing sets it here. The one such caller is `cluster start`'s readiness probe
    /// ([`crate::cluster`]), which asks a driver that may not be up yet — and which is also why
    /// this one connects before it returns: *"is this driver up"* is the question, and a lazy
    /// connection would answer it one line later and in another sentence.
    pub(crate) fn connect_with(
        address: SocketAddr,
        config: TransportConfig,
    ) -> Result<Self, String> {
        let held = BlockingTransport::connect_with(address, config)
            .map_err(|error| format!("connecting to the placement driver at {address}: {error}"))?;
        Ok(Self {
            book: LeaderBook::lone(address),
            config,
            transport: RefCell::new(Some((address, Arc::new(held)))),
            cluster_id: Cell::new(0),
        })
    }

    /// A placement-driver **group**, given every member's address.
    ///
    /// Only the leader answers, so this follows the group's own hints — and, when a member is
    /// killed rather than deposed, moves past the one it cannot reach
    /// ([`esker_proto::LeaderBook`],
    /// [ADR 0108](../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)).
    ///
    /// **Connected lazily**, unlike [`Self::connect_with`]: the first member of a list is not
    /// necessarily the one that will answer, so refusing to build the client because that one is
    /// down would be refusing on the strength of the wrong member.
    pub(crate) fn connect_to(
        endpoints: &[SocketAddr],
        config: TransportConfig,
    ) -> Result<Self, String> {
        let book =
            LeaderBook::new(endpoints).map_err(|error| format!("the placement driver: {error}"))?;
        Ok(Self {
            book,
            config,
            transport: RefCell::new(None),
            cluster_id: Cell::new(0),
        })
    }

    /// The member this tool currently believes leads.
    pub(crate) fn address(&self) -> SocketAddr {
        self.book.believed()
    }

    /// One PD call, to whichever member of the group this tool believes leads it.
    ///
    /// The rules are [`esker_proto::LeaderBook`]'s, and every method this file and
    /// [`crate::durability`] send through here is safe to send again: `Tso` hands out fresh
    /// timestamps and never reuses one, and the rest are reads.
    pub(crate) fn call(&self, request: &PdReq) -> Result<PdResp, ProtoError> {
        let mut redirects = Redirects::new();
        loop {
            match self.attempt(request) {
                Ok(response) => return Ok(response),
                Err(ProtoError::PdNotLeader {
                    leader_id,
                    leader_address,
                }) => {
                    if !redirects.take() {
                        return Err(ProtoError::PdNotLeader {
                            leader_id,
                            leader_address,
                        });
                    }
                    if !leader_address.is_empty() && self.follow(&leader_address) {
                        continue;
                    }
                    self.book.advance();
                    std::thread::sleep(redirects.backoff());
                }
                // A member that was killed answers nothing at all, so nothing above moves this
                // client off it. `NotSent` is a request that provably never left this process.
                Err(other) if esker_proto::is_unreachable(&other) && redirects.take() => {
                    self.book.advance();
                    std::thread::sleep(redirects.backoff());
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// Points this client at the member `hint` names, and says whether it moved.
    fn follow(&self, hint: &str) -> bool {
        self.book
            .follow(hint, || match self.attempt(&PdReq::Members)? {
                PdResp::Members(membership) => Ok(membership),
                other => Err(ProtoError::invalid(format!(
                    "asked the placement driver who is in its group and it answered {}",
                    other.method().name()
                ))),
            })
    }

    /// One attempt, addressed to the cluster this connection has learned about.
    ///
    /// Retried **once** and only on a mismatch that names a different cluster, so a PD that
    /// somehow refused the id it had just given would be reported rather than looped on.
    fn attempt(&self, request: &PdReq) -> Result<PdResp, ProtoError> {
        let transport = self.connection()?;
        let deadline = || std::time::Instant::now() + CALL_TIMEOUT;
        let known = self.cluster_id.get();
        let answer =
            match transport.call(esker_proto::pd::encode(known, request.clone()), deadline()) {
                Err(ProtoError::ClusterMismatch { expected, .. }) if expected != known => {
                    self.cluster_id.set(expected);
                    transport.call(
                        esker_proto::pd::encode(expected, request.clone()),
                        deadline(),
                    )
                }
                other => other,
            };
        match answer {
            Ok(response) => esker_proto::pd::decode(response),
            Err(error) => {
                // The connection is suspect after any failure, and the next call builds a fresh
                // one — to whichever member the book believes by then.
                *self.transport.borrow_mut() = None;
                Err(error)
            }
        }
    }

    /// The live connection to the member this client believes leads, or a new one.
    fn connection(&self) -> Result<Arc<BlockingTransport>, ProtoError> {
        let want = self.book.believed();
        let mut slot = self.transport.borrow_mut();
        if let Some((held, existing)) = slot.as_ref()
            && *held == want
            && !existing.is_closed()
        {
            return Ok(Arc::clone(existing));
        }
        let fresh = Arc::new(BlockingTransport::connect_with(want, self.config)?);
        *slot = Some((want, Arc::clone(&fresh)));
        Ok(fresh)
    }
}

/// Every region in the cluster, in key order, a page at a time.
///
/// The contiguity check is this function's, not PD's: each region must start exactly where the
/// previous one ended, and the last must run to the end of the key space. A gap or an overlap is
/// **reported**, because a routing table that is not a partition is the kind of bug an operator's
/// tool exists to surface rather than to render tidily.
fn walk(pd: &PdConn) -> Result<Vec<Located>, String> {
    let mut found: Vec<Located> = Vec::new();
    let mut next = Bytes::new();
    loop {
        let PdResp::ScanRegions { regions, stores } = pd
            .call(&PdReq::ScanRegions {
                start_key: next.clone(),
                limit: 0,
            })
            .map_err(|error| {
                format!(
                    "asking the placement driver for the regions from {}: {error}",
                    escape(&next)
                )
            })?
        else {
            return Err("the placement driver answered a different question".to_owned());
        };
        if regions.is_empty() {
            // At the start of the key space that is a cluster with nothing in it. Anywhere else
            // it is a gap: something ended and nothing begins there.
            if found.is_empty() {
                return Ok(found);
            }
            return Err(format!(
                "no region covers {}, which is a gap in the key space after region {}",
                escape(&next),
                found.last().map_or(0, |(region, _, _)| region.id)
            ));
        }
        let addresses: Vec<(u64, String)> = stores
            .into_iter()
            .map(|store| (store.store_id, store.address))
            .collect();

        let mut end = Bytes::new();
        for scanned in regions {
            // The check the old per-key walk got for free. `next` is where the previous region
            // ended — or the empty key on the first page — so a region that does not start there
            // is a gap or an overlap, and this catches it **across** page boundaries as well as
            // within one.
            if scanned.region.start_key != next {
                return Err(format!(
                    "region {} starts at {} where {} was expected: the routing table is not a \
                     partition of the key space",
                    scanned.region.id,
                    escape(&scanned.region.start_key),
                    escape(&next),
                ));
            }
            end = scanned.region.end_key.clone();
            next = end.clone();
            let leader = (scanned.leader_peer_id != 0).then_some(scanned.leader_peer_id);
            // Only the stores this region's peers are on, out of the page's shared list: the
            // caller prints them per region and a region must not be shown a peer it has not got.
            let peers: Vec<(u64, String)> = addresses
                .iter()
                .filter(|(store_id, _)| {
                    scanned
                        .region
                        .peers
                        .iter()
                        .any(|peer| peer.store_id == *store_id)
                })
                .cloned()
                .collect();
            found.push((scanned.region, leader, peers));
        }

        if end.is_empty() {
            // The last region runs to the end of the key space, so the table is complete.
            return Ok(found);
        }
        // A short page means the table ended without the last region closing the key space,
        // which is a gap at `next` — caught by the empty page the next round asks for, so there
        // is one place that reports it rather than two.
    }
}

fn list(pd: &PdConn, hex: bool) -> Result<(), String> {
    let regions = walk(pd)?;
    if regions.is_empty() {
        println!("no regions: the cluster has not been bootstrapped");
        return Ok(());
    }

    println!(
        "{:>8}  {:>10}  {:<20}  {:<20}  peers",
        "region", "epoch", "start", "end"
    );
    for (region, leader, stores) in &regions {
        let peers: Vec<String> = region
            .peers
            .iter()
            .map(|peer| {
                let here = if Some(peer.peer_id) == *leader {
                    "*"
                } else {
                    ""
                };
                // `C` and `L` are different facts, not two spellings of one. A learner is on its
                // way to being a voter; a **columnar** learner never is (ADR 0022 Decision 1), so
                // an operator counting voters must not read one as the other — which is exactly
                // the mistake that made two of PD's own schedulers report a healthy cluster that
                // was not (`docs/plans/phase-8-learner.md` §wire).
                let role = match peer.role {
                    esker_proto::PeerRole::Learner => "L",
                    esker_proto::PeerRole::ColumnarLearner => "C",
                    esker_proto::PeerRole::Voter => "",
                };
                let address = stores
                    .iter()
                    .find(|(store_id, _)| *store_id == peer.store_id)
                    .map_or_else(|| "?".to_owned(), |(_, address)| address.clone());
                format!("{here}{}{role}@{address}", peer.peer_id)
            })
            .collect();
        println!(
            "{:>8}  {:>4},{:<5}  {:<20}  {:<20}  {}",
            region.id,
            region.epoch.conf_ver,
            region.epoch.version,
            show(&region.start_key, hex),
            if region.end_key.is_empty() {
                "+inf".to_owned()
            } else {
                show(&region.end_key, hex)
            },
            peers.join(" ")
        );
    }
    println!(
        "\n{} regions; * is the leader, L a learner, C a columnar learner",
        regions.len()
    );
    Ok(())
}

fn split(pd: &PdConn, key: &Bytes, hex: bool) -> Result<(), String> {
    let (region, leader, stores) = locate(pd, key)?;
    let address = leader_address(&region, leader, &stores)?;
    let store = BlockingTransport::connect_with(address, TransportConfig::new())
        .map_err(|error| format!("connecting to the leader at {address}: {error}"))?;

    let response = store
        .call(
            Request::Admin(AdminReq::Split {
                region_id: region.id,
                split_key: key.clone(),
            }),
            std::time::Instant::now() + CALL_TIMEOUT,
        )
        .map_err(|error| format!("splitting region {}: {error}", region.id))?;
    let esker_proto::Response::Admin(AdminResp::Split { left, right }) = response else {
        return Err(format!("the store answered {response:?}"));
    };
    println!(
        "region {} split at {}",
        left.id,
        show(&right.start_key, hex)
    );
    println!(
        "  {:>8}  [{}, {})",
        left.id,
        show(&left.start_key, hex),
        show(&left.end_key, hex)
    );
    println!(
        "  {:>8}  [{}, {})",
        right.id,
        show(&right.start_key, hex),
        if right.end_key.is_empty() {
            "+inf".to_owned()
        } else {
            show(&right.end_key, hex)
        }
    );
    Ok(())
}

fn transfer(pd: &PdConn, region_id: u64, to_peer_id: u64) -> Result<(), String> {
    // A transfer names a region rather than a key, so the walk is how its leader is found.
    let regions = walk(pd)?;
    let (region, leader, stores) = regions
        .into_iter()
        .find(|(region, _, _)| region.id == region_id)
        .ok_or_else(|| format!("no region {region_id} in this cluster"))?;
    if !region.peers.iter().any(|peer| peer.peer_id == to_peer_id) {
        return Err(format!(
            "region {region_id} has no peer {to_peer_id}; it has {:?}",
            region
                .peers
                .iter()
                .map(|peer| peer.peer_id)
                .collect::<Vec<_>>()
        ));
    }
    let address = leader_address(&region, leader, &stores)?;
    let store = BlockingTransport::connect_with(address, TransportConfig::new())
        .map_err(|error| format!("connecting to the leader at {address}: {error}"))?;

    store
        .call(
            Request::Admin(AdminReq::TransferLeader {
                region_id,
                to_peer_id,
            }),
            std::time::Instant::now() + CALL_TIMEOUT,
        )
        .map_err(|error| format!("transferring region {region_id}: {error}"))?;
    // *Asked*, not *done*: what completes a transfer is an election, and the leader that started
    // it cannot promise the outcome (`docs/DESIGN.md` §6).
    println!("region {region_id}: leadership asked to move to peer {to_peer_id}");
    println!("run `esker region ls` to see whether it did");
    Ok(())
}

/// The region covering `key`, as the placement driver has it.
fn locate(pd: &PdConn, key: &Bytes) -> Result<Located, String> {
    let PdResp::GetRegion {
        region,
        leader_peer_id,
        stores,
    } = pd
        .call(&PdReq::GetRegion { key: key.clone() })
        .map_err(|error| format!("asking the placement driver about {key:?}: {error}"))?
    else {
        return Err("the placement driver answered a different question".to_owned());
    };
    let region = region.ok_or_else(|| format!("no region covers {}", escape(key)))?;
    Ok((
        region,
        (leader_peer_id != 0).then_some(leader_peer_id),
        stores
            .into_iter()
            .map(|store| (store.store_id, store.address))
            .collect(),
    ))
}

/// Where a region's leader can be reached.
///
/// A region PD has no leader for is one nothing can be asked of: refusing here beats sending the
/// request to a follower and reading a `NotLeader` back, because the operator's next question is
/// "which store" and this answers it.
fn leader_address(
    region: &Region,
    leader: Option<u64>,
    stores: &[(u64, String)],
) -> Result<SocketAddr, String> {
    let leader = leader.ok_or_else(|| {
        format!(
            "the placement driver does not know who leads region {}; try again once it has heard \
             a heartbeat",
            region.id
        )
    })?;
    let store_id = region
        .peers
        .iter()
        .find(|peer| peer.peer_id == leader)
        .map(|peer| peer.store_id)
        .ok_or_else(|| format!("region {} does not list its own leader {leader}", region.id))?;
    let address = stores
        .iter()
        .find(|(id, _)| *id == store_id)
        .map(|(_, address)| address.clone())
        .ok_or_else(|| format!("the placement driver has no address for store {store_id}"))?;
    address
        .parse()
        .map_err(|error| format!("store {store_id}'s address `{address}` is not usable: {error}"))
}

/// A key as the operator asked to see it.
fn show(key: &[u8], hex: bool) -> String {
    if hex {
        use std::fmt::Write;
        key.iter().fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
    } else {
        escape(key)
    }
}

#[cfg(test)]
mod tests {
    use super::{PdConn, RegionCommand, RegionOptions, run, show};
    use crate::testserver::TestPd;
    use esker_proto::PdReq;

    #[test]
    fn the_defaults_are_a_listing_against_the_local_placement_driver() {
        let options = RegionOptions::default();
        assert_eq!(options.command, RegionCommand::Ls);
        assert_eq!(options.pd, "127.0.0.1:2379");
        assert!(!options.hex);
    }

    /// The defect the phase-4 acceptance runs found: these three commands addressed cluster `0`,
    /// which is the id of no cluster, so a bootstrapped PD refused every one of them before they
    /// did anything.
    ///
    /// Against a PD that has never been bootstrapped it looked fine, which is why it survived —
    /// so this test bootstraps, exactly as any cluster an operator would point the tool at has
    /// been.
    ///
    /// Mutation check: sending a literal `0` from `PdConn::call` instead of the learned id
    /// turns this into `request is for cluster 0, this is cluster N`.
    #[test]
    fn a_pd_call_is_addressed_to_the_cluster_pd_says_it_serves() {
        let pd = TestPd::start(3);
        assert_ne!(pd.cluster_id(), 0, "a bootstrapped cluster has a real id");

        let connection = PdConn::connect(pd.addr().parse().expect("an address")).expect("connects");
        assert_eq!(
            connection.cluster_id.get(),
            0,
            "nothing is known before a call"
        );

        connection
            .call(&PdReq::GetRegion {
                key: bytes::Bytes::new(),
            })
            .expect("the placement driver answers");
        assert_eq!(
            connection.cluster_id.get(),
            pd.cluster_id(),
            "the refusal named the cluster and the connection did not take it"
        );

        // And it stays learned, so only the first call of a session pays for the discovery.
        connection
            .call(&PdReq::GetRegion {
                key: bytes::Bytes::new(),
            })
            .expect("the second call is addressed correctly");
        assert_eq!(connection.cluster_id.get(), pd.cluster_id());
    }

    /// The same defect from the outside: `esker region ls` against a bootstrapped cluster. It is
    /// the command an operator reaches for first, and it was the one that could not run at all.
    #[test]
    fn region_ls_runs_against_a_bootstrapped_cluster() {
        let pd = TestPd::start(1);
        let options = RegionOptions {
            pd: pd.addr(),
            command: RegionCommand::Ls,
            hex: false,
        };
        // No region has been placed yet, so the listing is empty — but it is an *answer*, which
        // is what the fixed cluster id was preventing.
        assert_eq!(run(&options), Ok(()));
    }

    /// **The paged walk**, over more regions than one `GetRegion` round trip each would be worth.
    ///
    /// Sixty contiguous regions in PD's table, listed in one command. What is asserted is that the
    /// walk returns them all, in key order, and that it composes a partition — the check the old
    /// per-key walk got for free from PD answering "no region".
    #[test]
    fn ls_pages_the_whole_routing_table_in_key_order() {
        use esker_proto::{Epoch, Peer, Region};

        let pd = TestPd::start(1);
        let count = 60_u64;
        for index in 0..count {
            let start = if index == 0 {
                bytes::Bytes::new()
            } else {
                bytes::Bytes::from(format!("k{index:03}"))
            };
            let end = if index == count - 1 {
                bytes::Bytes::new()
            } else {
                bytes::Bytes::from(format!("k{:03}", index + 1))
            };
            pd.pd()
                .region_heartbeat(&esker_pd::RegionBeat {
                    region: Region {
                        id: index + 1,
                        start_key: start,
                        end_key: end,
                        peers: vec![Peer::voter(1, index + 10)],
                        epoch: Epoch::new(1, index + 1),
                    },
                    leader_peer_id: index + 10,
                    term: 1,
                    approximate_size: 0,
                    applied_index: 0,
                })
                .expect("the heartbeat is recorded");
        }

        let connection = PdConn::connect(pd.addr().parse().unwrap()).unwrap();
        let found = super::walk(&connection).expect("the walk lists the table");
        assert_eq!(
            found.len(),
            usize::try_from(count).unwrap(),
            "the walk lost regions"
        );

        // **The point of the unit**, and the count is the only place it is visible. The walk this
        // replaced asked `GetRegion` once per region.
        //
        // Two calls rather than one, because the *first* call of a session is refused with the
        // cluster id and retried — `PdConn`'s documented discovery round trip, which every command
        // here pays once. So the listing itself is one call, and a second listing on the same
        // connection proves it: one more, not sixty-one more.
        assert_eq!(
            pd.calls("Pd::ScanRegions"),
            2,
            "sixty regions took {} scans, discovery included",
            pd.calls("Pd::ScanRegions"),
        );
        let again = super::walk(&connection).expect("a second listing");
        assert_eq!(again.len(), found.len());
        assert_eq!(
            pd.calls("Pd::ScanRegions"),
            3,
            "a warm listing of sixty regions cost more than one call"
        );
        assert_eq!(
            pd.calls("Pd::GetRegion"),
            0,
            "the listing still asks about regions one key at a time"
        );

        let mut expected = bytes::Bytes::new();
        for (region, leader, stores) in &found {
            assert_eq!(
                region.start_key, expected,
                "region {} is out of order or leaves a gap",
                region.id
            );
            expected = region.end_key.clone();
            assert_eq!(*leader, Some(region.peers[0].peer_id));
            assert_eq!(
                stores.len(),
                1,
                "a region was shown the page's whole store list rather than its own peers'"
            );
        }
        assert!(
            expected.is_empty(),
            "the last region does not close the key space"
        );

        // And the command itself runs, which is the thing an operator types.
        assert_eq!(
            run(&RegionOptions {
                pd: pd.addr(),
                command: RegionCommand::Ls,
                hex: false,
            }),
            Ok(())
        );
    }

    /// A key is bytes and a terminal is not. Escaped by default, hex on request, and never the
    /// raw bytes — the same rule every dump in this crate follows.
    #[test]
    fn a_key_is_shown_escaped_or_as_hex_and_never_raw() {
        assert_eq!(show(b"plain", false), "plain");
        assert_eq!(show(b"\x00\xff", false), "\\x00\\xff");
        assert_eq!(show(b"\x00\xff", true), "00ff");
        assert_eq!(show(b"", true), "");
    }
}
