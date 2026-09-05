//! The cluster the measurement runs on: a placement driver, some stores and one SQL node.
//!
//! `esker cluster start` already does most of this and is deliberately not reused. It has no way
//! to shorten a store's heartbeat — and at the default 60 s a columnar learner takes two minutes
//! to place (`docs/bench/columnar-learner.md`, "How long it takes, and why") — no way to start a
//! SQL node, and it supervises until ctrl-C rather than handing its children back. What is
//! borrowed is the part that was learned the hard way: **a readiness check must prove identity**,
//! so every wait here is a round trip only the right kind of process completes, watching the child
//! at the same time so an immediate exit is reported as an exit rather than as a timeout.
//!
//! # Nothing is left running
//!
//! [`Cluster`] kills every child in `Drop`, and [`Cluster::stop`] waits for them and says what
//! could not be reaped. A benchmark that leaves a store holding a port is a benchmark whose next
//! run measures the previous one.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

use esker_proto::{
    AdminReq, AdminResp, PeerRole, Region, RegionStatus, Request, Response, TransportConfig,
};

use super::pg::Pg;
use super::probe::Sample;

/// How long one readiness probe waits before it is retried.
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// How often a wait re-probes.
const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// How long the placement driver has to answer.
const PD_START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a store has to open its database and answer.
const STORE_START_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the SQL node has to take a connection and answer `SELECT 1`.
const SQL_START_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a stopped child has to exit before it is reported as still running.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How many lines of a failed child's stderr an error carries.
const LOG_TAIL_LINES: usize = 12;

/// A child's name as a filename.
fn slug(what: &str) -> String {
    what.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// What to build.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    /// Everything the cluster writes, one directory per process.
    pub(crate) data_dir: PathBuf,
    /// How many stores.
    pub(crate) stores: u64,
    /// Store `i` listens on `base_port + i - 1`; the driver above them, the SQL node above that.
    pub(crate) base_port: u16,
    /// Approximate region bytes past which a leader looks for a split key.
    ///
    /// This is how the number of fragments is chosen: one fragment per region, so a small value
    /// and a fixed row count is what puts four regions under one table.
    pub(crate) region_split_size: u64,
    /// How often a region's leader reports it absent a change, in milliseconds.
    ///
    /// Every placement operator costs one of these, so the default 60 s would spend minutes
    /// placing learners this benchmark has to wait for anyway.
    pub(crate) region_heartbeat_ms: u64,
    /// The resolution the two heartbeat intervals are counted in. An interval below one tick is
    /// rounded up to one, so shortening one without this does nothing.
    pub(crate) heartbeat_tick_ms: u64,
}

impl Layout {
    /// Where store `id` listens.
    fn store_address(&self, id: u64) -> String {
        format!(
            "127.0.0.1:{}",
            self.base_port + u16::try_from(id).unwrap_or(1) - 1
        )
    }

    /// Where the placement driver listens: one above the last store.
    pub(crate) fn pd_address(&self) -> String {
        format!(
            "127.0.0.1:{}",
            self.base_port + u16::try_from(self.stores).unwrap_or(1)
        )
    }

    /// Where the SQL node listens: one above the driver.
    pub(crate) fn sql_address(&self) -> String {
        format!(
            "127.0.0.1:{}",
            self.base_port + u16::try_from(self.stores).unwrap_or(1) + 1
        )
    }
}

/// One store, once it is answering.
#[derive(Debug, Clone)]
pub(crate) struct StoreNode {
    /// Its store id, which is also its index from one.
    pub(crate) id: u64,
    /// Where it listens.
    pub(crate) address: String,
    /// Its process, for [`Sample`].
    pub(crate) pid: u32,
}

/// A running cluster, and the children that are it.
#[derive(Debug)]
pub(crate) struct Cluster {
    /// The stores, in id order.
    pub(crate) stores: Vec<StoreNode>,
    /// The SQL node's address.
    pub(crate) sql_address: String,
    /// The SQL node's process, for [`Sample`].
    pub(crate) sql_pid: u32,
    children: Vec<(String, Child)>,
    log_dir: PathBuf,
}

impl Cluster {
    /// Starts a driver, `layout.stores` stores and a SQL node, and returns once all of them answer.
    ///
    /// Every failure kills what is already running, because a half-started cluster holding ports
    /// is what makes the *next* run fail for a reason that has nothing to do with it.
    pub(crate) fn start(layout: &Layout) -> Result<Self, String> {
        let esker =
            std::env::current_exe().map_err(|error| format!("finding this executable: {error}"))?;
        let sql_binary = esker
            .parent()
            .ok_or("this executable has no directory")?
            .join("esker-sql");
        if !sql_binary.exists() {
            return Err(format!(
                "no esker-sql beside this binary at {}: build it with \
                 `cargo build --release -p esker-sql`",
                sql_binary.display()
            ));
        }

        let log_dir = layout.data_dir.join("logs");
        create(&log_dir)?;
        let mut cluster = Self {
            stores: Vec::new(),
            sql_address: layout.sql_address(),
            sql_pid: 0,
            children: Vec::new(),
            log_dir,
        };
        if let Err(why) = cluster.build(layout, &esker, &sql_binary) {
            let said = cluster.logs();
            cluster.kill_all();
            return Err(format!("{why}{said}"));
        }
        Ok(cluster)
    }

    fn build(&mut self, layout: &Layout, esker: &Path, sql_binary: &Path) -> Result<(), String> {
        let pd_dir = layout.data_dir.join("pd");
        create(&pd_dir)?;
        let pd_address = layout.pd_address();
        let mut driver = Process::new(esker);
        driver
            .arg("pd")
            .arg("serve")
            .arg("--data-dir")
            .arg(&pd_dir)
            .arg("--listen")
            .arg(&pd_address);
        self.spawn("the placement driver", driver)?;
        self.wait("the placement driver", PD_START_TIMEOUT, || {
            ask_the_driver(&pd_address)
        })?;

        for id in 1..=layout.stores {
            let dir = layout.data_dir.join(format!("node-{id}"));
            create(&dir)?;
            let address = layout.store_address(id);
            let mut store = Process::new(esker);
            store
                .arg("server")
                .arg("--data-dir")
                .arg(&dir)
                .arg("--listen")
                .arg(&address)
                .arg("--store-id")
                .arg(id.to_string())
                .arg("--pd")
                .arg(&pd_address);
            // **`--peer` as well as `--pd`, which is what the working reference does.**
            // `crate::cluster` builds its peer list unconditionally and passes it whether or not
            // there is a placement driver, and leaving it out is what wedged this benchmark's
            // first cluster: PD issued `AddPeer` 32 ms after bootstrap, the leader could not
            // reach a peer whose address nothing had told it, and the operator timed out at
            // PD's 300 s ceiling and was reissued to the next store for ever. The region stayed
            // at one voter, so `repair_for` held its only operator slot and `columnar_for` --
            // which `esker_pd::pd::repair` reaches only after repair -- never got a turn.
            for peer in 1..=layout.stores {
                store
                    .arg("--peer")
                    .arg(format!("{peer}@{}", layout.store_address(peer)));
            }
            store
                .arg("--region-split-size")
                .arg(layout.region_split_size.to_string())
                .arg("--region-heartbeat-ms")
                .arg(layout.region_heartbeat_ms.to_string())
                .arg("--heartbeat-tick-ms")
                .arg(layout.heartbeat_tick_ms.to_string());
            let pid = self.spawn(&format!("store {id}"), store)?;
            self.stores.push(StoreNode {
                id,
                address: address.clone(),
                pid,
            });
        }
        for store in self.stores.clone() {
            self.wait(&format!("store {}", store.id), STORE_START_TIMEOUT, || {
                ask_a_store(&store.address)
            })?;
        }

        let sql_address = layout.sql_address();
        let mut sql = Process::new(sql_binary);
        sql.arg("--pd").arg(&pd_address).arg(&sql_address);
        for store in &self.stores {
            sql.arg(&store.address);
        }
        self.sql_pid = self.spawn("the SQL node", sql)?;
        self.wait("the SQL node", SQL_START_TIMEOUT, || {
            // A connection *and* an answer. A SQL node accepts a socket before it holds a schema
            // lease, so a bare connect would let the load begin against a node that is about to
            // refuse every statement.
            let mut pg = Pg::connect(&sql_address, "esker", "esker")?;
            match pg.query("SELECT 1")?.scalar()? {
                "1" => Ok(()),
                other => Err(format!("`SELECT 1` answered {other}")),
            }
        })?;
        Ok(())
    }

    /// Spawns a child, keeping it for teardown, and answers with its pid.
    ///
    /// **Each child gets its own log file**, under the data directory and named after it, with
    /// stdout and stderr both going into it so their order is preserved.
    ///
    /// Inheriting this process's would be simpler and is what `esker cluster start` does — and
    /// `docs/bench/columnar-learner.md` records what that cost: nine processes writing one pipe,
    /// two of the failures interleaving mid-line, and no way to say which store said what. A
    /// benchmark whose diagnosis is "one of these ten processes panicked" has no diagnosis.
    ///
    /// **Both streams, because the interesting half is on stdout.** This first captured only
    /// stderr, on the reasoning that a store which refuses to start says why there. It does — a
    /// *panic* does. `tracing_subscriber::fmt()` writes to **stdout**, so every `INFO` and `WARN`
    /// any of these processes emitted went to `/dev/null`, and a schema lease expiring mid-load
    /// read as an empty file.
    fn spawn(&mut self, what: &str, mut process: Process) -> Result<u32, String> {
        let log = self.log_dir.join(format!("{}.log", slug(what)));
        let out = std::fs::File::create(&log)
            .map_err(|error| format!("creating {}: {error}", log.display()))?;
        let err = out
            .try_clone()
            .map_err(|error| format!("duplicating {}: {error}", log.display()))?;
        let child = process
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .map_err(|error| format!("starting {what}: {error}"))?;
        let pid = child.id();
        self.children.push((what.to_owned(), child));
        Ok(pid)
    }

    /// The tail of every child's log, for an error that needs to name which process failed.
    pub(crate) fn logs(&self) -> String {
        let mut said = String::new();
        for (what, _) in &self.children {
            let path = self.log_dir.join(format!("{}.log", slug(what)));
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            if lines.is_empty() {
                continue;
            }
            let tail = lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n    ");
            let _ = write!(said, "\n  {what} said:\n    {tail}");
        }
        said
    }

    /// Probes until `ask` succeeds, the child exits, or the deadline passes.
    fn wait<F>(&mut self, what: &str, within: Duration, ask: F) -> Result<(), String>
    where
        F: Fn() -> Result<(), String>,
    {
        let deadline = Instant::now() + within;
        loop {
            if let Some((name, status)) = self.first_child_that_died() {
                return Err(format!(
                    "{name} exited with {status} while waiting for {what}"
                ));
            }
            // The deadline is only ever reached after a probe, so the last refusal always exists
            // and the message can name which half failed.
            let refusal = match ask() {
                Ok(()) => return Ok(()),
                Err(why) => why,
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "{what} did not answer within {within:?}: {refusal}"
                ));
            }
            std::thread::sleep(PROBE_INTERVAL);
        }
    }

    fn first_child_that_died(&mut self) -> Option<(String, std::process::ExitStatus)> {
        self.children
            .iter_mut()
            .find_map(|(what, child)| match child.try_wait() {
                Ok(Some(status)) => Some((what.clone(), status)),
                _ => None,
            })
    }

    /// A [`Sample`] of the SQL node and of every store, or why one could not be read.
    pub(crate) fn sample(&self) -> Result<(Sample, Vec<Sample>), String> {
        let sql = Sample::read(self.sql_pid)?;
        let stores = self
            .stores
            .iter()
            .map(|store| Sample::read(store.pid))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((sql, stores))
    }

    /// Every region, **as its own leader describes it**.
    ///
    /// One entry per region, with the peers and their roles, so the report can say how many
    /// fragments a query has and how many *distinct stores* answer them — which is not the same
    /// number and is the one a reader will assume.
    ///
    /// # Why the leader's view and not the first answer
    ///
    /// Taking whichever store answered first made this benchmark report *"every region reached 3
    /// voters in 1.8ms"*, which is not a thing that can happen: a store started with `--peer` has
    /// the whole peer list in hand before any conf change is committed, so it can describe a
    /// membership that is only configured. The leader is the one peer that cannot say that — a
    /// region's committed membership is exactly what its leader has applied. A region no store
    /// claims to lead is left out rather than guessed at, so a check over this list is waiting for
    /// a leader too.
    pub(crate) fn regions(&self) -> Result<Vec<Region>, String> {
        let mut led: Vec<Region> = Vec::new();
        for store in &self.stores {
            for status in regions_of(&store.address)? {
                if status.is_leader && !led.iter().any(|region| region.id == status.region.id) {
                    led.push(status.region);
                }
            }
        }
        led.sort_by_key(|region| region.id);
        Ok(led)
    }

    /// Stops every child and waits for it, naming any that would not go.
    pub(crate) fn stop(mut self) -> Result<(), String> {
        self.kill_all();
        let deadline = Instant::now() + STOP_TIMEOUT;
        let mut stubborn = Vec::new();
        for (what, child) in &mut self.children {
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(PROBE_INTERVAL);
                    }
                    Ok(None) => {
                        stubborn.push(format!("{what} (pid {})", child.id()));
                        break;
                    }
                    Err(error) => {
                        stubborn.push(format!("{what}: {error}"));
                        break;
                    }
                }
            }
        }
        self.children.clear();
        if stubborn.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "still running after {STOP_TIMEOUT:?}: {}",
                stubborn.join(", ")
            ))
        }
    }

    fn kill_all(&mut self) {
        for (_, child) in &mut self.children {
            let _ = child.kill();
        }
    }
}

impl Drop for Cluster {
    /// The last defence, for the paths that return early.
    ///
    /// [`Cluster::stop`] is what a finished run calls and what reports; this is what an error on
    /// the way there relies on, and it is why no failure in this module leaves a store on a port.
    fn drop(&mut self) {
        self.kill_all();
        for (_, child) in &mut self.children {
            let _ = child.wait();
        }
    }
}

fn create(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("creating {}: {error}", dir.display()))
}

/// One round trip only a placement driver completes.
fn ask_the_driver(address: &str) -> Result<(), String> {
    let socket: SocketAddr = address
        .parse()
        .map_err(|error| format!("`{address}` is not an address: {error}"))?;
    let config = TransportConfig {
        request_timeout: PROBE_TIMEOUT,
        ..TransportConfig::new()
    };
    let pd = crate::region::PdConn::connect_with(socket, config)?;
    match pd.call(&esker_proto::PdReq::Status) {
        Ok(esker_proto::PdResp::Status { .. }) => Ok(()),
        Ok(other) => Err(format!(
            "{address} answered a driver's question with {other:?}"
        )),
        Err(error) => Err(format!("asking {address} for its status: {error}")),
    }
}

/// One round trip only a store completes.
fn ask_a_store(address: &str) -> Result<(), String> {
    regions_of(address).map(|_| ())
}

/// What `address` says it hosts, leader flag included.
fn regions_of(address: &str) -> Result<Vec<RegionStatus>, String> {
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
        Request::Admin(AdminReq::Regions),
        Instant::now() + PROBE_TIMEOUT,
    ) {
        Ok(Response::Admin(AdminResp::Regions { regions })) => Ok(regions),
        Ok(other) => Err(format!(
            "{address} answered a store's question with {other:?}"
        )),
        Err(error) => Err(format!("asking {address} for its regions: {error}")),
    }
}

/// How many regions have a columnar learner, and how many distinct stores hold one.
///
/// Both numbers, because they diverge: PD places a learner on the *healthiest store without a
/// peer* of that region (`esker_pd::schedule`), and four regions whose voters sit on the same
/// three stores put all four learners on the same fourth one.
pub(crate) fn columnar_spread(regions: &[Region]) -> (usize, usize) {
    let mut stores: Vec<u64> = Vec::new();
    let mut with_a_learner = 0;
    for region in regions {
        let learners: Vec<u64> = region
            .peers
            .iter()
            .filter(|peer| peer.role == PeerRole::ColumnarLearner)
            .map(|peer| peer.store_id)
            .collect();
        if !learners.is_empty() {
            with_a_learner += 1;
        }
        for store in learners {
            if !stores.contains(&store) {
                stores.push(store);
            }
        }
    }
    (with_a_learner, stores.len())
}

/// The distinct stores a columnar learner sits on, ascending.
///
/// [`columnar_spread`] returns how many; this returns which, and the difference matters in the
/// report. "2 distinct stores" on a four-store cluster is arithmetic a reader can argue with;
/// "stores 1 and 3" is a placement they can go and look at.
pub(crate) fn learner_stores(regions: &[Region]) -> Vec<u64> {
    let mut stores: Vec<u64> = regions
        .iter()
        .flat_map(|region| region.peers.iter())
        .filter(|peer| peer.role == PeerRole::ColumnarLearner)
        .map(|peer| peer.store_id)
        .collect();
    stores.sort_unstable();
    stores.dedup();
    stores
}
