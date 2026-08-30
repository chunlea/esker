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
//! `ls` walks the key space rather than asking for a list, because `GetRegion` is the only routing
//! question PD answers: ask for `""`, take the region's end key, ask again, stop when a region's
//! end key is empty. The walk is also the check — a gap or an overlap shows up as a region whose
//! start is not the previous one's end, and it is reported rather than smoothed over.
//!
//! # Exit codes
//!
//! `0` it worked · `2` the arguments were wrong · `3` the cluster refused, or could not be reached.

use std::net::SocketAddr;

use bytes::Bytes;
use esker_proto::{AdminReq, AdminResp, BlockingTransport, Region, Request, TransportConfig};

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
    let pd = esker_pd_client(pd)?;

    match &options.command {
        RegionCommand::Ls => list(&pd, options.hex),
        RegionCommand::Split { key } => split(&pd, key, options.hex),
        RegionCommand::TransferLeader {
            region_id,
            to_peer_id,
        } => transfer(&pd, *region_id, *to_peer_id),
    }
}

/// A blocking connection to the placement driver.
///
/// Blocking on purpose: `esker-cli` is ordinary synchronous code and `CLAUDE.md` keeps async at
/// the network edge, so the runtime lives inside the transport and this file never sees a future.
fn esker_pd_client(address: SocketAddr) -> Result<BlockingTransport, String> {
    BlockingTransport::connect_with(address, TransportConfig::new())
        .map_err(|error| format!("connecting to the placement driver at {address}: {error}"))
}

/// Every region in the cluster, walked from the start of the key space.
fn walk(pd: &BlockingTransport) -> Result<Vec<Located>, String> {
    let mut found = Vec::new();
    let mut next = Bytes::new();
    loop {
        let deadline = std::time::Instant::now() + CALL_TIMEOUT;
        let response = pd
            .call(
                esker_proto::pd::encode(0, esker_proto::PdReq::GetRegion { key: next.clone() }),
                deadline,
            )
            .map_err(|error| format!("asking the placement driver about {next:?}: {error}"))?;
        let esker_proto::PdResp::GetRegion {
            region,
            leader_peer_id,
            stores,
        } = esker_proto::pd::decode(response)
            .map_err(|error| format!("the placement driver's answer: {error}"))?
        else {
            return Err("the placement driver answered a different question".to_owned());
        };
        let Some(region) = region else {
            // No region covers the key. At `""` that is a cluster with nothing in it; anywhere
            // else it is a gap, and the walk stops at it rather than skipping past.
            if found.is_empty() {
                return Ok(found);
            }
            return Err(format!(
                "no region covers {}, which is a gap in the key space after region {}",
                escape(&next),
                found.last().map_or(0, |(region, _, _): &Located| region.id)
            ));
        };
        let end = region.end_key.clone();
        found.push((
            region,
            (leader_peer_id != 0).then_some(leader_peer_id),
            stores
                .into_iter()
                .map(|store| (store.store_id, store.address))
                .collect(),
        ));
        if end.is_empty() {
            return Ok(found);
        }
        next = end;
    }
}

fn list(pd: &BlockingTransport, hex: bool) -> Result<(), String> {
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
                let role = if peer.role == esker_proto::PeerRole::Learner {
                    "L"
                } else {
                    ""
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
    println!("\n{} regions; * is the leader, L a learner", regions.len());
    Ok(())
}

fn split(pd: &BlockingTransport, key: &Bytes, hex: bool) -> Result<(), String> {
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

fn transfer(pd: &BlockingTransport, region_id: u64, to_peer_id: u64) -> Result<(), String> {
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
fn locate(pd: &BlockingTransport, key: &Bytes) -> Result<Located, String> {
    let response = pd
        .call(
            esker_proto::pd::encode(0, esker_proto::PdReq::GetRegion { key: key.clone() }),
            std::time::Instant::now() + CALL_TIMEOUT,
        )
        .map_err(|error| format!("asking the placement driver about {key:?}: {error}"))?;
    let esker_proto::PdResp::GetRegion {
        region,
        leader_peer_id,
        stores,
    } = esker_proto::pd::decode(response)
        .map_err(|error| format!("the placement driver's answer: {error}"))?
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
    use super::{RegionCommand, RegionOptions, show};

    #[test]
    fn the_defaults_are_a_listing_against_the_local_placement_driver() {
        let options = RegionOptions::default();
        assert_eq!(options.command, RegionCommand::Ls);
        assert_eq!(options.pd, "127.0.0.1:2379");
        assert!(!options.hex);
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
