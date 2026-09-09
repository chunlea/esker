//! A placement driver, a store and a SQL node, started from the real binaries.
//!
//! The cluster construction is **copied** from `tests/pgwire_cluster.rs` rather than shared with
//! it, which is the same choice `esker-sql/tests/routing_differential.rs` records: that file
//! belongs to another lane and this one does not edit it. What is copied is the process
//! management; what is new is a store started with a small region-split threshold, and a way to
//! ask how many regions a table ended up in.
//!
//! Raw protocol bytes rather than a client library, for `pgwire_cluster.rs`'s reason: this crate
//! has no pgwire client, and a test that shells out to `psql` skips on a machine without one —
//! which is exactly how a release shipped a node that could not serve a single connection.

#![allow(dead_code, unreachable_pub)]

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long a child has to open its port.
const STARTUP_SECONDS: u64 = 120;

/// A child killed when the test ends, however it ends.
struct Supervisor(Child);

/// A child's stdout and stderr, into one file named after it.
///
/// **Both streams**, because `tracing_subscriber::fmt()` writes to stdout and a panic to stderr,
/// and a harness that captured one of them reports an empty file for half the failures there are.
/// This lane learned that with `bench-mpp` and repeated it here; the file is what turns "the test
/// hung" into "the store said why".
fn log_into(dir: &std::path::Path, what: &str) -> (Stdio, Stdio) {
    let path = dir.join(format!("{what}.log"));
    match std::fs::File::create(&path) {
        Ok(out) => match out.try_clone() {
            Ok(err) => (Stdio::from(out), Stdio::from(err)),
            Err(_) => (Stdio::null(), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    }
}

/// The tail of every child's log, for a failure that needs to name which process caused it.
fn tails(dir: &std::path::Path) -> String {
    let mut said = String::new();
    for what in ["pd", "store", "sql"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(format!("{what}.log"))) {
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(8)..].join("\n    ");
            let _ = write!(said, "\n  {what} said:\n    {tail}");
        }
    }
    said
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The processes, and where to reach them.
pub struct Cluster {
    dir: tempfile::TempDir,
    _store: Supervisor,
    _others: Vec<Supervisor>,
    _pd: Supervisor,
    _sql: Supervisor,
    pd_port: u16,
    sql_port: u16,
}

impl Cluster {
    /// Starts a driver, one store with `split_size` as its region threshold, and a SQL node.
    ///
    /// One store, because a split is the *leader's* own decision and needs only PD's id allocator
    /// — the replica target is another matter and not what this harness is about.
    pub fn start(split_size: u64) -> Self {
        Self::start_with(1, split_size)
    }

    /// Starts `stores` stores, a driver and a SQL node.
    ///
    /// **Four is the minimum that can hold a columnar learner** — a region has three voters and PD
    /// places a learner on a store with no peer of that region — and it is also what makes a SQL
    /// node see more than one *shard*, which is what the region-count guard turns on.
    pub fn start_with(stores: u16, split_size: u64) -> Self {
        for attempt in 1..=4 {
            if let Some(cluster) = Self::try_start(stores, split_size) {
                return cluster;
            }
            eprintln!("harness: attempt {attempt} lost a port before its child bound; retrying");
        }
        panic!("four starts in a row lost a port before a child could bind");
    }

    /// One attempt, or `None` if a child lost its port.
    ///
    /// Long, and deliberately not split: it is sequential process setup where every step depends
    /// on the last, and two attempts at cutting it produced worse code than the length it saved.
    #[allow(clippy::too_many_lines)]
    fn try_start(stores: u16, split_size: u64) -> Option<Self> {
        let dir = tempfile::TempDir::new().unwrap();
        eprintln!("harness: logs in {}", dir.path().display());
        let base = free_ports(stores + 2);
        let (store_port, pd_port, sql_port) = (base, base + stores, base + stores + 1);
        warm(esker_cli());
        warm(esker_sql());

        let (out, err) = log_into(dir.path(), "pd");
        let mut pd = Supervisor(
            Command::new(esker_cli())
                .args(["pd", "serve", "--data-dir"])
                .arg(dir.path().join("pd"))
                .args(["--listen", &format!("127.0.0.1:{pd_port}")])
                .stdout(out)
                .stderr(err)
                .spawn()
                .expect("the placement driver starts"),
        );
        if !wait_for_port("the driver", pd_port, &mut pd, STARTUP_SECONDS, dir.path()) {
            return None;
        }

        let mut others: Vec<Supervisor> = Vec::new();
        let mut store: Option<Supervisor> = None;
        for id in 1..=stores {
            let (store_out, store_err) = log_into(dir.path(), &format!("store{id}"));
            let mut spawn = Command::new(esker_cli());
            spawn
                .args(["server", "--data-dir"])
                .arg(dir.path().join(format!("store{id}")))
                .args(["--listen", &format!("127.0.0.1:{}", store_port + id - 1)])
                .args(["--store-id", &id.to_string()])
                .args(["--pd", &format!("127.0.0.1:{pd_port}")])
                .args(["--region-split-size", &split_size.to_string()])
                // Placement costs one region heartbeat an operator, so a test that waits for a
                // learner shortens it rather than waiting a minute each.
                .args(["--region-heartbeat-ms", "2000"])
                .args(["--heartbeat-tick-ms", "500"]);
            // `--peer` as well as `--pd`, which is what `crate::cluster` does: without it PD's
            // `AddPeer` has no address to reach and times out at its ceiling, for ever.
            for peer in 1..=stores {
                spawn.args([
                    "--peer",
                    &format!("{peer}@127.0.0.1:{}", store_port + peer - 1),
                ]);
            }
            let mut child = Supervisor(
                spawn
                    .stdout(store_out)
                    .stderr(store_err)
                    .spawn()
                    .expect("the store starts"),
            );
            if !wait_for_port(
                &format!("store {id}"),
                store_port + id - 1,
                &mut child,
                STARTUP_SECONDS,
                dir.path(),
            ) {
                return None;
            }
            if store.is_none() {
                store = Some(child);
            } else {
                others.push(child);
            }
        }
        let store = store.expect("at least one store");

        let (sql_out, sql_err) = log_into(dir.path(), "sql");
        let mut sql = Supervisor(
            Command::new(esker_sql())
                .arg(format!("127.0.0.1:{sql_port}"))
                .args((1..=stores).map(|id| format!("127.0.0.1:{}", store_port + id - 1)))
                .args(["--pd", &format!("127.0.0.1:{pd_port}")])
                .stdout(sql_out)
                .stderr(sql_err)
                .spawn()
                .expect("`esker-sql` runs"),
        );
        wait_for_port(
            "the SQL node",
            sql_port,
            &mut sql,
            STARTUP_SECONDS,
            dir.path(),
        );

        let cluster = Cluster {
            dir,
            _store: store,
            _others: others,
            _pd: pd,
            _sql: sql,
            pd_port,
            sql_port,
        };
        // **A node that answers, not a port that accepts.** A SQL node takes a connection before
        // it holds a schema lease, and every write until it does is `25006`.
        let deadline = Instant::now() + Duration::from_secs(STARTUP_SECONDS);
        loop {
            let answer = cluster.query("SELECT 1");
            if rows_of(&answer) == vec!["1".to_owned()] {
                return Some(cluster);
            }
            if Instant::now() >= deadline {
                eprintln!("harness: the SQL node never answered `SELECT 1`: {answer}");
                return None;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Runs a statement and returns the server's reply as text, errors included.
    pub fn query(&self, sql: &str) -> String {
        one_query(self.sql_port, sql)
    }

    /// Runs a statement on a named engine.
    ///
    /// The `SET` travels **in the same simple-query message**, because every call here opens its
    /// own connection and session state would not survive one.
    pub fn query_on(&self, engine: &str, sql: &str) -> String {
        one_query(
            self.sql_port,
            &format!("SET esker.engine = '{engine}'; {sql}"),
        )
    }

    /// Runs a statement and fails the test if the server refused it.
    ///
    /// **`25006` is waited out rather than failed on.** A SQL node takes connections before it
    /// holds a schema lease, and until it does every write is *"this node's schema lease has
    /// expired and the placement driver is unreachable"*. It is a startup race, not a refusal —
    /// `esker bench-mpp` learned it the same way, six runs in — so readiness here means a node
    /// that accepts a **write**, not one that answers.
    ///
    /// # And three more, each measured off a gate log rather than guessed
    ///
    /// `multi_region_differential` loads 8 KiB rows until the table splits, so its `INSERT`s run
    /// *while* regions are being cut and re-elected. That reddened the gate six times in one
    /// night, always green on the rerun, and the three failures are these:
    ///
    /// ```text
    /// 40003 the transaction's outcome is unknown: the TxnPrewrite may or may not have been
    ///       applied: connection closed: region 1 stopped leading with this proposal in its log
    /// 08006 could not reach the store: deadline passed after 14 attempts
    /// 08006 could not reach the store: gave up after 9 attempts: peer is not the leader of
    ///       region 1
    /// ```
    ///
    /// **All three say "not now", and none of them says "not ever".** A region mid-election has no
    /// leader to prewrite against and a client that has spent its attempts says so; the answer a
    /// second later is a commit. This is the same reading h1 landed inside the client — wait the
    /// *caller's* deadline instead of a hard-coded number of attempts — applied at the layer that
    /// has a deadline to give, and it is a **bounded** wait rather than a loop: past the deadline
    /// the assertion fires with the server's own words, so a store that is really gone still fails
    /// the test.
    ///
    /// **Why this is waiting and not a weaker assertion.** Nothing about what the statement must
    /// *do* is relaxed: every row still has to be written, and the test's `assert_eq!` on the rows
    /// is untouched. What changes is that the harness stops treating "the cluster is busy
    /// splitting" — a state it deliberately provokes — as a defect in the thing it is measuring.
    ///
    /// # Retrying `40003` is only safe because these writes carry their own key
    ///
    /// An unknown outcome may have applied. Retrying one is a **double write** unless the write is
    /// idempotent, and here it is: every `INSERT` this harness sends names its `id`, which is the
    /// table's primary key, so a retry either writes the row or hits `23505` on the row its own
    /// first attempt wrote. That second case is a **success** and is treated as one — which is
    /// also why a `23505` is only accepted *after* an unknown outcome and not in general, since a
    /// duplicate key on the first attempt is a real defect in the fixture.
    pub fn run(&self, sql: &str) {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut outcome_was_unknown = false;
        loop {
            let answer = self.query(sql);
            if !answer.contains("ERROR") {
                return;
            }
            // The retry finding its own earlier attempt applied. See the doc above: accepted only
            // after an unknown outcome, because otherwise it is the fixture writing a row twice.
            if outcome_was_unknown && answer.contains("23505") {
                eprintln!("harness: `{sql}` had already applied; the unknown outcome was a commit");
                return;
            }
            outcome_was_unknown |= answer.contains("40003");
            assert!(
                Self::waited_out(&answer) && Instant::now() < deadline,
                "`{sql}` was refused: {}",
                answer.trim()
            );
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Whether a refusal is one of the four transient states [`Cluster::run`] waits out.
    ///
    /// Listed by **sqlstate and sentence**, not by sqlstate alone: `08006` is "could not reach the
    /// store" for a region being re-elected and also for a store that has died, and only the
    /// sentence tells them apart. A state not named here fails the test on its first appearance,
    /// which is the property this list exists to keep — a harness that waited out every error
    /// would wait out the defect it was built to find.
    pub fn waited_out(answer: &str) -> bool {
        // A node that has connections and not yet a schema lease.
        answer.contains("25006")
            // A prewrite whose region stopped leading mid-proposal. Idempotent to retry here; see
            // `run`'s doc for why that is true of this harness's writes and not in general.
            || answer.contains("40003")
            // A client that spent its attempts on a region with no leader yet. Both spellings are
            // from gate logs: one ran out of deadline, one ran out of attempts.
            || (answer.contains("08006")
                && (answer.contains("deadline passed after")
                    || answer.contains("peer is not the leader")))
    }

    /// How many regions the cluster holds, from the placement driver's own routing table.
    pub fn regions(&self) -> usize {
        self.region_lines().count()
    }

    /// How many regions have a columnar learner, from PD's own routing table.
    pub fn regions_with_a_learner(&self) -> usize {
        self.region_lines()
            .filter(|line| line.contains('C'))
            .count()
    }

    /// Waits until the cluster holds more than one region, and answers how many.
    ///
    /// **A wait, not an assertion.** A split is the leader's own decision, taken on its region
    /// heartbeat after the size estimate crosses the threshold — so "did it split" straight after
    /// a load is a question about timing, and under a loaded machine the answer is "not yet".
    /// Asserting it there made this test fail in a full gate and pass alone.
    pub fn wait_for_a_split(&self, seconds: u64) -> usize {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let regions = self.regions();
            if regions > 1 {
                eprintln!("harness: the table is in {regions} regions");
                return regions;
            }
            assert!(
                Instant::now() < deadline,
                "the table did not split within {seconds}s, so nothing here is about a boundary"
            );
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// Waits until every region's learner **answers a fragment**, not merely exists.
    ///
    /// **Placement is not readiness.** PD places a learner and the learner then catches up; a
    /// fragment sent in between is refused `TooFarBehind` and the planner answers from the rows —
    /// correctly, and silently. A test that started comparing there would be comparing the row
    /// engine with itself and calling the agreement evidence.
    ///
    /// The observable is the plan's own `Fragments: N asked, N answered`, which is the only place
    /// that distinguishes *asked and refused* from *asked and answered*. `Engine: rows` here is
    /// not a failure of the query — it is the fragment path not being ready yet, which is why the
    /// message says `placement never completed` rather than blaming the answer.
    pub fn wait_until_fragments_answer(&self, probe: &str, seconds: u64) {
        assert!(
            self.columnar_within(probe, seconds),
            "placement never completed: no fragment answered within {seconds}s, so \
             \"columnar agreed\" could only have meant \"columnar never ran\". The last plan \
             was:\n{}",
            self.query_on("auto", &format!("EXPLAIN ANALYZE {probe}"))
        );
    }

    /// Whether **this** query's fragments answer within the bound — `true` when the plan says the
    /// columns ran, `false` on the timeout, and no panic either way.
    ///
    /// **Readiness is per query, not per table**, which is the second half of the same night's
    /// gate flakes. `multi_region_differential` waited once, on `SELECT count(*)`, and then
    /// asserted `Engine: columnar` for every query it compares — so a `min`/`max` whose fragment
    /// was still catching up made the *declaration* assertion fail and read as
    /// "the columns had to answer this and did not", which is a readiness message wearing a
    /// correctness message's clothes. A learner catches up per column family and per region, so
    /// `count(*)` answering says nothing about `min(amount)`.
    ///
    /// **`Engine: columnar` is the readiness condition, and it is the only one that covers both
    /// halves.** A plan that was never routed carries no `Fragments:` line at all — the planner's
    /// own region cache had not yet seen a learner — and one that was routed and refused carries
    /// `N asked, 0 answered`. Waiting on the fragment counts alone waits for ever in the first
    /// case, which is what it did.
    ///
    /// The caller decides what a timeout means. [`Cluster::wait_until_fragments_answer`] makes it
    /// a failure, because it is asked once about the table and nothing can be compared until the
    /// columns answer *something*; the per-query caller makes it a **deferral** and keeps
    /// comparing the answers, because the row engine is always there and the answer being right is
    /// the assertion that must not be waited out.
    pub fn columnar_within(&self, probe: &str, seconds: u64) -> bool {
        self.columnar_plan_within(probe, seconds).is_some()
    }

    /// The same wait, **handing back the plan it saw** — which is what a caller that then asserts
    /// on a plan has to use.
    ///
    /// `columnar_within` answering `true` and the caller running its own `EXPLAIN ANALYZE`
    /// afterwards is two observations of a state that is **not monotone**: a learner that has
    /// caught up can fall behind again under load, and then the second plan says `Engine: rows`
    /// for a query the first one had ready. That is what reddened
    /// `multi_region_differential` at load 12 after the bounded wait landed — the wait worked and
    /// the re-plan raced it.
    ///
    /// So the observation is taken once. The bound and the answer assertions are untouched: this
    /// removes a second sample, not a comparison.
    pub fn columnar_plan_within(&self, probe: &str, seconds: u64) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let plan = self.query_on("auto", &format!("EXPLAIN ANALYZE {probe}"));
            if plan.contains("Engine: columnar") {
                return Some(plan);
            }
            if Instant::now() >= deadline {
                eprintln!("harness: `{probe}` was still on the rows after {seconds}s");
                return None;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// Waits until every region has a columnar learner.
    ///
    /// A series and not a reading: placement costs one region heartbeat an operator and PD does
    /// one region at a time, so a count that climbs was latency and a single sample cannot tell
    /// that from a count that sits.
    pub fn wait_for_learners(&self, seconds: u64) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            let (regions, with) = (self.regions(), self.regions_with_a_learner());
            if regions > 0 && with == regions {
                eprintln!("harness: {with} of {regions} regions have a columnar learner");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "only {with} of {regions} regions got a columnar learner within {seconds}s"
            );
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// One line per region from `region ls`, the header and trailer dropped.
    fn region_lines(&self) -> std::vec::IntoIter<String> {
        let listed = Command::new(esker_cli())
            .args([
                "region",
                "ls",
                "--pd",
                &format!("127.0.0.1:{}", self.pd_port),
            ])
            .output()
            .expect("`region ls` runs");
        String::from_utf8_lossy(&listed.stdout)
            .lines()
            .filter(|line| {
                line.split_whitespace()
                    .next()
                    .is_some_and(|first| first.parse::<u64>().is_ok())
            })
            .map(str::to_owned)
            .collect::<Vec<_>>()
            .into_iter()
    }
}

/// The values of every `DataRow` in a reply, in order.
pub fn rows_of(answer: &str) -> Vec<String> {
    answer
        .lines()
        .filter_map(|line| line.strip_prefix("row: "))
        .map(str::to_owned)
        .collect()
}

/// Connects, completes the startup exchange, runs one statement, and renders the reply.
///
/// `row: <first column>` for each `DataRow` and `ERROR <code>: <message>` for a refusal — enough
/// for a test to assert on without a decoder this crate does not have.
fn one_query(port: u16, sql: &str) -> String {
    let Ok(mut socket) = TcpStream::connect(("127.0.0.1", port)) else {
        return "ERROR: the SQL node did not accept a connection".to_owned();
    };
    let _ = socket.set_read_timeout(Some(Duration::from_secs(120)));
    let mut startup = Vec::new();
    startup.extend_from_slice(&196_608_i32.to_be_bytes());
    for (key, value) in [("user", "esker"), ("database", "esker")] {
        startup.extend_from_slice(key.as_bytes());
        startup.push(0);
        startup.extend_from_slice(value.as_bytes());
        startup.push(0);
    }
    startup.push(0);
    let mut out = Vec::new();
    out.extend_from_slice(&(i32::try_from(startup.len() + 4).unwrap()).to_be_bytes());
    out.extend_from_slice(&startup);
    if socket.write_all(&out).is_err() {
        return "ERROR: the startup packet was refused".to_owned();
    }
    let mut query = vec![b'Q'];
    query.extend_from_slice(&(i32::try_from(sql.len() + 5).unwrap()).to_be_bytes());
    query.extend_from_slice(sql.as_bytes());
    query.push(0);
    if socket.write_all(&query).is_err() {
        return "ERROR: the query was refused".to_owned();
    }
    render(read_messages(&mut socket))
}

/// Every tagged message the server sent before the second `ReadyForQuery`.
fn read_messages(socket: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut buffered: Vec<u8> = Vec::new();
    let mut messages = Vec::new();
    let mut ready = 0;
    let mut chunk = [0u8; 8192];
    loop {
        let mut at = 0;
        while buffered.len() >= at + 5 {
            let length = i32::from_be_bytes([
                buffered[at + 1],
                buffered[at + 2],
                buffered[at + 3],
                buffered[at + 4],
            ]);
            let Ok(body) = usize::try_from(length - 4) else {
                return messages;
            };
            if buffered.len() < at + 5 + body {
                break;
            }
            let tag = buffered[at];
            messages.push((tag, buffered[at + 5..at + 5 + body].to_vec()));
            if tag == b'Z' {
                ready += 1;
            }
            at += 5 + body;
        }
        buffered.drain(..at);
        // Two: the startup burst ends with one, and the statement's answer with the next.
        if ready >= 2 {
            return messages;
        }
        match socket.read(&mut chunk) {
            Ok(0) | Err(_) => return messages,
            Ok(read) => buffered.extend_from_slice(&chunk[..read]),
        }
    }
}

/// The messages a test cares about, one per line.
fn render(messages: Vec<(u8, Vec<u8>)>) -> String {
    let mut out = String::new();
    for (tag, body) in messages {
        match tag {
            b'D' => {
                let mut at = 2;
                let value = if body.len() >= 6 {
                    let length =
                        i32::from_be_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
                    at += 4;
                    match usize::try_from(length) {
                        Ok(width) if body.len() >= at + width => {
                            String::from_utf8_lossy(&body[at..at + width]).into_owned()
                        }
                        _ => String::new(),
                    }
                } else {
                    String::new()
                };
                let _ = writeln!(out, "row: {value}");
            }
            b'E' => {
                let mut code = String::new();
                let mut message = String::new();
                for field in body.split(|byte| *byte == 0) {
                    match field.first() {
                        Some(b'C') => code = String::from_utf8_lossy(&field[1..]).into_owned(),
                        Some(b'M') => message = String::from_utf8_lossy(&field[1..]).into_owned(),
                        _ => {}
                    }
                }
                let _ = writeln!(out, "ERROR {code}: {message}");
            }
            _ => {}
        }
    }
    out
}

fn esker_cli() -> PathBuf {
    binary("esker-cli")
}

fn esker_sql() -> PathBuf {
    binary("esker-sql")
}

/// A sibling binary of this test's own executable.
fn binary(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().expect("this test has a path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(name)
}

/// Runs `--version` once, so the first real spawn is not also paying for a cold page cache.
fn warm(binary: PathBuf) {
    let _ = Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// A run of `span` consecutive free ports.
///
/// **Bind one and probe upward.** Binding `span` ephemeral ports and hoping they come out
/// consecutive is what this did first, and three arbitrary ports are consecutive about never — so
/// it spun for ever before a single process started, and every failure looked like a hung cluster.
/// The kernel picks the first port; the rest are checked, and the whole run is held until the
/// children have them.
fn free_ports(span: u16) -> u16 {
    for _ in 0..256 {
        let Ok(first) = TcpListener::bind("127.0.0.1:0") else {
            continue;
        };
        let Ok(base) = first.local_addr().map(|addr| addr.port()) else {
            continue;
        };
        if base.checked_add(span).is_none() {
            continue;
        }
        let held: Vec<TcpListener> = (1..span)
            .filter_map(|step| TcpListener::bind(("127.0.0.1", base + step)).ok())
            .collect();
        if held.len() == usize::from(span - 1) {
            return base;
        }
    }
    panic!("no run of {span} consecutive free ports after 256 attempts");
}

/// Waits for a port to accept, failing early if the child has already exited.
fn wait_for_port(
    what: &str,
    port: u16,
    child: &mut Supervisor,
    seconds: u64,
    dir: &std::path::Path,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Ok(Some(status)) = child.0.try_wait() {
            // **Not a panic: a lost port is a retry, not a result.** `free_ports` reserves a run
            // and releases it before the children bind, so a cluster test starting beside another
            // can lose one in that window — the child exits, and panicking here reports a harness
            // race as a product failure. It did, twice, in one gate.
            eprintln!("harness: {what} exited with {status} before listening on {port}");
            return false;
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            eprintln!("harness: {what} is listening on {port}");
            return true;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "harness: {what} did not listen on {port} within {seconds}s{}",
                tails(dir)
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
