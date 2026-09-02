//! **Phase 6b acceptance**: a store loses its SSTs and rebuilds from object storage and its peers.
//!
//! `prompts/06-sql-serverless.md` §6b, Acceptance: *a store can be started with
//! `--sst-store s3://bucket/prefix` against `MinIO`, lose its local disk, and rebuild from object
//! storage plus its Raft peers with no data loss.*
//!
//! This is that run, against three real store **processes** and a real `MinIO`. The scenario is
//! built so that the two halves of "S3 **plus** its Raft peers" are separately necessary:
//!
//! ```text
//!   phase A   3 nodes up      writes flushed to SSTs, uploaded      -> only S3 can return these
//!   node 3 killed
//!   phase B   2 nodes up      writes committed by the quorum        -> only the peers can return these
//!   node 3's *.sst deleted
//!   node 3 restarted
//!   assert    node 3's own engine holds every key from both phases
//! ```
//!
//! Node 3's manifest survives the deletion and still names the files, so **an untiered store in
//! the same situation cannot open at all** — which is the control this file also runs. That is
//! what makes the positive result mean something: the SSTs really are gone, and the tier really
//! is what supplies them.
//!
//! The final assertion opens node 3's database **directly** rather than reading through the
//! cluster. Reading through the cluster proves only that *some* node has the data; a leader that
//! never lost anything would answer every query. Opening node 3's engine on the same tiered
//! filesystem it ran with is the only check that says what *that node* holds.
//!
//! # Running it
//!
//! ```sh
//! docker run -d --name esker-minio -p 19000:9000 \
//!     -e MINIO_ROOT_USER=eskertest -e MINIO_ROOT_PASSWORD=eskertest123 \
//!     minio/minio:latest server /data
//! docker exec esker-minio mc alias set local http://127.0.0.1:9000 eskertest eskertest123
//! docker exec esker-minio mc mb --ignore-existing local/esker
//!
//! cargo test -p esker-cli --test tier_acceptance -- --ignored --test-threads=1
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use esker_client::region_cache::StaticRegion;
use esker_client::{RawClient, TcpStores};
use esker_engine::{Db, Options, ReadOptions};
use esker_proto::TransportConfig;
use esker_s3::ObjectStore;
use tempfile::TempDir;

const REGION: u64 = 1;
const NODES: u64 = 3;
const NODE_COUNT: usize = 3;
/// The node that loses its SSTs. Not the one that starts as leader, so the scenario is a
/// follower losing its disk — which is the case an operator actually meets.
const VICTIM: u64 = 3;
const SEED: u64 = 20_260_831;
/// Small enough that a few hundred keys fill it, so the test produces real SSTs without pushing
/// 64 MiB through Raft to get one.
const WRITE_BUFFER: usize = 256 * 1024;
/// 400 bytes each; with the buffer above, phase A is several memtables and therefore several
/// SSTs rather than one.
const VALUE_LEN: usize = 400;
const PHASE_A_KEYS: usize = 2_000;
const PHASE_B_KEYS: usize = 400;

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

fn key_of(phase: &str, index: usize) -> Vec<u8> {
    format!("{phase}-{index:06}").into_bytes()
}

fn value_of(phase: &str, index: usize) -> Vec<u8> {
    let mut value = format!("{phase}:{index}:").into_bytes();
    value.resize(VALUE_LEN, b'.');
    value
}

fn address_of(base_port: u16, id: u64) -> SocketAddr {
    format!("127.0.0.1:{}", u64::from(base_port) + id - 1)
        .parse()
        .unwrap()
}

fn node_dir(data_dir: &Path, id: u64) -> PathBuf {
    data_dir.join(format!("node-{id}"))
}

/// A run of `NODES` consecutive free ports, as `cluster start` numbers from one base.
fn free_port_run() -> u16 {
    for base in (31_000_u16..39_000).step_by(NODE_COUNT + 1) {
        let bound: Vec<TcpListener> = (0..NODE_COUNT)
            .filter_map(|at| {
                let offset = u16::try_from(at).ok()?;
                TcpListener::bind(("127.0.0.1", base.checked_add(offset)?)).ok()
            })
            .collect();
        if bound.len() == NODE_COUNT {
            return base;
        }
    }
    panic!("no run of {NODES} consecutive free ports");
}

#[derive(Debug, Clone)]
struct Node {
    id: u64,
    address: SocketAddr,
    pid: u32,
}

fn read_state(data_dir: &Path) -> Option<Vec<Node>> {
    let text = std::fs::read_to_string(data_dir.join("cluster.state")).ok()?;
    let nodes: Vec<Node> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(Node {
                id: fields.next()?.parse().ok()?,
                address: fields.next()?.parse().ok()?,
                pid: fields.next()?.parse().ok()?,
            })
        })
        .collect();
    (nodes.len() == NODE_COUNT).then_some(nodes)
}

/// Three store processes, tiered into a prefix of this run's own.
struct Cluster {
    supervisor: Child,
    data_dir: TempDir,
    base_port: u16,
    nodes: Vec<Node>,
    store_url: String,
}

impl Cluster {
    fn start(store_url: Option<&str>) -> Self {
        let data_dir = TempDir::new().unwrap();
        let base_port = free_port_run();
        let mut command = Command::new(env!("CARGO_BIN_EXE_esker-cli"));
        command
            .arg("cluster")
            .arg("start")
            .arg("--nodes")
            .arg(NODES.to_string())
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("--base-port")
            .arg(base_port.to_string())
            .arg("--seed")
            .arg(SEED.to_string())
            .arg("--write-buffer-size")
            .arg(WRITE_BUFFER.to_string());
        if let Some(url) = store_url {
            command.arg("--sst-store").arg(url);
        }
        // Kept, not discarded: when a cluster does not come up, the supervisor's own output is
        // the only thing that says why, and a test that throws it away asserts "it did not
        // start" and leaves you guessing.
        let log_path = data_dir.path().join("supervisor.log");
        let log = std::fs::File::create(&log_path).expect("the supervisor log");
        let errors = log.try_clone().expect("the supervisor log");
        let supervisor = command
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("the cluster command starts");

        let deadline = Instant::now() + Duration::from_secs(30);
        let nodes = loop {
            if let Some(nodes) = read_state(data_dir.path()) {
                break nodes;
            }
            assert!(
                Instant::now() < deadline,
                "the supervisor never wrote cluster.state; it said:\n{}",
                std::fs::read_to_string(&log_path).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        };

        Self {
            supervisor,
            data_dir,
            base_port,
            nodes,
            store_url: store_url.unwrap_or_default().to_string(),
        }
    }

    fn addrs(&self) -> Vec<SocketAddr> {
        self.nodes.iter().map(|node| node.address).collect()
    }

    /// The addresses of every node but `id`, for driving a cluster one node short.
    fn addrs_without(&self, id: u64) -> Vec<SocketAddr> {
        self.nodes
            .iter()
            .filter(|node| node.id != id)
            .map(|node| node.address)
            .collect()
    }

    /// `kill -9`. Shelling out for the reason `cluster_chaos.rs` gives: `libc` is a
    /// `*-sys`-shaped dependency this project does not take.
    fn kill(&self, id: u64) {
        if let Some(node) = self.nodes.iter().find(|node| node.id == id) {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(node.pid.to_string())
                .status();
        }
    }

    /// Starts node `id` again with the arguments the supervisor gave it.
    ///
    /// Returns the child so a caller can find out whether it *stayed* up — which is the whole
    /// point of the untiered control, where it must not.
    fn restart(&mut self, id: u64) -> Child {
        let peers: Vec<String> = (1..=NODES)
            .map(|peer| format!("{peer}@{}", address_of(self.base_port, peer)))
            .collect();
        let mut command = Command::new(env!("CARGO_BIN_EXE_esker-cli"));
        command
            .arg("server")
            .arg("--data-dir")
            .arg(node_dir(self.data_dir.path(), id))
            .arg("--listen")
            .arg(address_of(self.base_port, id).to_string())
            .arg("--store-id")
            .arg(id.to_string())
            .arg("--seed")
            .arg(SEED.to_string())
            .arg("--write-buffer-size")
            .arg(WRITE_BUFFER.to_string());
        if !self.store_url.is_empty() {
            command
                .arg("--sst-store")
                .arg(format!("{}/node-{id}", self.store_url));
        }
        for peer in &peers {
            command.arg("--peer").arg(peer);
        }
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the node restarts");
        if let Some(node) = self.nodes.iter_mut().find(|node| node.id == id) {
            node.pid = child.id();
        }
        child
    }

    /// Stops the cluster the way the tool is meant to be stopped, then makes sure.
    ///
    /// `SIGINT` first: the supervisor's ctrl-C path stops every child it spawned and waits for
    /// them. `SIGKILL`ing it instead **orphans those children**, which then sit on their ports
    /// until something notices — and what notices is the next test, failing to start a cluster
    /// for reasons that have nothing to do with it. Learned the slow way.
    fn stop(&mut self) {
        let _ = Command::new("kill")
            .arg("-INT")
            .arg(self.supervisor.id().to_string())
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match self.supervisor.try_wait() {
                // Exited, or cannot be waited on: either way there is nothing left to wait for.
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        // The backstop: anything the supervisor did not own (a node this test restarted) or
        // did not manage to stop.
        for node in &self.nodes {
            let _ = Command::new("kill")
                .arg("-9")
                .arg(node.pid.to_string())
                .status();
        }
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Connects to whatever is listening, retrying while nodes come up.
fn connect(addrs: &[SocketAddr], within: Duration) -> RawClient {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(stores) = TcpStores::connect_all(addrs, TransportConfig::new()) {
            let ids = stores.store_ids();
            if !ids.is_empty() {
                return RawClient::new(
                    Arc::new(stores),
                    Arc::new(StaticRegion::replicated(REGION, &ids)),
                );
            }
        }
        assert!(
            Instant::now() < deadline,
            "nothing was listening on {addrs:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Writes `count` keys of `phase`, retrying each until it is acknowledged.
///
/// Retried because an election may be in flight; an unacknowledged write is not a claim this
/// test makes, so only the ones that come back `Ok` are asserted on later.
fn write_phase(client: &RawClient, phase: &str, count: usize) {
    for index in 0..count {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if client
                .put(&key_of(phase, index), &value_of(phase, index))
                .is_ok()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{phase}-{index:06} never committed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The `s3://` URL and the `ObjectStore` for one node's prefix.
fn store_for(store_url: &str, node: u64) -> Arc<dyn ObjectStore> {
    let endpoint =
        esker_s3::Endpoint::parse(&env_or("ESKER_S3_ENDPOINT", "http://localhost:19000")).unwrap();
    let config = esker_s3::Config::from_store_url(
        &format!("{store_url}/node-{node}"),
        endpoint,
        env_or("ESKER_S3_REGION", "us-east-1"),
        esker_s3::Credentials::new(
            env_or("ESKER_S3_KEY", "eskertest"),
            env_or("ESKER_S3_SECRET", "eskertest123"),
        ),
    )
    .unwrap();
    Arc::new(esker_s3::S3Client::new(config))
}

/// Deletes every `*.sst` under `dir`, leaving the manifest, `CURRENT`, the WAL and the Raft log.
///
/// This is "lose the local SST dir": the manifest still names every file, so a store without a
/// tier cannot open afterwards — which the control test asserts.
fn delete_local_ssts(dir: &Path) -> Vec<String> {
    let mut deleted = Vec::new();
    for entry in std::fs::read_dir(dir).expect("the node's data directory") {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_some_and(|ext| ext == "sst") {
            std::fs::remove_file(&path).expect("deleting an SST");
            deleted.push(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or_default()
                    .to_string(),
            );
        }
    }
    deleted.sort();
    deleted
}

/// The `*.sst` file names present in `dir`, sorted.
fn local_ssts(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the node's data directory")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            path.extension()
                .is_some_and(|ext| ext == "sst")
                .then(|| path.file_name()?.to_str().map(str::to_string))?
        })
        .collect();
    names.sort();
    names
}

/// Opens a node's engine directly and asserts every key of both phases is there.
///
/// `store_url` empty means a local filesystem, which is the control.
fn assert_engine_holds_everything(dir: &Path, store_url: &str, node: u64) {
    let fs: Arc<dyn esker_engine::FileSystem> = if store_url.is_empty() {
        Arc::new(esker_engine::LocalFileSystem::new())
    } else {
        let endpoint =
            esker_s3::Endpoint::parse(&env_or("ESKER_S3_ENDPOINT", "http://localhost:19000"))
                .unwrap();
        let config = esker_s3::Config::from_store_url(
            &format!("{store_url}/node-{node}"),
            endpoint,
            env_or("ESKER_S3_REGION", "us-east-1"),
            esker_s3::Credentials::new(
                env_or("ESKER_S3_KEY", "eskertest"),
                env_or("ESKER_S3_SECRET", "eskertest123"),
            ),
        )
        .unwrap();
        let key_prefix = config.prefix.clone();
        esker_engine::fs::tier::TieredFileSystem::new(
            Arc::new(esker_engine::LocalFileSystem::new()),
            Arc::new(esker_s3::S3Client::new(config)),
            dir,
            esker_engine::fs::tier::TierOptions {
                key_prefix,
                background: false,
                ..esker_engine::fs::tier::TierOptions::default()
            },
        )
        .expect("opening the tier")
    };

    let db = Db::open_with(dir, Options::default(), fs, &esker_engine::cf::BUILTIN)
        .expect("node's engine must open");

    let mut missing = Vec::new();
    for (phase, count) in [("phase-a", PHASE_A_KEYS), ("phase-b", PHASE_B_KEYS)] {
        for index in 0..count {
            let stored = db
                .get(
                    esker_engine::cf::DEFAULT,
                    &esker_keys::prefix::raw_key(&key_of(phase, index)),
                    &ReadOptions::default(),
                )
                .expect("a read of a tiered SST");
            match stored {
                Some(value) if value.as_ref() == value_of(phase, index).as_slice() => {}
                Some(_) => missing.push(format!("{phase}-{index:06} (wrong value)")),
                None => missing.push(format!("{phase}-{index:06}")),
            }
        }
    }
    assert!(
        missing.is_empty(),
        "node {node} lost {} of {} keys; first few: {:?}",
        missing.len(),
        PHASE_A_KEYS + PHASE_B_KEYS,
        &missing[..missing.len().min(5)]
    );

    // Informational only. By the time this opens, node `node` has been running for a while
    // with its own tier and has already fetched back everything it needed, so a miss here would
    // mean the *second* open found something absent — not that the first one did. What says
    // the tier was load-bearing is the restored file names above, and the untiered control.
    if let Some(stats) = db.tier_stats() {
        println!(
            "node {node} tier on re-open: hits {} misses {} ranged reads {}",
            stats.cache_hits, stats.cache_misses, stats.ranged_reads
        );
    }
}

/// **The acceptance.** Node 3 loses its SSTs while it is down, misses a phase of writes, and
/// comes back holding every key: the old ones from object storage, the new ones from its peers.
#[test]
#[ignore = "needs a MinIO container and starts three store processes; see the module docs"]
fn a_store_that_lost_its_ssts_rebuilds_from_s3_and_its_peers() {
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let store_url = format!(
        "s3://{}/acceptance-{run}",
        env_or("ESKER_S3_BUCKET", "esker")
    );
    let mut cluster = Cluster::start(Some(&store_url));
    let victim_dir = node_dir(cluster.data_dir.path(), VICTIM);

    // --- phase A: everything up, writes that will end up in SSTs -------------------------
    let client = connect(&cluster.addrs(), Duration::from_secs(30));
    write_phase(&client, "phase-a", PHASE_A_KEYS);
    drop(client);

    // The uploader is a background thread on a 5 s idle tick, so give it room. This waits on
    // the *object store*, which is the only thing that actually knows.
    let objects = store_for(&store_url, VICTIM);
    let prefix = format!("acceptance-{run}/node-{VICTIM}/");
    let deadline = Instant::now() + Duration::from_secs(60);
    let uploaded = loop {
        let listed = objects.list(&prefix).expect("listing the node's prefix");
        if !listed.is_empty() {
            break listed;
        }
        assert!(
            Instant::now() < deadline,
            "node {VICTIM} never uploaded an SST to {prefix}"
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    println!("node {VICTIM} uploaded {} SSTs", uploaded.len());

    // --- node 3 dies ----------------------------------------------------------------------
    cluster.kill(VICTIM);

    // --- phase B: the quorum keeps taking writes node 3 will have to catch up on -----------
    let client = connect(&cluster.addrs_without(VICTIM), Duration::from_secs(30));
    write_phase(&client, "phase-b", PHASE_B_KEYS);
    drop(client);

    // --- node 3 loses its SSTs ------------------------------------------------------------
    let deleted = delete_local_ssts(&victim_dir);
    assert!(
        !deleted.is_empty(),
        "the test deleted no SSTs, so it is not testing anything"
    );
    println!(
        "deleted {} local SSTs from node {VICTIM}: {deleted:?}",
        deleted.len()
    );

    // --- node 3 comes back ----------------------------------------------------------------
    let child = cluster.restart(VICTIM);
    // It must both start and stay started: a store that opens and then dies on its first read
    // would otherwise look like a pass.
    let client = connect(&cluster.addrs(), Duration::from_secs(60));
    for phase in ["phase-a", "phase-b"] {
        assert!(
            client.get(&key_of(phase, 0)).is_ok(),
            "the cluster stopped answering after node {VICTIM} rejoined"
        );
    }
    // Give the restarted node time to catch up on phase B from its peers.
    std::thread::sleep(Duration::from_secs(5));
    drop(client);

    // --- and holds everything itself ------------------------------------------------------
    cluster.stop();
    std::mem::forget(child);
    // A moment for the killed process to release the directory.
    std::thread::sleep(Duration::from_millis(500));

    // **The direct evidence.** The very files deleted while node 3 was down are back on its
    // disk, and the only place they could have come from is the bucket. This also separates
    // the two ways node 3 could have recovered: a Raft snapshot would have re-ingested the
    // range under *new* file numbers, so it is the reappearance of these exact names that says
    // object storage did the work.
    let present = local_ssts(&victim_dir);
    let restored: Vec<&String> = deleted
        .iter()
        .filter(|name| present.contains(name))
        .collect();
    println!(
        "node {VICTIM} after the restart holds {} SSTs; {} of the {} deleted came back: {restored:?}",
        present.len(),
        restored.len(),
        deleted.len()
    );
    assert!(
        !restored.is_empty(),
        "none of the deleted SSTs {deleted:?} came back — node {VICTIM} holds {present:?}, so it \
         recovered some other way and object storage was not what saved it"
    );

    assert_engine_holds_everything(&victim_dir, &store_url, VICTIM);
}

/// **The control.** The same deletion without a tier leaves a store that cannot open.
///
/// This is what makes the test above mean something: it proves the SSTs really were gone and
/// really were supplied by object storage, rather than the manifest having quietly stopped
/// referring to them.
#[test]
#[ignore = "starts three store processes; needs no MinIO"]
fn without_a_tier_the_same_deletion_leaves_a_store_that_cannot_open() {
    let mut cluster = Cluster::start(None);
    let victim_dir = node_dir(cluster.data_dir.path(), VICTIM);

    let client = connect(&cluster.addrs(), Duration::from_secs(30));
    write_phase(&client, "phase-a", PHASE_A_KEYS);
    drop(client);

    cluster.kill(VICTIM);
    let deleted = delete_local_ssts(&victim_dir);
    assert!(!deleted.is_empty(), "the test deleted no SSTs");
    let deleted = deleted.len();
    cluster.stop();
    std::thread::sleep(Duration::from_millis(500));

    // Opening is not the check: `Db::open_with` reads the manifest and builds the version, and
    // opens no SST until something reads one. The check is a *read*.
    let db = Db::open_with(
        &victim_dir,
        Options::default(),
        Arc::new(esker_engine::LocalFileSystem::new()),
        &esker_engine::cf::BUILTIN,
    )
    .expect("the manifest alone is enough to open");

    let mut lost = 0;
    let mut errors = 0;
    for index in 0..PHASE_A_KEYS {
        match db.get(
            esker_engine::cf::DEFAULT,
            &esker_keys::prefix::raw_key(&key_of("phase-a", index)),
            &ReadOptions::default(),
        ) {
            Ok(Some(_)) => {}
            Ok(None) => lost += 1,
            Err(_) => errors += 1,
        }
    }
    println!("untiered control: {deleted} SSTs deleted, {lost} keys gone, {errors} reads failed");
    assert!(
        lost + errors > 0,
        "an untiered store answered all {PHASE_A_KEYS} keys after {deleted} of its SSTs were \
         deleted — the SSTs held nothing, and the acceptance test above proves nothing"
    );
}

/// **`esker bench --adopt-sst-store`**, which is the hatch the refusal names.
///
/// A benchmark's database is a temporary directory, so its claim id is new on every run and every
/// re-run against a hand-named prefix meets objects it did not write. Before this flag existed the
/// refusal told the operator to *"re-run with `--adopt-sst-store`"* on a command that had no such
/// flag — a dead end with instructions on it.
///
/// The prefix is left holding objects and no marker, which is what a pre-6c prefix looks like and
/// what the refusal is written for.
#[test]
#[ignore = "needs a MinIO container; see the module docs"]
fn a_bench_is_refused_by_an_unclaimed_prefix_and_adopts_it_when_asked() {
    let bucket = env_or("ESKER_S3_BUCKET", "esker");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let store_url = format!("s3://{bucket}/bench-adopt-{nanos:x}");

    let run = |dir: &Path, adopt: bool| -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_esker-cli"));
        command
            .args(["bench", "fillrandom", "--num", "2000"])
            .arg("--dir")
            .arg(dir)
            .arg(format!("--sst-store={store_url}"))
            .env(
                "ESKER_S3_ENDPOINT",
                env_or("ESKER_S3_ENDPOINT", "http://localhost:19000"),
            )
            .env("ESKER_S3_KEY", env_or("ESKER_S3_KEY", "eskertest"))
            .env("ESKER_S3_SECRET", env_or("ESKER_S3_SECRET", "eskertest123"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if adopt {
            command.arg("--adopt-sst-store");
        }
        command.output().expect("the benchmark runs")
    };

    // The first run claims the prefix and writes into it.
    let first = tempfile::tempdir().unwrap();
    let output = run(first.path(), false);
    assert!(
        output.status.success(),
        "the first run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The marker goes, and an object stays: a prefix with data and nothing saying whose it is,
    // which is what a pre-6c prefix looks like from outside.
    let (store, prefix) = plain_store(&store_url);
    store
        .put(&format!("{prefix}000004.sst"), b"somebody's sst")
        .expect("the object store takes it");
    store
        .delete(&format!("{prefix}ESKER-CLAIM"))
        .expect("the marker is removed");
    assert!(
        store.get(&format!("{prefix}ESKER-CLAIM")).is_err(),
        "the marker survived the delete, so the refusal below would be the wrong one"
    );

    // A second run, from a different directory and therefore a different claim id, is refused —
    // and the message names the flag.
    let second = tempfile::tempdir().unwrap();
    let output = run(second.path(), false);
    assert!(
        !output.status.success(),
        "an unclaimed prefix was adopted silently"
    );
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("no claim marker"), "{text}");
    assert!(text.contains("--adopt-sst-store"), "{text}");

    // With the flag, the same run succeeds. That is the whole unit: the hatch the refusal names
    // now exists on the command that prints it.
    let third = tempfile::tempdir().unwrap();
    let output = run(third.path(), true);
    assert!(
        output.status.success(),
        "the hatch did not open: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// An `ObjectStore` for a whole `s3://bucket/prefix`, and the key prefix it resolves to.
///
/// The prefix comes back because [`ObjectStore`] keys are **whole** keys: the client does not
/// prepend anything, `TieredFileSystem` does. Computing it here by hand is how the first draft of
/// this test deleted nothing and then asserted on the wrong refusal.
fn plain_store(store_url: &str) -> (Arc<dyn ObjectStore>, String) {
    let endpoint =
        esker_s3::Endpoint::parse(&env_or("ESKER_S3_ENDPOINT", "http://localhost:19000")).unwrap();
    let config = esker_s3::Config::from_store_url(
        store_url,
        endpoint,
        env_or("ESKER_S3_REGION", "us-east-1"),
        esker_s3::Credentials::new(
            env_or("ESKER_S3_KEY", "eskertest"),
            env_or("ESKER_S3_SECRET", "eskertest123"),
        ),
    )
    .unwrap();
    let prefix = config.prefix.clone();
    (Arc::new(esker_s3::S3Client::new(config)), prefix)
}
