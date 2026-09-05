//! `esker cluster start` / `esker cluster stop` — three stores on localhost, one region.
//!
//! This is a development and testing command, not an operator tool. Without `--pd` the membership
//! is fixed at start and every store is told the whole peer list on its command line; with it,
//! this also starts a placement driver, points every store at it, and prints the address a SQL
//! node should be given — which is what makes the phase-8 story runnable in one command
//! (`docs/bench/columnar-learner.md`).
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
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process};
use std::time::Duration;

use esker_proto::TransportConfig;

/// The lowest port a cluster uses; node `i` listens on `base + i - 1`.
pub(crate) const DEFAULT_BASE_PORT: u16 = 20_160;

/// Where `start` records what it launched.
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
        /// Also start a placement driver, and point every node at it.
        ///
        /// A switch rather than an address: it listens on the port **above** the nodes', so it
        /// cannot collide with one, and `start` prints where it is. A cluster this command starts
        /// is one it also has to be able to stop, and an address it was handed might belong to
        /// somebody else's driver.
        pd: bool,
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
        } => start(
            *nodes,
            data_dir,
            *base_port,
            *seed,
            sst_store.as_deref(),
            *write_buffer_size,
            *pd,
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
fn announce(nodes: u64, base_port: u16, launched: &[Node], pd: Option<&str>) {
    println!("esker cluster: {nodes} nodes started");
    for node in launched {
        match node.id {
            0 => println!("  placement driver on {} (pid {})", node.address, node.pid),
            id => println!("  node {id} on {} (pid {})", node.address, node.pid),
        }
    }
    if let Some(address) = pd {
        let stores: Vec<String> = (1..=nodes).map(|id| address_of(base_port, id)).collect();
        println!(
            "esker cluster: a SQL node over this cluster is\n  \
             esker-sql 127.0.0.1:5432 {} --pd {address}",
            stores.join(" ")
        );
    }
}

/// Starts the placement driver, and describes it the way the state file records a node.
///
/// Id **zero**, which is not a store id anywhere in this codebase and is therefore an honest way
/// to say "this line is not a store" in a format that has one shape. `stop` kills it like any
/// other line.
fn start_pd(binary: &Path, data_dir: &Path, address: &str) -> Result<(Node, Child), String> {
    let mut process = Process::new(binary);
    process
        .arg("pd")
        .arg("serve")
        .arg("--data-dir")
        .arg(data_dir.join("pd"))
        .arg("--listen")
        .arg(address);
    let child = process
        .spawn()
        .map_err(|error| format!("starting the placement driver: {error}"))?;
    Ok((
        Node {
            id: 0,
            address: address.to_owned(),
            pid: child.id(),
        },
        child,
    ))
}

/// Where the placement driver listens when `--pd` is given: one above the last node.
fn pd_address_of(base_port: u16, nodes: u64) -> String {
    format!("127.0.0.1:{}", u64::from(base_port) + nodes)
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
    /// The driver's address, once it is known to be listening.
    pd: Option<&'a str>,
    /// `id@address` for every node, which every node is told in full.
    peers: &'a [String],
}

/// Starts one store per node, recording each in `children` and `launched`.
///
/// Stops at the first that will not spawn and leaves the cleanup to the caller, which is holding
/// the ones already running.
fn spawn_stores(
    binary: &Path,
    layout: &Layout<'_>,
    children: &mut Vec<(u64, Child)>,
    launched: &mut Vec<Node>,
) -> Result<(), String> {
    for id in 1..=layout.nodes {
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
        if let Some(address) = layout.pd {
            process.arg("--pd").arg(address);
        }
        for peer in layout.peers {
            process.arg("--peer").arg(peer);
        }

        let child = process
            .spawn()
            .map_err(|error| format!("starting node {id}: {error}"))?;
        launched.push(Node {
            id,
            address: address_of(layout.base_port, id),
            pid: child.id(),
        });
        children.push((id, child));
    }
    Ok(())
}

fn start(
    nodes: u64,
    data_dir: &Path,
    base_port: u16,
    seed: u64,
    sst_store: Option<&str>,
    write_buffer_size: Option<usize>,
    with_pd: bool,
) -> Result<(), String> {
    if nodes == 0 {
        return Err("`--nodes` must be at least 1".to_owned());
    }
    // An even group has no advantage over the odd one below it — four nodes tolerate one
    // failure, exactly as three do — and it makes a split-brain-looking two-two partition
    // possible. Worth saying rather than silently allowing.
    if nodes % 2 == 0 {
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

    let mut children: Vec<(u64, Child)> = Vec::new();
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
    let pd = with_pd.then(|| pd_address_of(base_port, nodes));
    if let Some(address) = &pd {
        let (node, mut child) = start_pd(&binary, data_dir, address)?;
        if let Err(error) = wait_until_the_driver_answers(address, &mut child, PD_START_TIMEOUT) {
            let _ = child.kill();
            return Err(error);
        }
        launched.push(node);
        children.push((0, child));
    }

    if let Err(error) = spawn_stores(
        &binary,
        &Layout {
            nodes,
            data_dir,
            base_port,
            seed,
            sst_store,
            write_buffer_size,
            pd: pd.as_deref(),
            peers: &peers,
        },
        &mut children,
        &mut launched,
    ) {
        // A node that will not start leaves the cluster short of a quorum, so the ones already
        // running are stopped rather than left half-formed.
        for (_, mut child) in children {
            let _ = child.kill();
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
        for (_, mut child) in children {
            let _ = child.kill();
        }
        return Err(error);
    }

    write_state(data_dir, &launched)?;
    announce(nodes, base_port, &launched, pd.as_deref());
    println!(
        "esker cluster: ctrl-C to stop, or `esker cluster stop --data-dir {}`",
        data_dir.display()
    );

    // Supervise until ctrl-C, naming any child that dies on the way but leaving the rest running:
    // killing one node while the others carry on is what this command is for.
    wait_for_interrupt(&mut children);
    println!("esker cluster: stopping");
    for (id, child) in &mut children {
        signal(child.id(), "TERM");
        let what = what(*id);
        match child.wait() {
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
        match node.id {
            0 => println!(
                "esker cluster: stopping the placement driver (pid {})",
                node.pid
            ),
            id => println!("esker cluster: stopping node {id} (pid {})", node.pid),
        }
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

/// How long a placement driver has to start listening before this gives up on it.
///
/// Generous: it opens a database first. Short enough that a driver which is never going to bind —
/// a port already taken, a directory it cannot write — is an error rather than a hang.
const PD_START_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one readiness probe waits for an answer before it is retried.
///
/// It bounds a probe against a socket that accepts and then says nothing, which is the case the
/// probe exists for; the budgets that decide whether a process started are [`PD_START_TIMEOUT`]
/// and [`STORE_START_TIMEOUT`].
///
/// **It was 500 ms, and at 500 ms it decided instead of bounding.** A driver that is up and
/// healthy still has to be scheduled to answer, and on a box under real load that takes longer
/// than half a second — so every probe timed out, the loop never saw an answer, and the start
/// failed at [`PD_START_TIMEOUT`] blaming a driver that was fine. `esker-cli::cluster_start` went
/// red on the ci-tree that way on 2026-09-04 while passing 3/3 alone.
///
/// **Raising it costs nothing on the path that matters.** A process that has not bound yet
/// refuses the *connect*, which returns at once whatever this says; this value is only ever spent
/// on a socket that accepted and then went quiet, which is exactly what it is for. Five seconds
/// still leaves four probes inside the driver's budget and twelve inside the stores', and neither
/// of those two budgets changed — the verdict stays theirs, which is what the paragraph above
/// always claimed and what 500 ms quietly took away.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Between two readiness probes. A round trip is the cost, so this polls rather than spins.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// How long the stores have to answer before this gives up on them.
///
/// Generous, because a store opens a database and — with `--pd` — registers with the driver
/// before it binds, and both are slower on a cold cache under load. Bounded, because a start
/// that never returns and never says why is worse than one that gives up with a name in it.
const STORE_START_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the supervisor looks at its children while waiting for ctrl-C.
const SUPERVISE_TICK: Duration = Duration::from_millis(250);

/// Waits until `address` answers **as a placement driver**, or says why it never will.
///
/// Asking rather than reading the child's output: the children inherit this process's stdio
/// precisely so that an operator sees their logs, which leaves nothing to parse, and a readiness
/// check that depends on a log line is a check that breaks when the wording does. The child is
/// watched at the same time, so a driver that exits immediately is reported as that rather than
/// as a timeout.
///
/// # A port that answers is not a driver that answers
///
/// This used to poll `TcpStream::connect`, which asks *is anybody listening here* — and anybody
/// is not the driver. Whatever else holds the port satisfies it: a driver left over from an
/// earlier run, another cluster started on the same base port, or this file's own squatter in
/// `cluster_start.rs`'s `a_driver_that_cannot_listen_is_a_failure_and_not_a_cluster`. The driver is then spawned,
/// fails to bind with `Address already in use` and exits — while this check has already answered
/// `Ok` on somebody else's socket and every store has been spawned against it.
///
/// What that cost, measured: at eighty busy threads `cluster start` printed *4 nodes started*,
/// wrote a state file and blocked on a signal for ever, with the driver and all four stores dead
/// or dying behind it. The same command took 0.3 s at forty threads. Nothing about it was slow —
/// what load changed was whether the driver's exit landed inside [`first_child_that_died`]'s
/// window, and a readiness check that could not tell the driver from a squatter is what left that
/// window carrying the decision.
///
/// So the probe is a **round trip only a driver completes**: the wire handshake, then
/// [`esker_proto::PdReq::Status`]. A socket that accepts and never speaks fails the handshake; a
/// *store* on the driver's port passes the handshake and fails the request.
fn wait_until_the_driver_answers(
    address: &str,
    child: &mut Child,
    within: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "the placement driver exited with {status} before it listened on {address}"
            ));
        }
        // The deadline is only ever reached after a probe, so the last refusal always exists and
        // the message can name it: "did not answer" alone would leave an operator guessing which
        // half — the socket, the handshake, or the question — is the one that failed.
        let refusal = match ask_the_driver(address) {
            Ok(()) => return Ok(()),
            Err(why) => why,
        };
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            return Err(format!(
                "the placement driver did not answer on {address} within {within:?}: {refusal}"
            ));
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

/// One round trip that only a placement driver completes, or why it did not.
fn ask_the_driver(address: &str) -> Result<(), String> {
    let socket: SocketAddr = address
        .parse()
        .map_err(|error| format!("`{address}` is not an address: {error}"))?;
    // `request_timeout` bounds the handshake as well as the call, so a socket that accepts and
    // then says nothing costs one probe rather than the transport's default thirty seconds.
    let config = TransportConfig {
        request_timeout: PROBE_TIMEOUT,
        ..TransportConfig::new()
    };
    let pd = crate::region::PdConn::connect_with(socket, config)?;
    match pd.call(&esker_proto::PdReq::Status) {
        Ok(esker_proto::PdResp::Status { .. }) => Ok(()),
        Ok(other) => Err(format!(
            "{address} answered a placement driver's question with {other:?}"
        )),
        Err(error) => Err(format!("asking {address} for its status: {error}")),
    }
}

/// Waits until every launched store answers on its own port, or names the first that will not.
///
/// # "Nothing has failed yet" is not "everything started"
///
/// This replaces a `sleep(250ms)` followed by [`first_child_that_died`], which is a **negative**
/// assertion behind a wall clock — the shape `docs/plans/debt-c6.md` §9 names three times in this
/// tree. What it proves is that nothing had failed *by then*, and that is equally true of a store
/// which has not finished opening.
///
/// Measured, with node 1's port already taken and the driver's left free: at no extra load the
/// bind failure lands inside the 250 ms and `cluster start` correctly refuses in 0.36 s; at eighty
/// busy threads it does not, and the command prints *4 nodes started*, writes a state file naming
/// a pid that is already dead, and supervises three nodes it calls four. `stop` then signals
/// whatever the operating system has since given that pid to — which is the second assertion in
/// `cluster_start.rs`'s `a_driver_that_cannot_listen_is_a_failure_and_not_a_cluster`, reached from the other side.
///
/// A store that answers `Admin::Regions` has opened its engine, registered with the driver if
/// there is one, and is serving. That is the event "started" was always meant to name, and it is
/// waited for here rather than timed.
fn wait_until_the_stores_answer(
    children: &mut [(u64, Child)],
    launched: &[Node],
    within: Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + within;
    let mut waiting: Vec<(u64, String)> = launched
        .iter()
        .filter(|node| node.id != 0)
        .map(|node| (node.id, node.address.clone()))
        .collect();
    loop {
        // A child that has exited is the precise answer and it is available at once. Without this
        // arm a store that cannot open would spend the whole budget failing to connect, and the
        // message at the end would say "did not answer" where "exited with status 1" is the truth.
        if let Some(died) = first_child_that_died(children) {
            return Err(died);
        }
        waiting.retain(|(_, address)| ask_a_store(address).is_err());
        if waiting.is_empty() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            let (id, address) = &waiting[0];
            return Err(format!(
                "{} did not answer on {address} within {within:?}",
                what(*id)
            ));
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

/// One round trip that only a store answers, or why it did not.
///
/// `Admin::Regions` for the same reason the driver is asked for its status: it is read-only, it
/// carries no epoch to be checked against a region the caller has not routed to, and nothing but
/// a store answers it.
fn ask_a_store(address: &str) -> Result<(), String> {
    let socket: SocketAddr = address
        .parse()
        .map_err(|error| format!("`{address}` is not an address: {error}"))?;
    let config = TransportConfig {
        request_timeout: PROBE_TIMEOUT,
        ..TransportConfig::new()
    };
    let store = esker_proto::BlockingTransport::connect_with(socket, config)
        .map_err(|error| format!("connecting to {address}: {error}"))?;
    match store.call(
        esker_proto::Request::Admin(esker_proto::AdminReq::Regions),
        std::time::Instant::now() + PROBE_TIMEOUT,
    ) {
        Ok(esker_proto::Response::Admin(_)) => Ok(()),
        Ok(other) => Err(format!(
            "{address} answered a store's question with {other:?}"
        )),
        Err(error) => Err(format!("asking {address} for its regions: {error}")),
    }
}

/// The first child that has already exited, described the way an operator needs it.
fn first_child_that_died(children: &mut [(u64, Child)]) -> Option<String> {
    children
        .iter_mut()
        .find_map(|(id, child)| match child.try_wait() {
            Ok(Some(status)) => Some(format!("{} exited with {status}", what(*id))),
            _ => None,
        })
}

/// `node N` or `the placement driver`, for a message an operator reads.
fn what(id: u64) -> String {
    if id == 0 {
        "the placement driver".to_owned()
    } else {
        format!("node {id}")
    }
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
fn wait_for_interrupt(children: &mut [(u64, Child)]) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("esker cluster: cannot listen for ctrl-c; stopping immediately");
        return;
    };
    // One line per child and never more: a reaped child answers `try_wait` with its status for
    // ever, and a supervisor that said so every tick would bury the log it exists to write.
    let mut reported: Vec<u64> = Vec::new();
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
                    for (id, child) in children.iter_mut() {
                        if reported.contains(id) {
                            continue;
                        }
                        if let Ok(Some(status)) = child.try_wait() {
                            reported.push(*id);
                            eprintln!("esker cluster: {} exited with {status}", what(*id));
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

    use std::process::{Command as Process, Stdio};
    use std::time::Duration;

    use super::{
        DEFAULT_BASE_PORT, Node, address_of, dir_of, first_child_that_died, read_state,
        wait_until_the_driver_answers, wait_until_the_stores_answer, write_state,
    };
    use crate::testserver::{TestPd, TestServer};

    /// A child that outlives the check, so the wait is timing out on the port rather
    /// than noticing an exit. `sleep` because a test may not assume a build artefact is around.
    fn sleeper() -> std::process::Child {
        Process::new("sleep")
            .arg("30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

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

    /// The wait returns as soon as the **driver** answers, which is the whole point: "started" is
    /// not "listening", and every store spawned after this is about to connect.
    #[test]
    fn the_wait_ends_when_the_driver_answers() {
        let pd = TestPd::start(0);
        let mut child = sleeper();
        let answer = wait_until_the_driver_answers(&pd.addr(), &mut child, Duration::from_secs(5));
        let _ = child.kill();
        assert!(answer.is_ok(), "{answer:?}");
    }

    /// **A socket that accepts and never speaks is not the driver.**
    ///
    /// The check this replaced polled `TcpStream::connect`, so a squatter satisfied it and
    /// `cluster start` went on to spawn four stores against somebody else's port — see
    /// `wait_until_the_driver_answers`. Red against that version, which returns `Ok` here in
    /// microseconds.
    #[test]
    fn a_squatter_on_the_drivers_port_is_not_the_driver() {
        let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = squatter.local_addr().unwrap().to_string();
        let mut child = sleeper();
        let error = wait_until_the_driver_answers(&address, &mut child, Duration::from_secs(2))
            .expect_err("a squatter on the driver's port was taken for the driver");
        let _ = child.kill();
        assert!(error.contains(&address), "{error}");
        assert!(error.contains("did not answer"), "{error}");
    }

    /// **And neither is a store**, which is the half a handshake alone would miss.
    ///
    /// A store on this port speaks the same wire protocol and completes the same handshake, so
    /// only asking a placement driver's question tells the two apart. Without this, a probe that
    /// stopped at the handshake would pass the test above and still hand four stores a port with
    /// no driver behind it.
    #[test]
    fn a_store_on_the_drivers_port_is_not_the_driver() {
        let store = TestServer::start();
        let mut child = sleeper();
        let error =
            wait_until_the_driver_answers(&store.addr(), &mut child, Duration::from_secs(2))
                .expect_err("a store on the driver's port was taken for the driver");
        let _ = child.kill();
        assert!(error.contains("did not answer"), "{error}");
    }

    /// A driver that exits is reported **as having exited**, not as a timeout: the two need
    /// different things from whoever reads the message, and waiting twenty seconds to say the
    /// wrong one of them is worse than useless.
    #[test]
    fn a_driver_that_exits_is_reported_as_that() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = free.local_addr().unwrap().to_string();
        drop(free);
        let mut child = Process::new("true").spawn().unwrap();
        let error = wait_until_the_driver_answers(&address, &mut child, Duration::from_secs(5))
            .expect_err("a driver that exited immediately was accepted");
        assert!(error.contains("exited with"), "{error}");
        assert!(!error.contains("within"), "reported as a timeout: {error}");
    }

    /// And one that stays up without ever binding is a timeout, with the address in it.
    #[test]
    fn a_driver_that_never_listens_times_out() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = free.local_addr().unwrap().to_string();
        drop(free);
        let mut child = sleeper();
        let error = wait_until_the_driver_answers(&address, &mut child, Duration::from_millis(200))
            .expect_err("a driver that never listened was accepted");
        let _ = child.kill();
        assert!(error.contains(&address), "{error}");
        assert!(error.contains("did not answer"), "{error}");
    }

    /// **A node that is alive and not serving is not a node that started**, and the check this
    /// replaced could not tell the two apart.
    ///
    /// The first assertion is the point: `first_child_that_died` — all the announcement gate used
    /// to consult — answers `None` for a child that is merely alive, which is what let
    /// `cluster start` print *4 nodes started* over a store still opening (or already doomed and
    /// not yet reaped). `sleep` stands in for exactly that store: nothing has died, and nothing
    /// answers either.
    ///
    /// It pins this function, not the wiring, and would pass against a gate that still slept and
    /// counted corpses. The regression for the **gate** is `cluster_start.rs`'s
    /// `a_node_that_cannot_listen_is_a_failure_and_not_a_cluster`, which is red against that gate
    /// at forty busy threads and above.
    #[test]
    fn a_node_that_is_alive_and_not_serving_is_not_a_started_cluster() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = free.local_addr().unwrap().to_string();
        drop(free);
        let mut children = vec![(1, sleeper())];
        let launched = vec![Node {
            id: 1,
            address: address.clone(),
            pid: 0,
        }];

        assert!(
            first_child_that_died(&mut children).is_none(),
            "the old gate saw a problem here; this test no longer shows what it could not see"
        );
        let error =
            wait_until_the_stores_answer(&mut children, &launched, Duration::from_millis(300))
                .expect_err("a node that never answered was reported as started");
        for (_, mut child) in children {
            let _ = child.kill();
        }
        assert!(error.contains("node 1"), "{error}");
        assert!(error.contains("did not answer"), "{error}");
    }

    /// And a store that **is** serving ends the wait, so the check cannot pass by never being
    /// satisfiable.
    #[test]
    fn the_wait_ends_when_every_store_answers() {
        let store = TestServer::start();
        let mut children = vec![(1, sleeper())];
        let launched = vec![Node {
            id: 1,
            address: store.addr(),
            pid: 0,
        }];
        let answer = wait_until_the_stores_answer(&mut children, &launched, Duration::from_secs(5));
        for (_, mut child) in children {
            let _ = child.kill();
        }
        assert!(answer.is_ok(), "{answer:?}");
    }

    /// A store that died is reported **as having died**, not as one that did not answer: the
    /// exit status is the diagnosis and the silence is only its symptom.
    #[test]
    fn a_store_that_exited_is_reported_as_that_rather_than_as_silence() {
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = free.local_addr().unwrap().to_string();
        drop(free);
        let mut dead = Process::new("false").spawn().unwrap();
        dead.wait().unwrap();
        let mut children = vec![(1, dead)];
        let launched = vec![Node {
            id: 1,
            address,
            pid: 0,
        }];
        let error = wait_until_the_stores_answer(&mut children, &launched, Duration::from_secs(5))
            .expect_err("a store that had exited was reported as started");
        assert!(error.contains("node 1 exited with"), "{error}");
    }

    /// The check that stands between a store that failed to open and an announcement claiming it
    /// started. `id` zero is the driver, and it is named as such.
    #[test]
    fn a_child_that_has_already_died_is_found_and_named() {
        let mut dead = Process::new("true").spawn().unwrap();
        // Waited for rather than slept past: `first_child_that_died` no longer sleeps 250 ms of
        // its own, so "it has exited" has to be a fact here and not a hope. `wait` caches the
        // status, which is what the `try_wait` inside then reads.
        dead.wait().unwrap();
        let mut children = vec![(0, dead), (1, sleeper())];
        let gone = first_child_that_died(&mut children).expect("the exited child was not noticed");
        assert!(
            gone.starts_with("the placement driver exited with"),
            "{gone}"
        );
        for (_, mut child) in children {
            let _ = child.kill();
        }

        let mut alive = vec![(1, sleeper()), (2, sleeper())];
        assert!(first_child_that_died(&mut alive).is_none());
        for (_, mut child) in alive {
            let _ = child.kill();
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
