//! `esker cluster start` / `esker cluster stop` — three stores on localhost, one region.
//!
//! This is a development and testing command, not an operator tool. There is no placement driver
//! yet (phase 4), so the membership is fixed at start and every store is told the whole peer list
//! on its command line.
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
        } => start(
            *nodes,
            data_dir,
            *base_port,
            *seed,
            sst_store.as_deref(),
            *write_buffer_size,
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

fn start(
    nodes: u64,
    data_dir: &Path,
    base_port: u16,
    seed: u64,
    sst_store: Option<&str>,
    write_buffer_size: Option<usize>,
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
    for id in 1..=nodes {
        let dir = dir_of(data_dir, id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("creating {}: {error}", dir.display()))?;

        let mut process = Process::new(&binary);
        process
            .arg("server")
            .arg("--data-dir")
            .arg(&dir)
            .arg("--listen")
            .arg(address_of(base_port, id))
            .arg("--store-id")
            .arg(id.to_string())
            .arg("--seed")
            .arg(seed.to_string());
        if let Some(store_url) = sst_store {
            process
                .arg("--sst-store")
                .arg(crate::sst_store::for_node(store_url, id));
        }
        if let Some(size) = write_buffer_size {
            process.arg("--write-buffer-size").arg(size.to_string());
        }
        for peer in &peers {
            process.arg("--peer").arg(peer);
        }

        match process.spawn() {
            Ok(child) => {
                launched.push(Node {
                    id,
                    address: address_of(base_port, id),
                    pid: child.id(),
                });
                children.push((id, child));
            }
            Err(error) => {
                // A node that will not start leaves the cluster short of a quorum, so the ones
                // already running are stopped rather than left half-formed.
                for (_, mut child) in children {
                    let _ = child.kill();
                }
                return Err(format!("starting node {id}: {error}"));
            }
        }
    }

    write_state(data_dir, &launched)?;
    println!("esker cluster: {nodes} nodes started");
    for node in &launched {
        println!("  node {} on {} (pid {})", node.id, node.address, node.pid);
    }
    println!(
        "esker cluster: ctrl-C to stop, or `esker cluster stop --data-dir {}`",
        data_dir.display()
    );

    // Supervise: wait for ctrl-C, then stop every child politely and wait for it.
    wait_for_interrupt();
    println!("esker cluster: stopping");
    for (id, child) in &mut children {
        signal(child.id(), "TERM");
        match child.wait() {
            Ok(status) => println!("  node {id} exited with {status}"),
            Err(error) => eprintln!("  node {id}: {error}"),
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
            "esker cluster: stopping node {} (pid {})",
            node.id, node.pid
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

/// Blocks until ctrl-C, using a small runtime of its own.
fn wait_for_interrupt() {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        eprintln!("esker cluster: cannot listen for ctrl-c; stopping immediately");
        return;
    };
    runtime.block_on(async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            eprintln!("esker cluster: cannot listen for ctrl-c ({error}); stopping");
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
