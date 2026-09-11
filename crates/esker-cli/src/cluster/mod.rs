//! `esker cluster start` / `esker cluster stop` — three stores on localhost, one region.
//!
//! This is a development and testing command, not an operator tool. Without `--pd` the membership
//! is fixed at start and every store is told the whole peer list on its command line; with it,
//! this also starts a placement driver — or `--pd-nodes N` of them, founding one group — points
//! every store at all of them, and prints the addresses a SQL node should be given, which is what
//! makes the phase-8 story runnable in one command (`docs/bench/columnar-learner.md`).
//!
//! # The state file's id column, and why it did not change
//!
//! A driver's line has id **zero**, which is not a store id anywhere in this codebase. With
//! `--pd-nodes 3` there are three such lines, so zero means *a* driver rather than *the* driver and
//! the address is what tells them apart
//! ([ADR 0108](../../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)).
//! That was chosen against a self-describing format on purpose: `esker durability chaos` skips
//! every `id == "0"` line rather than the first, and `esker-rails-harness/leader-kill.py` matches a
//! census `store=N` against `id == N`, so both keep working without being edited.
//!
//! # Why child processes
//!
//! Three stores in one process would be simpler and would prove less. The point of running them
//! separately is that one can be killed — `SIGKILL`, not a shutdown — while the other two carry
//! on, which is the only way to test that an acknowledged write survives losing the leader. The
//! phase-3 acceptance battery does exactly that (`prompts/03-raft.md`).
//!
//! # The state file
//!
//! `start` writes one line per node into `<data-dir>/cluster.state` — `id address pid` — so that
//! `stop`, run from another shell, can find them. It is a text format read by one function, so it
//! is written by hand like every other format in this project; there is no `serde` (`CLAUDE.md`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process};
use std::time::{Duration, Instant};

/// The lowest port a cluster uses; node `i` listens on `base + i - 1`.
pub(crate) const DEFAULT_BASE_PORT: u16 = 20_160;

/// Where `start` records what it launched.
mod probes;

use probes::{
    PD_START_TIMEOUT, STORE_START_TIMEOUT, wait_until_the_driver_answers,
    wait_until_the_stores_answer,
};

const STATE_FILE: &str = "cluster.state";

/// What `esker cluster` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClusterOptions {
    /// Launch a cluster and supervise it until ctrl-C.
    Start {
        /// How many stores.
        nodes: u64,
        /// The directory holding every node's data and the state file.
        data_dir: PathBuf,
        /// The port node 1 listens on.
        base_port: u16,
        /// Seed for the election-timeout RNG, shared by every node.
        seed: u64,
        /// Tier every node's SSTs into `s3://bucket/prefix`.
        ///
        /// Each node gets `prefix/node-N`, derived rather than configured: a cluster is
        /// several databases, and two sharing a prefix would overwrite each other's
        /// `000007.sst` (`crate::sst_store`).
        sst_store: Option<String>,
        /// Memtable bytes before a flush, for every node. `None` is the engine's default.
        write_buffer_size: Option<usize>,
        /// How many placement drivers to start, when `--pd` asks for any.
        ///
        /// One by default, which is every cluster this command has ever produced. Three is the
        /// number that survives losing one
        /// ([ADR 0108](../../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)):
        /// they found one group between them, elect among themselves, and every store and the
        /// printed SQL command line are given all of their addresses.
        pd_nodes: u64,
        /// Also start a placement driver, and point every node at it.
        ///
        /// A switch rather than an address: they listen on the ports **above** the nodes', so they
        /// cannot collide with one, and `start` prints where they are. A cluster this command
        /// starts is one it also has to be able to stop, and an address it was handed might belong
        /// to somebody else's driver.
        pd: bool,
        /// **Do not restart a store that exits.**
        ///
        /// The default is to restart it, because a supervisor that only reports a death leaves a
        /// chaos run with nothing to wait for: `esker durability chaos` kills, waits for the
        /// cluster to serve again, and kills again, and without a restart it gets exactly one
        /// kill out of a window (run 118 and 119). The switch keeps the old behaviour for anyone
        /// who wants a death to stay a death — watching a cluster degrade, or a test that asserts
        /// a node stayed down.
        no_respawn: bool,
        /// Log what each region's peer believes every N milliseconds, on every node.
        ///
        /// Off by default, and a diagnostic: nothing reads it. Passed straight through to
        /// `esker server --region-census-ms` (`esker_store::census`).
        region_census_ms: Option<u64>,
    },
    /// Stop a cluster `start` launched.
    Stop {
        /// The directory holding the state file.
        data_dir: PathBuf,
    },
}

/// One node's identity, as the state file records it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    id: u64,
    address: String,
    pid: u32,
}

/// One child this command started and watches.
///
/// **`id` is the id the state file carries**: a store's node id, or **zero** for a placement
/// driver. A cluster may now have several drivers ([ADR 0108](../../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)),
/// so zero means *a* driver rather than *the* driver and the **address** is what tells two of them
/// apart — which is also what `restart_command` needs, so it lives here rather than being derived
/// from a single `Option<&str>` the way it was when there could only be one.
///
/// Every reader of the state file keeps working because of that choice: `esker durability chaos`
/// skips every `id == "0"` line rather than the first, and `esker-rails-harness/leader-kill.py`
/// matches a census `store=N` against `id == N` and so never sees a driver at all.
struct Supervised {
    id: u64,
    address: String,
    child: Child,
}

/// Runs the command.
pub(crate) fn run(options: &ClusterOptions) -> Result<(), String> {
    match options {
        ClusterOptions::Start {
            nodes,
            data_dir,
            base_port,
            seed,
            sst_store,
            write_buffer_size,
            pd,
            pd_nodes,
            no_respawn,
            region_census_ms,
        } => start(
            *nodes,
            data_dir,
            *base_port,
            *seed,
            &Tuning {
                sst_store: sst_store.as_deref(),
                write_buffer_size: *write_buffer_size,
                region_census_ms: *region_census_ms,
            },
            Supervision {
                pd: *pd,
                pd_nodes: *pd_nodes,
                respawn: !*no_respawn,
            },
        ),
        ClusterOptions::Stop { data_dir } => stop(data_dir),
    }
}

/// The address node `id` listens on.
fn address_of(base_port: u16, id: u64) -> String {
    format!("127.0.0.1:{}", u64::from(base_port) + id - 1)
}

/// The directory node `id` keeps its database in.
fn dir_of(data_dir: &Path, id: u64) -> PathBuf {
    data_dir.join(format!("node-{id}"))
}

/// What `start` prints once everything is up.
///
/// The SQL node's command line is printed rather than left to be worked out: it needs both the
/// store addresses and the driver's, and the whole point of `--pd` is that this is the command
/// that follows.
fn announce(nodes: u64, base_port: u16, launched: &[Node], pd: &[String]) {
    println!("esker cluster: {nodes} nodes started");
    let mut member = 0;
    for node in launched {
        match node.id {
            0 => {
                member += 1;
                println!(
                    "  placement driver {member} on {} (pid {})",
                    node.address, node.pid
                );
            }
            id => println!("  node {id} on {} (pid {})", node.address, node.pid),
        }
    }
    if !pd.is_empty() {
        let stores: Vec<String> = (1..=nodes).map(|id| address_of(base_port, id)).collect();
        // **The whole group on the SQL node's command line**, because a node given one member
        // stops serving when that member is the one that dies — which is the thing several
        // drivers exist to prevent.
        println!(
            "esker cluster: a SQL node over this cluster is\n  \
             esker-sql 127.0.0.1:5432 {} --pd {}",
            stores.join(" "),
            pd.join(",")
        );
    }
}

/// Starts the placement driver, and describes it the way the state file records a node.
///
/// Id **zero**, which is not a store id anywhere in this codebase and is therefore an honest way
/// to say "this line is not a store" in a format that has one shape. `stop` kills it like any
/// other line.
/// Starts every placement driver and waits for each to answer, or stops what it started.
///
/// **Every member, and each waited for.** A group of three elects among itself, so the second and
/// third are not optional extras: a store pointed at a list whose members are not all listening can
/// be redirected to one that is not, and spend its redirect budget on it.
fn start_drivers(
    binary: &Path,
    data_dir: &Path,
    group: &[String],
    children: &mut Vec<Supervised>,
    launched: &mut Vec<Node>,
) -> Result<(), String> {
    for (at, address) in group.iter().enumerate() {
        let member = at as u64 + 1;
        let (node, mut child) = start_pd(binary, data_dir, address, member, group)?;
        if let Err(error) = wait_until_the_driver_answers(address, &mut child, PD_START_TIMEOUT) {
            let _ = child.kill();
            // The members already up are stopped too: a half-formed group is one that cannot
            // elect, and leaving it running would look like a cluster.
            for one in children.iter_mut() {
                let _ = one.child.kill();
            }
            return Err(error);
        }
        launched.push(node);
        children.push(Supervised {
            id: 0,
            address: address.clone(),
            child,
        });
    }
    Ok(())
}

fn start_pd(
    binary: &Path,
    data_dir: &Path,
    address: &str,
    member: u64,
    group: &[String],
) -> Result<(Node, Child), String> {
    let child = pd_command(binary, data_dir, address, member, group)
        .spawn()
        .map_err(|error| format!("starting placement driver {member}: {error}"))?;
    Ok((
        Node {
            id: 0,
            address: address.to_owned(),
            pid: child.id(),
        },
        child,
    ))
}

/// The driver's command line, from the data directory and the address.
///
/// **One place, for the reason [`store_command`] is one place**: the driver is started and now
/// also restarted, and a supervisor that rebuilt this in a second place would eventually restart
/// a driver pointed at a different directory than the one it replaced — with the cluster looking
/// healthy while its timestamps came from an empty database.
fn pd_command(
    binary: &Path,
    data_dir: &Path,
    address: &str,
    member: u64,
    group: &[String],
) -> Process {
    let mut process = Process::new(binary);
    process
        .arg("pd")
        .arg("serve")
        .arg("--data-dir")
        // **One directory per member**, derived rather than configured, for the reason
        // `--sst-store` derives one per node: two members sharing a database would each hold the
        // other's Raft log, and the second to start would refuse to open at all.
        .arg(data_dir.join(format!("pd-{member}")))
        .arg("--listen")
        .arg(address)
        .arg("--id")
        .arg(member.to_string());
    if group.len() > 1 {
        // **Founding, not joining.** Every member is handed the same list, so the group id is
        // derived from it once and written down (ADR 0061); `--join` is for adding a member to a
        // group that is already up, which is `esker pd members add`'s business and not this
        // command's. A group of one is left without the flag, which is exactly the command line
        // every existing invocation produced.
        let peers: Vec<String> = group
            .iter()
            .enumerate()
            .map(|(at, listed)| format!("{}@{listed}", at + 1))
            .collect();
        process.arg("--peers").arg(peers.join(","));
    }
    process
}

/// Where placement driver `member` listens when `--pd` is given: the ports above the nodes'.
///
/// One above the last store for the first member, one above that for the second, and so on —
/// extending the rule `--pd` already followed rather than opening a second band an operator would
/// have to know about.
fn pd_address_of(base_port: u16, nodes: u64, member: u64) -> String {
    format!("127.0.0.1:{}", u64::from(base_port) + nodes + member - 1)
}

/// Everything one store's command line is derived from, so that spawning them is one argument
/// rather than eight.
struct Layout<'a> {
    nodes: u64,
    data_dir: &'a Path,
    base_port: u16,
    seed: u64,
    sst_store: Option<&'a str>,
    write_buffer_size: Option<usize>,
    /// How often each region's peer logs what it believes. `None` is off.
    region_census_ms: Option<u64>,
    /// Every placement driver's address, once they are known to be listening. Empty without
    /// `--pd`.
    pd: &'a [String],
    /// `id@address` for every node, which every node is told in full.
    peers: &'a [String],
}

/// One store's command line, from the layout.
///
/// **One place, because a store is restarted as well as started.** A supervisor that rebuilt the
/// arguments in a second place would eventually restart a store that is not the one it replaced —
/// a different `--sst-store` prefix, a forgotten `--pd` — and the cluster would look healthy while
/// one member was quietly a different thing.
fn store_command(binary: &Path, layout: &Layout<'_>, id: u64) -> Result<Process, String> {
    let dir = dir_of(layout.data_dir, id);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("creating {}: {error}", dir.display()))?;

    let mut process = Process::new(binary);
    process
        .arg("server")
        .arg("--data-dir")
        .arg(&dir)
        .arg("--listen")
        .arg(address_of(layout.base_port, id))
        .arg("--store-id")
        .arg(id.to_string())
        .arg("--seed")
        .arg(layout.seed.to_string());
    if let Some(store_url) = layout.sst_store {
        process
            .arg("--sst-store")
            .arg(crate::sst_store::for_node(store_url, id));
    }
    if let Some(size) = layout.write_buffer_size {
        process.arg("--write-buffer-size").arg(size.to_string());
    }
    if let Some(every) = layout.region_census_ms {
        process.arg("--region-census-ms").arg(every.to_string());
    }
    if !layout.pd.is_empty() {
        // The whole group, comma-separated: only its leader answers, and a store given one
        // address cannot outlive that member (`esker_proto::LeaderBook`).
        process.arg("--pd").arg(layout.pd.join(","));
    }
    for peer in layout.peers {
        process.arg("--peer").arg(peer);
    }
    Ok(process)
}

/// Starts one store per node, recording each in `children` and `launched`.
///
/// Stops at the first that will not spawn and leaves the cleanup to the caller, which is holding
/// the ones already running.
fn spawn_stores(
    binary: &Path,
    layout: &Layout<'_>,
    children: &mut Vec<Supervised>,
    launched: &mut Vec<Node>,
) -> Result<(), String> {
    for id in 1..=layout.nodes {
        let child = store_command(binary, layout, id)?
            .spawn()
            .map_err(|error| format!("starting node {id}: {error}"))?;
        let address = address_of(layout.base_port, id);
        launched.push(Node {
            id,
            address: address.clone(),
            pid: child.id(),
        });
        children.push(Supervised { id, address, child });
    }
    Ok(())
}

/// What `start` passes straight through to every store it spawns.
///
/// A struct rather than three more parameters: `start` was already at the seven clippy allows,
/// and these three have one thing in common that the others do not — none of them is the
/// supervisor's business, they are each forwarded verbatim to `esker server`.
struct Tuning<'a> {
    sst_store: Option<&'a str>,
    write_buffer_size: Option<usize>,
    region_census_ms: Option<u64>,
}

fn start(
    nodes: u64,
    data_dir: &Path,
    base_port: u16,
    seed: u64,
    tuning: &Tuning<'_>,
    supervision: Supervision,
) -> Result<(), String> {
    if nodes == 0 {
        return Err("`--nodes` must be at least 1".to_owned());
    }
    if supervision.pd && supervision.pd_nodes == 0 {
        return Err("`--pd-nodes` must be at least 1".to_owned());
    }
    // The same argument as for the stores, and it is the placement driver's own Raft group: two
    // members tolerate no failure at all, because a quorum of two is two.
    if supervision.pd && supervision.pd_nodes.is_multiple_of(2) {
        eprintln!(
            "esker cluster: {} is an even number of placement drivers; it tolerates no more \
             failures than {} and can split evenly",
            supervision.pd_nodes,
            supervision.pd_nodes - 1
        );
    }
    // An even group has no advantage over the odd one below it — four nodes tolerate one
    // failure, exactly as three do — and it makes a split-brain-looking two-two partition
    // possible. Worth saying rather than silently allowing.
    if nodes.is_multiple_of(2) {
        eprintln!(
            "esker cluster: {nodes} is an even number of nodes; it tolerates no more failures \
             than {} and can split evenly",
            nodes - 1
        );
    }

    std::fs::create_dir_all(data_dir)
        .map_err(|error| format!("creating {}: {error}", data_dir.display()))?;

    let peers: Vec<String> = (1..=nodes)
        .map(|id| format!("{id}@{}", address_of(base_port, id)))
        .collect();
    let binary =
        std::env::current_exe().map_err(|error| format!("finding this executable: {error}"))?;

    let mut children: Vec<Supervised> = Vec::new();
    let mut launched: Vec<Node> = Vec::new();

    // The driver first, because every store below is about to ask it whether to bootstrap. A
    // store whose PD is not up yet fails to open, which is the behaviour that makes a cluster's
    // start order matter here and nowhere else.
    //
    // **And started is not listening.** Spawning the stores straight after this returns is a race
    // the stores lose: on a real run three of four exited with `connecting to 127.0.0.1:21264:
    // Connection refused` before the driver had printed its own "listening" line, leaving one
    // store that registered, bootstrapped a region alone, and looked perfectly healthy
    // (`docs/bench/columnar-learner.md`, "One more thing the real binaries said"). So this waits
    // until the driver answers a driver's question, and fails loudly if it never does —
    // *answers*, not *accepts*: `wait_until_the_driver_answers` says what a bare connect cost.
    let pd: Vec<String> = supervision
        .pd
        .then(|| (1..=supervision.pd_nodes).map(|m| pd_address_of(base_port, nodes, m)))
        .into_iter()
        .flatten()
        .collect();
    start_drivers(&binary, data_dir, &pd, &mut children, &mut launched)?;

    // Named once and used twice: `spawn_stores` starts them and the supervisor restarts them, and
    // a store is whatever this says it is.
    let layout = Layout {
        nodes,
        data_dir,
        base_port,
        seed,
        sst_store: tuning.sst_store,
        write_buffer_size: tuning.write_buffer_size,
        region_census_ms: tuning.region_census_ms,
        pd: &pd,
        peers: &peers,
    };
    if let Err(error) = spawn_stores(&binary, &layout, &mut children, &mut launched) {
        // A node that will not start leaves the cluster short of a quorum, so the ones already
        // running are stopped rather than left half-formed.
        for mut one in children {
            let _ = one.child.kill();
        }
        return Err(error);
    }

    // Every store is **serving** before the cluster is announced. The announcement's list of pids
    // is otherwise the only thing anyone sees: the children inherit this process's stdio, and this
    // process then blocks in `wait_for_interrupt`, so their own error lines are not flushed until
    // the whole thing is stopped. A cluster that is a quarter of one must not be reported as
    // started — and "no child has died yet" was not enough to know that it is not.
    if let Err(error) = wait_until_the_stores_answer(&mut children, &launched, STORE_START_TIMEOUT)
    {
        for mut one in children {
            let _ = one.child.kill();
        }
        return Err(error);
    }

    write_state(data_dir, &launched)?;
    announce(nodes, base_port, &launched, &pd);
    println!(
        "esker cluster: ctrl-C to stop, or `esker cluster stop --data-dir {}`",
        data_dir.display()
    );

    // Supervise until ctrl-C, naming any child that dies on the way — and, unless `--no-respawn`
    // says otherwise, starting it again. Killing one node while the others carry on is what this
    // command is for; bringing it back is what makes a *repeated* kill a test of recovery rather
    // than of attrition, which is what a chaos run needs (`esker durability chaos`).
    wait_for_interrupt(
        &mut children,
        &binary,
        &layout,
        &mut launched,
        supervision.respawn,
    );
    println!("esker cluster: stopping");
    for one in &mut children {
        signal(one.child.id(), "TERM");
        let what = what(one.id, &one.address);
        match one.child.wait() {
            Ok(status) => println!("  {what} exited with {status}"),
            Err(error) => eprintln!("  {what}: {error}"),
        }
    }
    let _ = std::fs::remove_file(data_dir.join(STATE_FILE));
    Ok(())
}

fn stop(data_dir: &Path) -> Result<(), String> {
    let nodes = read_state(data_dir)?;
    if nodes.is_empty() {
        return Err(format!(
            "no cluster recorded in {}",
            data_dir.join(STATE_FILE).display()
        ));
    }
    for node in &nodes {
        println!(
            "esker cluster: stopping {} (pid {})",
            what(node.id, &node.address),
            node.pid
        );
        signal(node.pid, "TERM");
    }
    std::fs::remove_file(data_dir.join(STATE_FILE))
        .map_err(|error| format!("removing the state file: {error}"))?;
    Ok(())
}

/// Sends `signal` to `pid`.
///
/// Shelling out to `kill` rather than calling `libc::kill`, because `libc` is a `*-sys`-shaped
/// dependency this project does not take (`CLAUDE.md`, "Dependency policy") and a development
/// command is not worth an exception. Nothing on a hot path goes through here.
fn signal(pid: u32, name: &str) {
    let _ = Process::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status();
}

/// How often the supervisor looks at its children while waiting for ctrl-C.
/// What the supervisor does besides start things: whether it also runs a driver, and whether it
/// puts a store back when one exits. Two booleans in one argument, because a call with eight
/// positional parameters is a call whose fifth and sixth get swapped one day.
#[derive(Debug, Clone, Copy)]
struct Supervision {
    /// Start a placement driver beside the stores and point every node at it.
    pd: bool,
    /// How many of them, when `pd` is set. One is a single point of failure and the default.
    pd_nodes: u64,
    /// Restart a store that exits.
    respawn: bool,
}

const SUPERVISE_TICK: Duration = Duration::from_millis(250);

/// How long after a store exits before it is started again, doubling on each consecutive failure.
///
/// Short, because the point of restarting is that a chaos run has something to wait for: a kill
/// that took thirty seconds to undo would make every later kill land on a cluster still recovering
/// from the last, and the run would measure the backoff rather than the cluster.
const RESPAWN_BACKOFF: Duration = Duration::from_millis(500);

/// The ceiling that doubling reaches. A store that cannot open its data directory is not going to
/// start on the ninth attempt either, and a supervisor spinning on it drowns its own log.
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(8);

/// `node N` or `the placement driver on ADDR`, for a message an operator reads.
///
/// The driver's address is in the sentence because a cluster may have several, and "the placement
/// driver exited" in a log with three of them is a line that names nothing.
fn what(id: u64, address: &str) -> String {
    if id == 0 {
        format!("the placement driver on {address}")
    } else {
        format!("node {id}")
    }
}

/// The command that starts child `id` again — the driver's, or a store's.
///
/// # Why the driver is restarted at all
///
/// It was not, and the comment that said so gave a reason about experiments rather than about
/// safety: *"a cluster whose driver is gone has lost the thing that hands out timestamps"*. That
/// is true and it is an argument for bringing it **back**, not for leaving it down. The exposure
/// is total while it is down — no node can start a transaction, so no statement runs at all,
/// reads included, and every session sees `08006` (`esker-coord/h1-driver-kill.md` §2).
///
/// And a restart cannot repeat a timestamp. The oracle fsyncs its high-water mark `save_interval`
/// **ahead** of every timestamp it hands out and resumes at `max(clock, mark)`
/// (`esker-pd/src/tso.rs`), so the whole cost of the restart is the window in which nothing could
/// `BEGIN`.
///
/// **What this buys is the availability of recovery, and not availability.** A single driver is
/// still a single point: it is replicated in the algorithm ([ADR 0059](../../../../docs/adr/0059-pd-is-a-raft-group.md),
/// [ADR 0061](../../../../docs/adr/0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md))
/// and singular in every deployment this command can produce. Restarting it turns "down until an
/// operator notices" into "down for one driver start"; it does not make the cluster survive
/// losing it.
///
/// One thing had to be true first, and now is: the directory the new driver opens is the one the
/// old one held. `try_wait` returned a status, so that process is reaped and the kernel has
/// released the claim `Db::open` takes on its data directory (#116) — without which this restart
/// would race the corpse for the database that holds the mark.
fn restart_command(
    binary: &Path,
    layout: &Layout<'_>,
    id: u64,
    address: &str,
) -> Result<Process, String> {
    if id != 0 {
        return store_command(binary, layout, id);
    }
    // **Its own address, not the group's first.** With several drivers, deriving one from the
    // layout would restart every casualty as member one — two processes on one port, one of them
    // opening a database another already holds.
    let member = layout
        .pd
        .iter()
        .position(|listed| listed == address)
        .ok_or_else(|| format!("{address} is not a placement driver this cluster started"))?;
    Ok(pd_command(
        binary,
        layout.data_dir,
        address,
        member as u64 + 1,
        layout.pd,
    ))
}

/// Blocks until ctrl-C, naming any child that exits along the way.
///
/// A child that dies while the cluster is up used to be invisible: this process blocked on a
/// signal and nothing looked at them again until the whole thing was stopped, so a cluster could
/// spend an afternoon as three nodes of four with no line saying so anywhere. It says so now.
///
/// **And it says so and keeps going**, which is not a detail. A node of this cluster being killed
/// — `SIGKILL`, not a shutdown — while the others carry on is the entire reason the command runs
/// child processes at all (this module's header), and it is what `esker-cli`'s chaos battery does
/// fifty times in a row. A supervisor that stopped the cluster at the first death would make that
/// battery test its own teardown; it did, for one commit, and the acceptance run hung on stores
/// that had been taken away from it.
///
/// The startup check is the one that fails the command, and it is a different question:
/// [`first_child_that_died`] runs before anything is announced, where a dead child means the
/// cluster never formed rather than that somebody is testing it.
fn wait_for_interrupt(
    children: &mut [Supervised],
    binary: &Path,
    layout: &Layout<'_>,
    nodes: &mut [Node],
    respawn: bool,
) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("esker cluster: cannot listen for ctrl-c; stopping immediately");
        return;
    };
    // One line per child and never more *when nothing is restarted*: a reaped child answers
    // `try_wait` with its status for ever, and a supervisor that said so every tick would bury the
    // log it exists to write. With `respawn` the entry is replaced instead, so the next exit is a
    // new fact and is reported again.
    //
    // **Keyed by position, not by id**, because a cluster may have several placement drivers and
    // every one of them is id zero in the state file. Three children sharing one key would share
    // one backoff and one "already reported" entry between them, so the second driver to die
    // would be silently taken for the first and never restarted.
    let mut reported: Vec<usize> = Vec::new();
    // How long before this child is started again, doubling on each consecutive failure. A store
    // that cannot open its data directory would otherwise be restarted as fast as this loop ticks.
    let mut backoff: std::collections::BTreeMap<usize, Duration> =
        std::collections::BTreeMap::new();
    let mut due: std::collections::BTreeMap<usize, Instant> = std::collections::BTreeMap::new();

    runtime.block_on(async {
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        loop {
            tokio::select! {
                signalled = &mut interrupt => {
                    if let Err(error) = signalled {
                        eprintln!("esker cluster: cannot listen for ctrl-c ({error}); stopping");
                    }
                    return;
                }
                () = tokio::time::sleep(SUPERVISE_TICK) => {
                    let mut exited: Vec<usize> = Vec::new();
                    for (at, one) in children.iter_mut().enumerate() {
                        if reported.contains(&at) {
                            continue;
                        }
                        if let Ok(Some(status)) = one.child.try_wait() {
                            reported.push(at);
                            eprintln!(
                                "esker cluster: {} exited with {status}",
                                what(one.id, &one.address)
                            );
                            if respawn {
                                exited.push(at);
                            }
                        }
                    }
                    for at in exited {
                        let wait = backoff
                            .get(&at)
                            .copied()
                            .unwrap_or(RESPAWN_BACKOFF)
                            .min(RESPAWN_BACKOFF_MAX);
                        due.insert(at, Instant::now() + wait);
                        backoff.insert(at, wait.saturating_mul(2).min(RESPAWN_BACKOFF_MAX));
                    }
                    let ready: Vec<usize> = due
                        .iter()
                        .filter(|(_, at)| Instant::now() >= **at)
                        .map(|(at, _)| *at)
                        .collect();
                    for at in ready {
                        due.remove(&at);
                        let (id, address) = (children[at].id, children[at].address.clone());
                        let name = what(id, &address);
                        match restart_command(binary, layout, id, &address).and_then(
                            |mut command| {
                                command
                                    .spawn()
                                    .map_err(|error| format!("restarting {name}: {error}"))
                            },
                        ) {
                            Ok(child) => {
                                let pid = child.id();
                                eprintln!("esker cluster: {name} restarted as pid {pid}");
                                reported.retain(|seen| *seen != at);
                                children[at].child = child;
                                // **The state file is what a chaos arm reads to find a pid**, and a
                                // restarted store has a new one. A file that still named the dead
                                // pid would send every later kill to a corpse, which is exactly the
                                // shape run 118 spent fourteen shots on.
                                //
                                // Found by **address** rather than by id: every driver's id is
                                // zero, so an id lookup would rewrite the first driver's pid
                                // whichever one came back.
                                if let Some(node) =
                                    nodes.iter_mut().find(|node| node.address == address)
                                {
                                    node.pid = pid;
                                }
                                if let Err(error) = write_state(layout.data_dir, nodes) {
                                    eprintln!("esker cluster: {error}");
                                }
                            }
                            Err(error) => {
                                eprintln!("esker cluster: {error}");
                                due.insert(at, Instant::now() + RESPAWN_BACKOFF_MAX);
                            }
                        }
                    }
                }
            }
        }
    });
}

/// `id address pid`, one node per line.
fn write_state(data_dir: &Path, nodes: &[Node]) -> Result<(), String> {
    let path = data_dir.join(STATE_FILE);
    let mut file = std::fs::File::create(&path)
        .map_err(|error| format!("writing {}: {error}", path.display()))?;
    for node in nodes {
        writeln!(file, "{} {} {}", node.id, node.address, node.pid)
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
    }
    Ok(())
}

fn read_state(data_dir: &Path) -> Result<Vec<Node>, String> {
    let path = data_dir.join(STATE_FILE);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    let mut nodes = Vec::new();
    for (at, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let parsed = (|| {
            Some(Node {
                id: fields.next()?.parse().ok()?,
                address: fields.next()?.to_owned(),
                pid: fields.next()?.parse().ok()?,
            })
        })();
        // A state file this command wrote is well formed; one that is not has been edited or
        // truncated, and guessing at it would stop the wrong process.
        let node = parsed.ok_or_else(|| {
            format!(
                "{}: line {} is not `id address pid`",
                path.display(),
                at + 1
            )
        })?;
        if fields.next().is_some() {
            return Err(format!(
                "{}: line {} has trailing fields",
                path.display(),
                at + 1
            ));
        }
        nodes.push(node);
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{DEFAULT_BASE_PORT, Node, address_of, dir_of, read_state, write_state};

    /// The addresses and directories are derived, not allocated: `esker raw --addr` against node 1
    /// has to be predictable without reading a file.
    #[test]
    fn addresses_and_directories_are_derived_from_the_node_id() {
        assert_eq!(address_of(DEFAULT_BASE_PORT, 1), "127.0.0.1:20160");
        assert_eq!(address_of(DEFAULT_BASE_PORT, 3), "127.0.0.1:20162");
        assert_eq!(dir_of(Path::new("/data"), 2), Path::new("/data/node-2"));
    }

    #[test]
    fn the_state_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let nodes = vec![
            Node {
                id: 1,
                address: "127.0.0.1:20160".to_owned(),
                pid: 111,
            },
            Node {
                id: 2,
                address: "127.0.0.1:20161".to_owned(),
                pid: 222,
            },
        ];
        write_state(dir.path(), &nodes).unwrap();
        assert_eq!(read_state(dir.path()).unwrap(), nodes);
    }

    /// A state file that has been edited or truncated is refused rather than guessed at: acting on
    /// half of one would stop the wrong process.
    #[test]
    fn a_malformed_state_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "1 127.0.0.1:20160",
            "1 addr notapid",
            "1 addr 5 extra",
            "x y z",
        ] {
            std::fs::write(dir.path().join("cluster.state"), bad).unwrap();
            assert!(read_state(dir.path()).is_err(), "accepted `{bad}`");
        }
    }

    #[test]
    fn a_missing_state_file_is_an_error_not_an_empty_cluster() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_state(dir.path()).is_err());
    }

    /// Blank lines are the one thing a hand-edited file gets away with.
    #[test]
    fn blank_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cluster.state"),
            "1 127.0.0.1:20160 111\n\n2 127.0.0.1:20161 222\n",
        )
        .unwrap();
        assert_eq!(read_state(dir.path()).unwrap().len(), 2);
    }
}
