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

// `PdConn` holds a socket and does not derive `Debug`, which the trait asks for. The useful thing
// to print is **which member it is talking to**, and since ADR 0108 the connection can say: a run
// that has moved between members is a run whose log should show it moved.
impl std::fmt::Debug for PdOracle {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pd.lock() {
            Ok(pd) => write!(out, "PdOracle({})", pd.address()),
            Err(_) => out.write_str("PdOracle(poisoned)"),
        }
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

/// Connects an oracle to the placement-driver **group** named by `pd`.
///
/// A list, because a run that kills the driver that was leading is the run this tool exists for:
/// an oracle holding one member stops handing out timestamps when that member is the one that
/// dies, and every writer stops with it (ADR 0108). One address is a group of one.
fn oracle(pd: &str) -> Result<Arc<dyn esker_client::TimestampOracle>, String> {
    let addresses = crate::raw::resolve_all(pd)?;
    Ok(Arc::new(PdOracle {
        pd: std::sync::Mutex::new(crate::region::PdConn::connect_to(
            &addresses,
            esker_proto::TransportConfig::new(),
        )?),
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
    /// A cluster's state file, re-read before every kill.
    ///
    /// **A restarted store has a new pid**, so a list given once on the command line names corpses
    /// after the first round — which is what makes `--pids` a one-shot and this the loop. The
    /// supervisor rewrites this file when it restarts a store (`esker cluster start`), so it is the
    /// only place that knows which pid is current.
    pub(crate) state: Option<String>,
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
            state: None,
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
    // **A client per writer, each with its own connection to the driver.**
    //
    // One shared oracle is one socket behind one mutex, and every transaction takes two timestamps
    // — so four writers sharing it serialise on it. Measured on this harness: 236 acknowledged
    // writes a second with a connection each, 36 with one between them. A load generator that
    // bottlenecks on its own plumbing measures the plumbing.
    //
    // Worth saying where this does *not* apply: a SQL node has one `PdConn` for the whole process
    // by design, and now takes its timestamps through it. Whether that is a serialisation point
    // under many sessions is a real question and it is the node's, not this tool's — it wants its
    // own measurement rather than a change made here on a hunch.
    let mut clients = Vec::new();
    for _ in 0..options.clients.max(1) {
        let (transport, resolver) = crate::bench_route::routed(&options.pd)?;
        clients.push(Arc::new(TxnClient::new(
            transport,
            resolver,
            oracle(&options.pd)?,
        )));
    }

    let mut file = std::fs::File::create(&options.file)
        .map_err(|error| format!("creating {}: {error}", options.file))?;
    // **One writer, and the line number assigned where the line is written.**
    //
    // Run 118 found a hole at line 15 of 3,550, nowhere near a kill: four clients took line numbers
    // from an atomic counter and then appended under a mutex, so the *order* they were numbered in
    // and the order they were written in were two different orders. A lock stops the bytes
    // interleaving; it does not make the numbering match. And a gap is not a cosmetic fault — the
    // checker reads one as "records were lost, so this run does not count", which is what turned a
    // whole real-topology run into no verdict at all.
    //
    // So the number is not taken until the line is about to be written, by the one thread that
    // writes: numbering and order become the same event rather than two that agree by luck.
    let (sender, records) = std::sync::mpsc::channel::<Ack>();
    let lines = Arc::new(AtomicU64::new(0));
    let written = Arc::clone(&lines);
    let scribe = std::thread::spawn(move || -> Result<(), String> {
        let mut line = 0u64;
        for mut ack in records {
            line += 1;
            ack.line = line;
            file.write_all(ack.render().as_bytes())
                .map_err(|error| format!("writing the record: {error}"))?;
            // Per line, because a record left in a buffer when the load is killed is a write the
            // cluster kept and the checker would call lost.
            file.flush()
                .map_err(|error| format!("flushing the record: {error}"))?;
            written.store(line, Ordering::SeqCst);
        }
        Ok(())
    });
    let refused = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let began = Instant::now();
    let mut writers = Vec::new();
    for (id, client) in clients.into_iter().enumerate() {
        let sender = sender.clone();
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
                        // **No line number here.** The scribe assigns it as it writes; see there.
                        let _ = sender.send(Ack {
                            line: 0,
                            key,
                            value,
                            commit_ts,
                            at_micros: u64::try_from(began.elapsed().as_micros())
                                .unwrap_or(u64::MAX),
                        });
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
    // Every writer is finished, so dropping this closes the channel and the scribe ends after the
    // last record — rather than at a deadline, which would drop the tail.
    drop(sender);
    scribe
        .join()
        .map_err(|_| "the record writer panicked".to_owned())??;
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

/// Kills a store every `--every`, at random, **and only while the cluster is healthy**.
///
/// # The guard run 118 found missing
///
/// The first version counted `--pids` **at parse time** and called that a quorum check. It is not:
/// it asks how many stores were *named*, not how many are *alive*. Against four stores it fired at
/// 7, 14, 28 and 42 s, took the cluster below its quorum of three on the second, and spent the
/// remaining fourteen shots on corpses — while the recorder, correctly, could no longer commit
/// anything. A whole real-topology run produced no verdict.
///
/// So the guard is dynamic and it asks the only authority that matters: **can the cluster still
/// acknowledge a write?** A probe commit before every kill, and none is fired unless the last one
/// succeeded. That needs nothing but `--pd`, which this already has.
///
/// **Liveness is not pid liveness**, and that is why the probe is a write. A store that is
/// restarted comes back with a *different* pid, so `kill -0` on the pid this was given answers "no"
/// for ever after the first kill and would stop the run for the wrong reason. What the invariant
/// cares about is whether the cluster serves, and a commit is that question asked directly.
///
/// # What this does not do, and why
///
/// It does not restart what it kills. A chaos arm that started stores would have to know the whole
/// launch configuration — data directory, ports, seed, store ids — which is `cluster start`'s
/// knowledge, and two things that both believe they know what a store is will one day disagree.
/// **The recipe supplies the supervisor**; this arm supplies the signal and refuses to fire when
/// firing would take the quorum. If nothing brings stores back, the run stops early and says so,
/// which is a true statement about the cluster rather than fourteen shots at nothing.
pub(crate) fn chaos(options: &DurabilityOptions) -> Result<String, String> {
    let named = live_pids(options)?;
    if named.len() < 3 {
        return Err(format!(
            "{} store processes were named; a cluster this kills from needs at least three, so \
             that taking one never takes the quorum",
            named.len()
        ));
    }
    let (transport, resolver) = crate::bench_route::routed(&options.pd)?;
    let client = TxnClient::new(transport, resolver, oracle(&options.pd)?);

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
    let mut refused = 0u64;
    // What the state file named last round, so a skipped round can say whether the pids moved.
    let mut previous: Vec<u32> = Vec::new();
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
        let at = began.elapsed().as_millis();
        // **Healthy first.** A cluster that cannot commit has already lost a store nobody brought
        // back, and another kill would only make the log longer.
        if let Err(reason) = probe(&client, &options.keyspace, killed) {
            refused += 1;
            // **What was observed, and not why.** This line used to end "Nothing is restarting
            // stores — the recipe needs a supervisor", which on run 120 was false in both
            // halves: the supervisor restarted a store four times, and one of the kills that
            // *did* land hit a pid it had produced. A tool that guesses a cause is worse than
            // one that reports a fact, because the guess is what gets quoted. So the line says
            // what the state file says, and whether it moved since the last round — which is
            // the evidence for "is anything restarting these" and is cheap to look at.
            let pids = live_pids(options);
            let stores = match &pids {
                Ok(now) if now.is_empty() => "the state file names no stores".to_owned(),
                Ok(now) if *now == previous => format!("the same stores as last round: {now:?}"),
                Ok(now) => format!("stores {now:?}, which is not last round's {previous:?}"),
                Err(reason) => format!("the state file could not be read ({reason})"),
            };
            if let Ok(now) = pids {
                previous = now;
            }
            log.push(format!(
                "{at:>8} ms  no kill: the cluster could not acknowledge a write ({reason}); \
                 {stores}"
            ));
            continue;
        }
        // **Re-read every round**, because a store the supervisor restarted has a new pid and a
        // list taken once names corpses after the first kill.
        let pids = match live_pids(options) {
            Ok(pids) if !pids.is_empty() => pids,
            Ok(_) => {
                log.push(format!(
                    "{at:>8} ms  no kill: the state file names no stores"
                ));
                continue;
            }
            Err(reason) => {
                log.push(format!("{at:>8} ms  no kill: {reason}"));
                continue;
            }
        };
        previous.clone_from(&pids);
        let pid = pids[usize::try_from(rng.next_u32()).unwrap_or(0) % pids.len()];
        let outcome = kill9(pid);
        killed += u64::from(outcome.is_ok());
        log.push(format!(
            "{at:>8} ms  SIGKILL pid {pid}  {}",
            match &outcome {
                Ok(()) => "sent".to_owned(),
                Err(reason) => format!("refused: {reason}"),
            }
        ));
    }
    Ok(format!(
        "{killed} kills, {refused} withheld because the cluster was not serving, in {:?}\n{}",
        began.elapsed(),
        log.join("\n"),
    ))
}

/// The store pids to choose from: the state file when one was given, else `--pids`.
///
/// `id address pid`, one node per line, written by `esker cluster start` and **rewritten when it
/// restarts a store**. A line with id 0 is a placement driver and not a store, so it is skipped —
/// killing the driver is a different experiment and `chaos` is not it. *Every* such line, not the
/// first: `--pd-nodes N` writes one per member (ADR 0108), and a filter that took "the driver" to
/// mean line one would start killing drivers two and three as if they were stores.
fn live_pids(options: &DurabilityOptions) -> Result<Vec<u32>, String> {
    let Some(path) = &options.state else {
        return Ok(options.pids.clone());
    };
    let text = std::fs::read_to_string(path).map_err(|error| format!("reading {path}: {error}"))?;
    let mut pids = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [id, _address, pid] = fields[..] else {
            return Err(format!(
                "{path} has a line that is not `id address pid`: {line}"
            ));
        };
        if id == "0" {
            continue;
        }
        pids.push(
            pid.parse::<u32>()
                .map_err(|error| format!("{path} names a pid that is not a number: {error}"))?,
        );
    }
    Ok(pids)
}

/// Commits one key, to ask whether the cluster is still serving.
///
/// A write and not a read: a read can be answered by a replica that has not noticed the loss yet,
/// and what the next kill must not do is take the quorum a *write* needs.
fn probe(client: &TxnClient, keyspace: &str, round: u64) -> Result<(), String> {
    let mut txn = client
        .begin()
        .map_err(|error| format!("no snapshot: {error}"))?;
    txn.put(format!("{keyspace}/probe/{round:08}").as_bytes(), b"probe");
    txn.commit().map_err(|error| format!("{error}")).map(|_| ())
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
