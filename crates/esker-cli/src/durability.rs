//! **Acknowledged writes, and proving none of them was lost** — `CLAUDE.md` invariant 1.
//!
//! *"Log before state, fsync before ack. A write is acknowledged only after its WAL/Raft-log bytes
//! are durable."* Every other invariant in this project has an acceptance on the real topology and
//! this one does not, because proving it needs three things at once: a load that records what it
//! was told succeeded, something shooting the cluster while it runs, and a verdict computed
//! **after** the run from a file rather than from a process that may itself have died.
//!
//! Three verbs, deliberately separate processes:
//!
//! ```text
//! esker durability record --pd HOST:PORT --out writes.log --clients 4 --for 120s
//! esker durability chaos  --pd HOST:PORT --every 7s --for 120s --pids 1234,1235,1236
//! esker durability verify --pd HOST:PORT --in  writes.log
//! ```
//!
//! # What is not here, and why
//!
//! `crates/esker-cli/tests/cluster_chaos.rs` already kills the leader of three real store processes
//! under a `RawKv` load and checks the history for linearizability. This is not that again: it is
//! the **transactional** layer, a **random** victim rather than the leader, and a record that
//! outlives the run. Its module doc draws the line this exists for — *"that is the difference
//! between 'the store shuts down correctly' and 'an acknowledged write is durable', and only one of
//! them is the invariant."*

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use esker_client::TxnClient;
use esker_proto::{PdReq, PdResp};

/// **PD's oracle, over the wire** — `CLAUDE.md` invariant 6, *"timestamps come only from PD's
/// TSO"*.
///
/// A `CountingOracle` would be wrong here in a way that matters: two processes each counting from
/// one hand out the same timestamps, and this unit is two processes by design — the recorder writes
/// and the checker reads back **at what the recorder was told**. A timestamp that means something
/// different in the second process makes every read a lie.
///
/// A batch of one per call. A batch is what `bench pd tso` measures and what a busy writer wants,
/// and it is not what this wants: a run that took a hundred timestamps and used four would leave a
/// gap in the cluster's timeline that a later reader cannot tell from a lost write.
struct PdOracle {
    /// Behind a mutex because the writers ask from several threads and a connection is one socket.
    pd: std::sync::Mutex<crate::region::PdConn>,
}

// `PdConn` holds a socket and does not derive `Debug`, which the trait asks for. The address would
// be the useful thing to print and the connection does not expose it, so this says what the value
// is rather than inventing a field.
impl std::fmt::Debug for PdOracle {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("PdOracle")
    }
}

impl esker_client::TimestampOracle for PdOracle {
    fn tso(&self, count: u32) -> Result<u64, esker_proto::ProtoError> {
        let pd = self.pd.lock().map_err(|_| {
            esker_proto::ProtoError::invalid("the placement driver connection is poisoned")
        })?;
        match pd.call(&PdReq::Tso { count })? {
            PdResp::Tso { start_ts, .. } => Ok(start_ts),
            other => Err(esker_proto::ProtoError::invalid(format!(
                "the placement driver answered {other:?} to a timestamp request"
            ))),
        }
    }
}

/// Connects an oracle to the placement driver named by `pd`.
fn oracle(pd: &str) -> Result<Arc<dyn esker_client::TimestampOracle>, String> {
    let address = crate::raw::resolve(pd)?;
    Ok(Arc::new(PdOracle {
        pd: std::sync::Mutex::new(crate::region::PdConn::connect(address)?),
    }))
}

/// What `record` and `verify` share: where the cluster is and which file to use.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DurabilityOptions {
    /// The placement driver, `HOST:PORT`.
    pub(crate) pd: String,
    /// The record of acknowledged writes: written by `record`, read by `verify`.
    pub(crate) file: String,
    /// Concurrent writers, for `record`.
    pub(crate) clients: usize,
    /// How long to run, for `record` and `chaos`.
    pub(crate) run_for: Duration,
    /// How often to kill, for `chaos`.
    pub(crate) every: Duration,
    /// The store processes `chaos` may kill, by pid. **Given rather than discovered**: a lane never
    /// kills by name pattern, and the only pids this may signal are ones its caller named.
    pub(crate) pids: Vec<u32>,
    /// The key prefix, so two runs on one cluster cannot read each other's keys.
    pub(crate) keyspace: String,
}

impl DurabilityOptions {
    /// The defaults a run uses when the caller says nothing.
    pub(crate) fn new() -> Self {
        Self {
            pd: String::new(),
            file: "writes.log".to_owned(),
            clients: 4,
            run_for: Duration::from_secs(60),
            every: Duration::from_secs(7),
            pids: Vec::new(),
            keyspace: "durability".to_owned(),
        }
    }
}

/// One acknowledged write, as one line of the record.
///
/// Tab-separated and hand-rolled: this project has no `serde` and does not want one for a test
/// file (`CLAUDE.md`'s dependency policy). A key never contains a tab because this writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ack {
    /// Line number, so a gap in the file is visible. A missing *record* and a missing *write* are
    /// opposite failures and the checker must not confuse them.
    line: u64,
    key: String,
    value: String,
    /// The timestamp the write committed at. `verify` reads **at this**, not at "now".
    commit_ts: u64,
    /// Microseconds from the start of the run to the moment `commit` returned.
    at_micros: u64,
}

impl Ack {
    fn render(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\n",
            self.line, self.key, self.value, self.commit_ts, self.at_micros
        )
    }

    fn parse(text: &str) -> Result<Self, String> {
        let parts: Vec<&str> = text.split('\t').collect();
        let [line, key, value, commit_ts, at_micros] = parts[..] else {
            return Err(format!("a record has {} fields, not five", parts.len()));
        };
        let number = |what: &str, text: &str| {
            text.parse::<u64>()
                .map_err(|error| format!("{what}: {error}"))
        };
        Ok(Self {
            line: number("line", line)?,
            key: key.to_owned(),
            value: value.to_owned(),
            commit_ts: number("commit_ts", commit_ts)?,
            at_micros: number("at_micros", at_micros)?,
        })
    }
}

/// Runs the write load, appending one line per acknowledged commit.
pub(crate) fn record(options: &DurabilityOptions) -> Result<String, String> {
    let (transport, resolver) = crate::bench_route::routed(&options.pd)?;
    let oracle = oracle(&options.pd)?;
    let client = Arc::new(TxnClient::new(transport, resolver, oracle));

    let file = std::fs::File::create(&options.file)
        .map_err(|error| format!("creating {}: {error}", options.file))?;
    let out = Arc::new(std::sync::Mutex::new(std::io::BufWriter::new(file)));
    let lines = Arc::new(AtomicU64::new(0));
    let refused = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let began = Instant::now();
    let mut writers = Vec::new();
    for id in 0..options.clients {
        let client = Arc::clone(&client);
        let out = Arc::clone(&out);
        let lines = Arc::clone(&lines);
        let refused = Arc::clone(&refused);
        let stop = Arc::clone(&stop);
        let keyspace = options.keyspace.clone();
        writers.push(std::thread::spawn(move || {
            let mut mine = 0u64;
            while !stop.load(Ordering::Relaxed) {
                mine += 1;
                let key = format!("{keyspace}/{id}/{mine:08}");
                let value = format!("{id}-{mine}");
                let outcome = (|| {
                    let mut txn = client.begin()?;
                    txn.put(key.as_bytes(), value.as_bytes());
                    txn.commit()
                })();
                match outcome {
                    // **Only a commit that answered a timestamp is an acknowledged write.** A
                    // `None` is a transaction that wrote nothing, and an error is a write the
                    // cluster never promised — neither is this invariant's business, and counting
                    // one would make the checker chase a write that was correctly refused.
                    Ok(Some(commit_ts)) => {
                        let line = lines.fetch_add(1, Ordering::SeqCst) + 1;
                        let ack = Ack {
                            line,
                            key,
                            value,
                            commit_ts,
                            at_micros: u64::try_from(began.elapsed().as_micros())
                                .unwrap_or(u64::MAX),
                        };
                        // **Flushed per line, under the lock.** A record left in a buffer when the
                        // load is killed is a write the cluster kept and the checker will call
                        // lost — a false red, and a false red on this invariant is worse than no
                        // test at all.
                        if let Ok(mut out) = out.lock() {
                            let _ = out.write_all(ack.render().as_bytes());
                            let _ = out.flush();
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        refused.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }));
    }

    std::thread::sleep(options.run_for);
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        let _ = writer.join();
    }
    if let Ok(mut out) = out.lock() {
        let _ = out.flush();
    }
    Ok(format!(
        "recorded {} acknowledged writes to {} in {:?}; {} attempts were refused",
        lines.load(Ordering::SeqCst),
        options.file,
        began.elapsed(),
        refused.load(Ordering::Relaxed),
    ))
}

/// Reads every recorded write back **at the timestamp it was acknowledged at**.
pub(crate) fn verify(options: &DurabilityOptions) -> Result<String, String> {
    let (transport, resolver) = crate::bench_route::routed(&options.pd)?;
    let oracle = oracle(&options.pd)?;
    let client = TxnClient::new(transport, resolver, oracle);

    let text = std::fs::read_to_string(&options.file)
        .map_err(|error| format!("reading {}: {error}", options.file))?;
    let mut lost: Vec<String> = Vec::new();
    let mut checked = 0u64;
    let mut expected_line = 0u64;
    for raw in text.lines().filter(|line| !line.trim().is_empty()) {
        let ack = Ack::parse(raw)?;
        expected_line += 1;
        if ack.line != expected_line {
            return Err(format!(
                "the record has a gap: line {} follows {}. Records were lost, which is a broken \
                 recorder and not a lost write — this run does not count",
                ack.line,
                expected_line - 1
            ));
        }
        checked += 1;
        // **At `commit_ts`, not at now.** A key a later transaction overwrote is not a lost write,
        // and a checker that read the newest value would call every overwritten key a loss.
        let txn = client
            .begin_at(ack.commit_ts)
            .map_err(|error| format!("opening a reader at {}: {error}", ack.commit_ts))?;
        match txn.get(ack.key.as_bytes()) {
            Ok(Some(found)) if found.as_ref() == ack.value.as_bytes() => {}
            Ok(Some(other)) => lost.push(format!(
                "{raw}\t# rolled back: found {:?}",
                String::from_utf8_lossy(&other)
            )),
            Ok(None) => lost.push(format!("{raw}\t# missing")),
            Err(error) => lost.push(format!("{raw}\t# unreadable: {error}")),
        }
    }

    if lost.is_empty() {
        return Ok(format!(
            "{checked} acknowledged writes, every one of them read back at its own commit \
             timestamp"
        ));
    }
    // **A replayable list**: every failing line verbatim, so a re-check needs nothing but this.
    let failures = format!("{}.failures", options.file);
    let mut body = String::new();
    for line in &lost {
        body.push_str(line);
        body.push('\n');
    }
    std::fs::write(&failures, &body).map_err(|error| format!("writing {failures}: {error}"))?;
    Err(format!(
        "**{} of {checked} acknowledged writes are gone.** This is `CLAUDE.md` invariant 1 and it \
         is a P0: stop, do not re-run to see whether it goes away, and bring {failures} with the \
         kill log.\n{}",
        lost.len(),
        body,
    ))
}

/// Kills a store every `--every`, at random, and brings nothing back — the supervisor does that.
///
/// **The pids are given, never discovered.** A lane that killed by name pattern once took four
/// other lanes down with it; the only processes this may signal are ones its caller named on the
/// command line, and the caller is whoever started them.
pub(crate) fn chaos(options: &DurabilityOptions) -> Result<String, String> {
    if options.pids.len() < 3 {
        return Err(format!(
            "--pids named {} store processes; a cluster this kills from needs at least three, so \
             that taking one never takes the quorum",
            options.pids.len()
        ));
    }
    // A seeded generator rather than the system's: a chaos run that cannot be replayed is a bug
    // report nobody can act on (`esker-base`'s PCG32, which is this project's own).
    let seed = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos()),
    )
    .unwrap_or(1);
    let mut rng = esker_base::rng::Pcg32::from_seed(seed);
    let began = Instant::now();
    let mut log = vec![format!("seed {seed}")];
    let mut killed = 0u64;
    // **Its own deadline.** The arm ends whether or not anything answers: 134 orphaned busy loops
    // once drove this host to a load of 237 and only the lane that started them could stop them.
    while began.elapsed() < options.run_for {
        std::thread::sleep(
            options
                .every
                .min(options.run_for.saturating_sub(began.elapsed())),
        );
        if began.elapsed() >= options.run_for {
            break;
        }
        let at = usize::try_from(rng.next_u32()).unwrap_or(0) % options.pids.len();
        let pid = options.pids[at];
        let outcome = kill9(pid);
        killed += u64::from(outcome.is_ok());
        log.push(format!(
            "{:>8} ms  SIGKILL pid {pid}  {}",
            began.elapsed().as_millis(),
            match &outcome {
                Ok(()) => "sent".to_owned(),
                Err(reason) => format!("refused: {reason}"),
            }
        ));
    }
    Ok(format!(
        "{killed} of {} attempts killed a store in {:?}\n{}",
        log.len(),
        began.elapsed(),
        log.join("\n"),
    ))
}

/// Sends `SIGKILL` to one pid.
///
/// `kill(2)` through `libc` is not available — this workspace compiles no C — so this spends a
/// process on `/bin/kill`, which is what a shell would do and costs nothing at this cadence.
fn kill9(pid: u32) -> Result<(), String> {
    let status = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status()
        .map_err(|error| format!("running kill: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill -9 {pid} exited {status}"))
    }
}
