//! Is it **listening**, and is it the thing that was started? — `esker cluster start`'s probes.
//!
//! Split out of `super` on 2026-09-11, when that file reached 1,371 lines against `CLAUDE.md`'s
//! "~800". It is a move and not a redesign: every line here was in `cluster.rs` and does the same
//! thing, so a reader chasing a behaviour change through this commit will find none.
//!
//! # Why these belong together and apart from the supervisor
//!
//! Supervising a child is about a process: start it, notice it died, put it back. **Readiness is
//! about a port**, and the difference is the whole reason this file exists — every one of these
//! probes was written after a version that watched the process instead and was wrong:
//!
//! * "started" is not "listening": three stores of four exited with `Connection refused` before
//!   the driver had bound, and the cluster announced four nodes;
//! * "listening" is not "answering": a squatter on the driver's port satisfied a bare
//!   `TcpStream::connect`, and `cluster start` spawned four stores against somebody else's
//!   process;
//! * "nothing has failed yet" is not "everything started", which is a negative assertion behind a
//!   wall clock and passes for a store that has not finished opening.
//!
//! Each of those is a test below, and each of them is red against the version it replaced.

use std::net::SocketAddr;
use std::process::Child;
use std::time::{Duration, Instant};

use esker_proto::TransportConfig;

use super::{Node, Supervised, what};
use crate::readiness::Budget;

/// How long a placement driver has to start listening before this gives up on it.
///
/// Generous: it opens a database first. Short enough that a driver which is never going to bind —
/// a port already taken, a directory it cannot write — is an error rather than a hang.
pub(super) const PD_START_TIMEOUT: Duration = Duration::from_secs(20);

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
pub(super) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Between two readiness probes. A round trip is the cost, so this polls rather than spins.
pub(super) const PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// How long the stores have to answer, **each of them, from the last one that did**.
///
/// Generous, because a store opens a database and — with `--pd` — registers with the driver
/// before it binds, and both are slower on a cold cache under load. Bounded, because a start
/// that never returns and never says why is worse than one that gives up with a name in it.
///
/// **Per store rather than for the set**, which is the difference between a bound and a rate.
/// One sixty-second clock for four stores is a claim about how fast this machine starts four
/// processes, and under a full gate that claim is false — `esker-cli::cluster_start` has gone red
/// here having watched three of its four come up. [`Budget`] spends this on each store and starts
/// it again whenever one arrives, with `60 s x stores` as the ceiling for a set that never
/// settles. [`PD_START_TIMEOUT`] is left alone: one driver is one thing waited on, and there is
/// no progress inside it to measure.
pub(super) const STORE_START_TIMEOUT: Duration = Duration::from_secs(60);

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
pub(super) fn wait_until_the_driver_answers(
    address: &str,
    child: &mut Child,
    within: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + within;
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
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(format!(
                "the placement driver did not answer on {address} within {within:?}: {refusal}"
            ));
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

/// One round trip that only a placement driver completes, or why it did not.
pub(super) fn ask_the_driver(address: &str) -> Result<(), String> {
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
///
/// # `within` is what one store gets, not what the set gets
///
/// It used to be one clock for all of them, which made the wait a claim about how fast this
/// machine starts four processes rather than about whether they started. [`Budget`] spends it per
/// store and starts it again each time one answers, so a start that is still bringing stores up is
/// not cut off; the ceiling underneath is what ends a set that never settles. The failure names
/// which limit it hit and how many stores had answered by then, because "did not answer within
/// 60s" was the same sentence for a store that never opened and for a machine that was busy.
pub(super) fn wait_until_the_stores_answer(
    children: &mut [Supervised],
    launched: &[Node],
    within: Duration,
) -> Result<(), String> {
    let mut waiting: Vec<(u64, String)> = launched
        .iter()
        .filter(|node| node.id != 0)
        .map(|node| (node.id, node.address.clone()))
        .collect();
    let stores = waiting.len();
    let mut budget = Budget::new(Instant::now(), within, stores);
    loop {
        // A child that has exited is the precise answer and it is available at once. Without this
        // arm a store that cannot open would spend the whole budget failing to connect, and the
        // message at the end would say "did not answer" where "exited with status 1" is the truth.
        if let Some(died) = first_child_that_died(children) {
            return Err(died);
        }
        let before = waiting.len();
        waiting.retain(|(_, address)| ask_a_store(address).is_err());
        if waiting.is_empty() {
            return Ok(());
        }
        // The probes themselves are what takes the time, so the clock is read after them and the
        // same reading decides both. A store that answered in this round is progress even though
        // others in it did not: what the budget is for is telling a slow start from a stuck one.
        let now = Instant::now();
        if waiting.len() < before {
            budget.progress(now);
        }
        if let Some(spent) = budget.spent(now) {
            let (id, address) = &waiting[0];
            return Err(format!(
                "{} did not answer on {address}: {spent} ({} of {stores} answered)",
                what(*id, address),
                stores - waiting.len(),
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
pub(super) fn ask_a_store(address: &str) -> Result<(), String> {
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
        Instant::now() + PROBE_TIMEOUT,
    ) {
        Ok(esker_proto::Response::Admin(_)) => Ok(()),
        Ok(other) => Err(format!(
            "{address} answered a store's question with {other:?}"
        )),
        Err(error) => Err(format!("asking {address} for its regions: {error}")),
    }
}

/// The first child that has already exited, described the way an operator needs it.
pub(super) fn first_child_that_died(children: &mut [Supervised]) -> Option<String> {
    children
        .iter_mut()
        .find_map(|one| match one.child.try_wait() {
            Ok(Some(status)) => Some(format!(
                "{} exited with {status}",
                what(one.id, &one.address)
            )),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use std::process::{Command as Process, Stdio};
    use std::time::Duration;

    use super::super::{DEFAULT_BASE_PORT, Node, Supervised, address_of};
    use super::{
        first_child_that_died, wait_until_the_driver_answers, wait_until_the_stores_answer,
    };
    use crate::testserver::{TestPd, TestServer};

    /// One supervised store, at the address `cluster start` would have given it.
    fn supervised(id: u64, child: std::process::Child) -> Supervised {
        Supervised {
            address: address_of(DEFAULT_BASE_PORT, id),
            id,
            child,
        }
    }

    /// One supervised placement driver: id zero, and its address is what names it.
    fn driver(child: std::process::Child) -> Supervised {
        Supervised {
            id: 0,
            address: "127.0.0.1:1".to_owned(),
            child,
        }
    }

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
        let mut children = vec![supervised(1, sleeper())];
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
        for mut one in children {
            let _ = one.child.kill();
        }
        assert!(error.contains("node 1"), "{error}");
        assert!(error.contains("did not answer"), "{error}");
    }

    /// And a store that **is** serving ends the wait, so the check cannot pass by never being
    /// satisfiable.
    #[test]
    fn the_wait_ends_when_every_store_answers() {
        let store = TestServer::start();
        let mut children = vec![supervised(1, sleeper())];
        let launched = vec![Node {
            id: 1,
            address: store.addr(),
            pid: 0,
        }];
        let answer = wait_until_the_stores_answer(&mut children, &launched, Duration::from_secs(5));
        for mut one in children {
            let _ = one.child.kill();
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
        let mut children = vec![supervised(1, dead)];
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
        let mut children = vec![driver(dead), supervised(1, sleeper())];
        let gone = first_child_that_died(&mut children).expect("the exited child was not noticed");
        // **Which driver**, not just "a driver": a cluster may have three of them and a line
        // that named none would send an operator to the wrong process.
        assert!(
            gone.starts_with("the placement driver on 127.0.0.1:1 exited with"),
            "{gone}"
        );
        for mut one in children {
            let _ = one.child.kill();
        }

        let mut alive = vec![supervised(1, sleeper()), supervised(2, sleeper())];
        assert!(first_child_that_died(&mut alive).is_none());
        for mut one in alive {
            let _ = one.child.kill();
        }
    }
}
