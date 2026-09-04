//! `esker pd` — run the placement driver, or look at what it has stored.
//!
//! `serve` is the process `docs/DESIGN.md` §7 describes: one PD, its state in its own engine,
//! answering the six methods of service `0x03`. `inspect` opens that state directly and prints
//! it, the way `sst-dump` and `manifest-dump` read the formats they belong to — so a cluster
//! that is misrouting can be diagnosed without a client and without guessing.
//!
//! Phase 4a is a **single** PD. It is a single point of failure, and that is a stated property
//! of the sub-phase rather than an oversight: high availability is 4e, where three of these
//! replicate with `esker-raft` (`prompts/04-multiraft-pd.md`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use esker_pd::{MemberList, Pd, PdInspector, PdMember, PdOptions, PdService, PdTcpTransport};
use esker_proto::{Server, TransportConfig};

/// Where `--listen` points when nothing says otherwise.
///
/// The port PD's clients use in `TiKV`, which this layer is modelled on (`CLAUDE.md`): a
/// familiar number is kinder than a new one.
pub(crate) const DEFAULT_LISTEN: &str = "127.0.0.1:2379";

/// Where PD keeps its state when nothing says otherwise.
pub(crate) const DEFAULT_DATA_DIR: &str = "esker-pd-data";

/// What `esker pd` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PdCommand {
    /// Open PD's state and serve it.
    Serve(ServeOptions),
    /// Print what PD has stored, without serving anything.
    Inspect(InspectOptions),
    /// Ask a **running** PD what it is doing right now.
    Status(StatusOptions),
    /// Ask a **running** PD who is in its group and which member leads.
    Members(MembersOptions),
}

/// `esker pd members`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MembersOptions {
    /// Any member of the group to ask. A follower answers this one, which is the point: it is
    /// what an operator reaches for when the leader is the thing that is missing.
    pub(crate) pd: String,
}

impl Default for MembersOptions {
    fn default() -> Self {
        Self {
            pd: crate::region::DEFAULT_PD.to_owned(),
        }
    }
}

/// `esker pd status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusOptions {
    /// The placement driver to ask.
    pub(crate) pd: String,
}

impl Default for StatusOptions {
    fn default() -> Self {
        Self {
            pd: crate::region::DEFAULT_PD.to_owned(),
        }
    }
}

/// `esker pd serve`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServeOptions {
    /// The directory holding PD's database. Created if it is not there.
    pub(crate) data_dir: PathBuf,
    /// The address to listen on.
    pub(crate) listen: String,
    /// This placement driver's member id within its group.
    ///
    /// Defaults to 1, which with no `--peers` is the single durable placement driver of phase 4a
    /// and is what every existing invocation gets.
    pub(crate) id: u64,
    /// The whole group, as `id@host:port` separated by commas — **this member included**.
    ///
    /// Empty means a group of one. Every member must be given the *same* list, in any order:
    /// the group's identity is derived from it ([`esker_pd::MemberList`]), so two members given
    /// different lists are two groups and will not talk to each other — which is a loud failure
    /// rather than a quiet half-formed cluster.
    pub(crate) peers: String,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            listen: DEFAULT_LISTEN.to_owned(),
            id: 1,
            peers: String::new(),
        }
    }
}

/// Reads a `--peers` list: `id@host:port`, comma-separated, this member included.
///
/// The `id@` prefix is required rather than inferred from position, because a list whose meaning
/// depends on its order is one that two operators will write differently — and two different
/// orders would be two different groups if the id came from the position.
pub(crate) fn parse_peers(raw: &str) -> Result<Vec<PdMember>, String> {
    let mut members = Vec::new();
    for part in raw
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let Some((id, address)) = part.split_once('@') else {
            return Err(format!(
                "`{part}` is not a placement driver; write it as `id@host:port`"
            ));
        };
        let id: u64 = id
            .parse()
            .map_err(|_| format!("`{id}` in `{part}` is not a member id"))?;
        members.push(PdMember::new(id, address));
    }
    Ok(members)
}

/// `esker pd inspect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InspectOptions {
    /// The directory holding PD's database. It must already exist.
    pub(crate) data_dir: PathBuf,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
        }
    }
}

/// Runs the command, and returns when it has finished.
pub(crate) fn run(command: &PdCommand) -> Result<(), String> {
    match command {
        PdCommand::Serve(options) => serve(options),
        PdCommand::Inspect(options) => {
            let mut stdout = std::io::stdout().lock();
            inspect(options, &mut stdout)
        }
        PdCommand::Status(options) => {
            let mut stdout = std::io::stdout().lock();
            status(options, &mut stdout)
        }
        PdCommand::Members(options) => {
            let mut stdout = std::io::stdout().lock();
            members(options, &mut stdout)
        }
    }
}

/// Asks a **running** placement driver who is in its group and which member leads.
///
/// Answered by any member, leader or not, which is the whole point: an operator reaches for this
/// exactly when the leader is the thing that is missing, and a command that only the leader could
/// answer would be useless then. It rides on `Pd::Status`, which is exempt from the leader check
/// for the same reason ([`esker_pd::service`]).
pub(crate) fn members(
    options: &MembersOptions,
    out: &mut impl std::io::Write,
) -> Result<(), String> {
    let address: SocketAddr = options
        .pd
        .parse()
        .map_err(|error| format!("`--pd {}` is not an address: {error}", options.pd))?;
    let pd = crate::region::PdConn::connect(address)?;
    let response = pd
        .call(&esker_proto::PdReq::Members)
        .map_err(|error| format!("asking the placement driver for its members: {error}"))?;
    let esker_proto::PdResp::Members(membership) = response else {
        return Err("the placement driver answered a different question".to_owned());
    };
    print_members(&membership, out)
}

fn print_members(
    members: &esker_proto::PdMembership,
    out: &mut impl std::io::Write,
) -> Result<(), String> {
    let write = |error: std::io::Error| format!("writing: {error}");
    writeln!(
        out,
        "group {:#018x}, term {}",
        members.group_id, members.term
    )
    .map_err(write)?;
    if members.members.is_empty() {
        writeln!(out, "  (no members reported)").map_err(write)?;
        return Ok(());
    }
    for member in &members.members {
        let role = if member.id == members.leader_id {
            "leader"
        } else if member.id == members.this_id {
            "follower (this one)"
        } else {
            "follower"
        };
        let here = if member.id == members.this_id {
            " *"
        } else {
            ""
        };
        writeln!(
            out,
            "  {:>3}  {:<24} {role}{here}",
            member.id, member.address
        )
        .map_err(write)?;
    }
    if members.leader_id == 0 {
        writeln!(out, "  no leader: an election is in progress").map_err(write)?;
    }
    Ok(())
}

/// Asks a **running** placement driver what it has in flight.
///
/// `inspect` opens a *stopped* PD's database and therefore cannot see an operator at all: the
/// in-flight set is memory and dies with the process
/// ([ADR 0013](../../../docs/adr/0013-repair-operators-are-requests-not-commands.md)). The two
/// commands are complements, not alternatives — `inspect` answers "what does PD believe about the
/// cluster", this one answers "what is it doing about it".
pub(crate) fn status(options: &StatusOptions, out: &mut impl std::io::Write) -> Result<(), String> {
    let address: SocketAddr = options
        .pd
        .parse()
        .map_err(|error| format!("`--pd {}` is not an address: {error}", options.pd))?;
    let pd = crate::region::PdConn::connect(address)?;
    let response = pd
        .call(&esker_proto::PdReq::Status)
        .map_err(|error| format!("asking the placement driver for its status: {error}"))?;
    let esker_proto::PdResp::Status { now_ms, operators } = response else {
        return Err("the placement driver answered a different question".to_owned());
    };
    print_status(now_ms, &operators, out)
}

/// The report itself, taking the answer rather than the connection so a test can drive it.
fn print_status(
    now_ms: u64,
    operators: &[esker_proto::OperatorStatus],
    out: &mut impl std::io::Write,
) -> Result<(), String> {
    let write = |error: std::io::Error| format!("writing: {error}");
    writeln!(out, "operators in flight ({})", operators.len()).map_err(write)?;
    if operators.is_empty() {
        // Said rather than left blank: an empty report and a broken one look the same otherwise,
        // and "PD has nothing to do" is the answer an operator is usually hoping for.
        writeln!(out, "  (none — PD is not moving anything)").map_err(write)?;
        return Ok(());
    }
    for status in operators {
        let operator = &status.operator;
        let epoch = operator.epoch();
        writeln!(
            out,
            "  region {:<5} {:<15} {:<8} epoch ({},{})  age {}  since {}  sends {}",
            operator.region_id(),
            operator.name(),
            status.progress.name(),
            epoch.conf_ver,
            epoch.version,
            age(now_ms, status.issued_ms),
            age(now_ms, status.since_ms),
            status.sends,
        )
        .map_err(write)?;
    }
    Ok(())
}

/// How long ago `then_ms` was, on PD's clock.
///
/// Saturating, and that is not paranoia: PD's clock is the only one in the answer, but a record
/// written before a clock adjustment can still sit above `now_ms`, and an age that wrapped to
/// nineteen billion seconds would be read as a hung operator.
fn age(now_ms: u64, then_ms: u64) -> String {
    let ms = now_ms.saturating_sub(then_ms);
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    format!("{}.{:01}s", ms / 1_000, (ms % 1_000) / 100)
}

fn serve(options: &ServeOptions) -> Result<(), String> {
    let address: SocketAddr = options
        .listen
        .parse()
        .map_err(|error| format!("`--listen {}` is not an address: {error}", options.listen))?;

    let peers = parse_peers(&options.peers)?;
    let members = if peers.is_empty() {
        MemberList::alone(options.id)
    } else {
        MemberList::new(peers).map_err(|error| format!("`--peers`: {error}"))?
    };
    if !members.contains(options.id) {
        return Err(format!(
            "`--id {}` is not in `--peers {}`; every member is given the same list, itself \
             included",
            options.id, options.peers
        ));
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("building the runtime: {error}"))?;

    // Inside the runtime, because the transport's delivery tasks live on it — and before
    // `Pd::open`, because the driver it starts sends its first vote the moment it ticks.
    let transport = runtime.block_on(async {
        PdTcpTransport::spawn(options.id, &members, TransportConfig::new())
            .map_err(|error| format!("connecting to the group: {error}"))
    })?;

    let alone = members.is_alone();
    let pd = Pd::open(
        &options.data_dir,
        PdOptions {
            id: options.id,
            members,
            transport: Some(transport),
            ..PdOptions::new()
        },
    )
    .map_err(|error| format!("opening {}: {error}", options.data_dir.display()))?;

    runtime.block_on(async move {
        // Time enters here and nowhere else; a group of one gets none and needs none.
        let _ticker = PdService::spawn_ticker(&pd, esker_pd::service::TICK);
        let server = Server::bind(
            address,
            PdService::new(Arc::clone(&pd)),
            TransportConfig::new(),
        )
        .await
        .map_err(|error| format!("listening on {address}: {error}"))?;
        let bound = server
            .local_addr()
            .map_err(|error| format!("reading the bound address: {error}"))?;

        if !alone {
            let membership = pd.membership();
            println!(
                "esker pd: member {} of group {:#018x} ({} members)",
                membership.this_id,
                membership.group_id,
                membership.members.len()
            );
        }
        match pd.cluster() {
            Ok(Some(cluster)) => println!(
                "esker pd: cluster {} listening on {bound}, data in {}",
                cluster.cluster_id,
                options.data_dir.display()
            ),
            Ok(None) => println!(
                "esker pd: listening on {bound}, data in {} — no cluster yet, waiting for a \
                 store to bootstrap one",
                options.data_dir.display()
            ),
            Err(error) => return Err(format!("reading the cluster record: {error}")),
        }

        server
            .serve(shutdown_signal())
            .await
            .map_err(|error| format!("serving: {error}"))?;
        println!("esker pd: stopped");
        Ok(())
    })
}

/// Resolves on the first ctrl-C.
async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => println!("esker pd: shutting down, finishing in-flight requests"),
        Err(error) => eprintln!("esker pd: cannot listen for ctrl-c ({error}); stopping"),
    }
}

/// Prints what PD has asked the cluster to do.
///
/// The in-flight set is memory and is gone with the process
/// (`docs/adr/0013-repair-operators-are-requests-not-commands.md`), so this ring is the only
/// thing that can answer "why is my cluster shaped like this" after the fact.
fn print_history(pd: &PdInspector, out: &mut impl std::io::Write) -> Result<(), String> {
    let write = |error: std::io::Error| format!("writing: {error}");
    let history = pd.history().map_err(|error| error.to_string())?;
    writeln!(out, "\noperator history ({})", history.len()).map_err(write)?;
    for event in &history {
        writeln!(
            out,
            "  {:>14} ms  region {:<5} {:<15} {:<10} store {:<4} peer {}",
            event.at_ms,
            event.region_id,
            event.kind.name(),
            event.outcome.name(),
            event.store_id,
            event.peer_id,
        )
        .map_err(write)?;
    }
    Ok(())
}

/// Prints PD's whole state: the cluster, the allocator, the oracle's mark, every store and
/// every region.
///
/// It opens the database, so it is for a PD that is **stopped** — the same rule the other
/// `dump` commands follow.
pub(crate) fn inspect(
    options: &InspectOptions,
    out: &mut impl std::io::Write,
) -> Result<(), String> {
    // A **read-only** view, and its own type for a reason worth knowing: opening a placement
    // driver now campaigns, and a campaign is a write ([`esker_pd::inspect`]). An inspector must
    // not create or move what it was asked to look at — and a typo in a path should be an error,
    // not an empty database that reads like a wiped cluster.
    let pd = PdInspector::open(&options.data_dir, esker_engine::Options::default())
        .map_err(|error| format!("opening {}: {error}", options.data_dir.display()))?;

    let write = |error: std::io::Error| format!("writing: {error}");

    match pd.cluster().map_err(|error| error.to_string())? {
        Some(cluster) => {
            writeln!(out, "cluster        {}", cluster.cluster_id).map_err(write)?;
            writeln!(out, "first region   {}", cluster.first_region_id).map_err(write)?;
            writeln!(out, "created (ms)   {}", cluster.created_ms).map_err(write)?;
        }
        None => writeln!(out, "cluster        (not bootstrapped)").map_err(write)?,
    }
    writeln!(
        out,
        "tso mark (ms)  {}",
        pd.tso_high_water_ms().map_err(|error| error.to_string())?
    )
    .map_err(write)?;
    writeln!(
        out,
        "id reserved    {}",
        pd.allocated_end().map_err(|error| error.to_string())?
    )
    .map_err(write)?;
    // The durable half of consensus, and only that. Who leads *now* is a live fact that dies with
    // the process, and `esker pd members` is what asks a running group for it.
    match pd.raft() {
        Some(raft) => {
            writeln!(
                out,
                "raft           term {}  voted {}  commit {}  applied {}  log begins after {}",
                raft.hard_state.term,
                raft.hard_state
                    .voted_for
                    .map_or_else(|| "-".to_owned(), |id| id.to_string()),
                raft.hard_state.commit,
                raft.applied_index,
                raft.truncated_index,
            )
            .map_err(write)?;
            writeln!(
                out,
                "members        {}",
                if raft.conf_state.voters.is_empty() {
                    "(none recorded)".to_owned()
                } else {
                    raft.conf_state
                        .voters
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )
            .map_err(write)?;
        }
        None => writeln!(out, "raft           (no consensus state on disk)").map_err(write)?,
    }

    let stores = pd.stores().map_err(|error| error.to_string())?;
    writeln!(out, "\nstores ({})", stores.len()).map_err(write)?;
    for store in &stores {
        writeln!(
            out,
            "  {:>4}  {:<24} last beat {} ms  regions {}  leaders {}  {}/{} bytes free",
            store.store_id,
            store.address,
            store.last_heartbeat_ms,
            store.stats.region_count,
            store.stats.leader_count,
            store.stats.available,
            store.stats.capacity,
        )
        .map_err(write)?;
    }

    let regions = pd.regions().map_err(|error| error.to_string())?;
    writeln!(out, "\nregions ({})", regions.len()).map_err(write)?;
    for region in &regions {
        writeln!(
            out,
            "  {:>4}  [{}, {})  epoch ({}, {})  leader {}  term {}  ~{} bytes  applied {}",
            region.region.id,
            crate::bytes::escape(&region.region.start_key),
            if region.region.end_key.is_empty() {
                "+inf".to_owned()
            } else {
                crate::bytes::escape(&region.region.end_key)
            },
            region.region.epoch.conf_ver,
            region.region.epoch.version,
            region.leader_peer_id,
            region.term,
            region.approximate_size,
            region.applied_index,
        )
        .map_err(write)?;
        for peer in &region.region.peers {
            writeln!(
                out,
                "        peer {} on store {} ({:?})",
                peer.peer_id, peer.store_id, peer.role
            )
            .map_err(write)?;
        }
    }

    print_history(&pd, out)?;

    // The index is what a lookup actually walks, so a disagreement between it and the records
    // is the failure that would make routing wrong while everything above still looked right.
    let index = esker_pd::routing::range_index(pd.db()).map_err(|error| error.to_string())?;
    writeln!(out, "\nrange index ({})", index.len()).map_err(write)?;
    for (end, region_id) in &index {
        writeln!(
            out,
            "  {:<20} -> region {region_id}",
            if end.is_empty() {
                "+inf".to_owned()
            } else {
                crate::bytes::escape(end)
            },
        )
        .map_err(write)?;
    }
    if index.len() != regions.len() {
        writeln!(
            out,
            "\nWARNING: {} regions and {} index entries — the routing table is inconsistent",
            regions.len(),
            index.len()
        )
        .map_err(write)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LISTEN, InspectOptions, ServeOptions, inspect};

    #[test]
    fn the_defaults_are_usable() {
        let options = ServeOptions::default();
        assert_eq!(options.listen, DEFAULT_LISTEN);
        assert!(DEFAULT_LISTEN.parse::<std::net::SocketAddr>().is_ok());
        assert!(!options.data_dir.as_os_str().is_empty());
    }

    /// An inspector that created the database it was asked to read would turn a typo into a
    /// report of an empty cluster, which is the most misleading answer available.
    #[test]
    fn inspecting_a_directory_that_is_not_there_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let options = InspectOptions {
            data_dir: dir.path().join("nothing-here"),
        };
        let mut out = Vec::new();
        assert!(inspect(&options, &mut out).is_err());
        assert!(!dir.path().join("nothing-here").exists());
    }

    #[test]
    fn inspect_prints_the_cluster_the_regions_and_the_index() {
        let dir = tempfile::tempdir().unwrap();
        {
            let pd = esker_pd::Pd::open(dir.path(), esker_pd::PdOptions::new()).unwrap();
            pd.bootstrap(1, "127.0.0.1:20160").unwrap();
        }
        let mut out = Vec::new();
        inspect(
            &InspectOptions {
                data_dir: dir.path().to_path_buf(),
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("first region   1"), "{text}");
        assert!(text.contains("stores (1)"), "{text}");
        assert!(text.contains("127.0.0.1:20160"), "{text}");
        assert!(text.contains("regions (1)"), "{text}");
        assert!(text.contains("[, +inf)"), "the unbounded region: {text}");
        assert!(text.contains("range index (1)"), "{text}");
        assert!(text.contains("operator history (0)"), "{text}");
        assert!(!text.contains("WARNING"), "{text}");
    }

    /// The history is the whole reason `pd inspect` is useful after a repair: the in-flight set
    /// is memory and is gone with the process, so without this a stopped PD could not say what
    /// it had asked the cluster to do.
    #[test]
    fn inspect_prints_what_pd_asked_the_cluster_to_do() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        {
            // A clock driven by hand, so "store 3 has been quiet for a minute" is a fact rather
            // than a race with the test's own scheduling.
            let clock = Arc::new(esker_pd::clock::TestClock::new(1_700_000_000_000));
            let pd = esker_pd::Pd::open(
                dir.path(),
                esker_pd::PdOptions::with_clock(Arc::clone(&clock) as Arc<dyn esker_pd::Clock>),
            )
            .unwrap();
            for store_id in 1..=4 {
                pd.bootstrap(store_id, &format!("127.0.0.1:{store_id}"))
                    .unwrap();
            }
            let region = esker_proto::Region {
                id: 1,
                start_key: bytes::Bytes::new(),
                end_key: bytes::Bytes::new(),
                peers: vec![
                    esker_proto::Peer::voter(1, 10),
                    esker_proto::Peer::voter(2, 20),
                    esker_proto::Peer::voter(3, 30),
                ],
                epoch: esker_proto::Epoch::new(1, 1),
            };
            let beat = |region: esker_proto::Region| esker_pd::RegionBeat {
                region,
                leader_peer_id: 10,
                term: 4,
                approximate_size: 0,
                applied_index: 0,
            };
            assert_eq!(
                pd.region_heartbeat(&beat(region.clone())).unwrap().operator,
                None,
                "nothing is wrong yet"
            );
            assert!(pd.history().unwrap().is_empty());

            // Store 3 goes quiet; the others keep beating.
            clock.advance(esker_pd::pd::MAX_STORE_DOWN_TIME_MS + 1);
            for store_id in [1, 2, 4] {
                pd.store_heartbeat(&esker_pd::StoreBeat {
                    store_id,
                    stats: esker_pd::StoreStats::default(),
                })
                .unwrap();
            }
            assert!(
                pd.region_heartbeat(&beat(region))
                    .unwrap()
                    .operator
                    .is_some(),
                "store 3 is down and the region is short a replica"
            );
        }

        let mut out = Vec::new();
        inspect(
            &InspectOptions {
                data_dir: dir.path().to_path_buf(),
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("operator history (1)"), "{text}");
        assert!(text.contains("AddPeer"), "{text}");
        assert!(text.contains("issued"), "{text}");
    }

    /// A PD nothing has bootstrapped says so rather than printing a cluster id of zero.
    #[test]
    fn inspect_says_when_there_is_no_cluster() {
        let dir = tempfile::tempdir().unwrap();
        drop(esker_pd::Pd::open(dir.path(), esker_pd::PdOptions::new()).unwrap());
        let mut out = Vec::new();
        inspect(
            &InspectOptions {
                data_dir: dir.path().to_path_buf(),
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("(not bootstrapped)"), "{text}");
        assert!(text.contains("regions (0)"), "{text}");
    }

    /// The report an operator reads, including the shape of an age.
    #[test]
    fn status_prints_every_operator_with_its_progress_and_age() {
        use esker_proto::{Epoch, Operator, OperatorProgress, OperatorStatus};

        let now = 1_700_000_000_000_u64;
        let operators = vec![
            OperatorStatus {
                operator: Operator::AddPeer {
                    region_id: 7,
                    epoch: Epoch::new(2, 3),
                    store_id: 4,
                    peer_id: 5,
                },
                progress: OperatorProgress::Issued,
                issued_ms: now - 12_400,
                since_ms: now - 12_400,
                sends: 9,
            },
            OperatorStatus {
                operator: Operator::RemovePeer {
                    region_id: 8,
                    epoch: Epoch::new(5, 1),
                    peer_id: 6,
                },
                progress: OperatorProgress::Started,
                issued_ms: now - 60_000,
                since_ms: now - 300,
                sends: 1,
            },
        ];

        let mut out = Vec::new();
        super::print_status(now, &operators, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("operators in flight (2)"), "{text}");
        assert!(text.contains("region 7"), "{text}");
        assert!(text.contains("AddPeer"), "{text}");
        assert!(text.contains("issued"), "{text}");
        assert!(text.contains("epoch (2,3)"), "{text}");
        assert!(text.contains("age 12.4s"), "{text}");
        assert!(text.contains("sends 9"), "{text}");
        // The second line is where the two clocks differ, which is the whole reason `since_ms` is
        // reported beside `issued_ms`: a minute old and moving is not a minute old and stuck.
        assert!(text.contains("RemovePeer"), "{text}");
        assert!(text.contains("started"), "{text}");
        assert!(
            text.contains("age 60.0s  since 300ms"),
            "an operator that is old and moving reads as stuck:\n{text}"
        );
    }

    /// Nothing in flight is said out loud, because an empty report and a broken one look the same.
    #[test]
    fn status_says_when_there_is_nothing_in_flight() {
        let mut out = Vec::new();
        super::print_status(1, &[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("operators in flight (0)"), "{text}");
        assert!(text.contains("PD is not moving anything"), "{text}");
    }

    /// A clock that went backwards must not read as a nineteen-billion-second-old operator.
    #[test]
    fn an_age_from_a_future_timestamp_saturates_rather_than_wrapping() {
        assert_eq!(super::age(1_000, 5_000), "0ms");
        assert_eq!(super::age(5_000, 1_000), "4.0s");
        assert_eq!(super::age(1_500, 1_000), "500ms");
    }
}
