//! `esker cluster start --pd-nodes 3` starts a placement-driver **group**, not three lone drivers.
//!
//! Three processes that each founded a group of one would look identical from the outside — three
//! ports answering, three lines in the state file, stores registering — and would survive losing
//! none of them. So what this asserts is the thing that distinguishes a group from three
//! strangers: they agree on **one group id**, they name **each other** as members, and exactly one
//! of them says it leads.
//!
//! This is also two of `docs/plans/phase-15-pd-ha.md` §11.10's owed items, paid by construction
//! rather than by a test written for them: three drivers as three **processes** rather than three
//! in one, and a real socket between them.
//!
//! # The state file, and why nothing that reads it had to change
//!
//! A driver's line has id `0`. With three of them there are three such lines, so zero means *a*
//! driver and the **address** is what tells them apart
//! ([ADR 0108](../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)).
//! `esker durability chaos` skips every `id == "0"` line rather than the first, and
//! `esker-rails-harness/leader-kill.py` matches a census `store=N` against `id == N`, so neither
//! sees a driver at all. This file asserts that shape directly, because it is what those two
//! depend on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// One store, because what is under test is the drivers. A store is here at all so that the
/// command takes the path a real cluster takes — the stores are given the whole group and have to
/// register with whichever member leads it.
const NODES: u64 = 1;

/// Three drivers: the smallest group that survives losing one.
const DRIVERS: u64 = 3;

/// A generous ceiling on a cluster of four processes coming up under a loaded gate. The wait below
/// measures progress, not this; this is only ever spent on a run that is already wrong.
const CEILING: Duration = Duration::from_secs(240);

/// This file's own port band, for the reason `cluster_start.rs` gives at length: binding a run,
/// releasing it and returning the base races whoever binds next, and the only thing that makes
/// that harmless between test binaries is that each scans a different band.
fn free_port_run() -> u16 {
    let span = usize::try_from(NODES + DRIVERS).unwrap();
    for base in (31_100_u16..32_000).step_by(span) {
        let bound: Vec<TcpListener> = (0..span)
            .filter_map(|at| {
                let offset = u16::try_from(at).ok()?;
                TcpListener::bind(("127.0.0.1", base.checked_add(offset)?)).ok()
            })
            .collect();
        if bound.len() == span {
            return base;
        }
    }
    panic!("no run of {span} consecutive free ports in 31,100–32,000, this file's own band");
}

/// The supervisor, stopped however the test ends.
struct Supervisor(Child);

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// What `esker pd members` says, asked at one member's address.
fn members(address: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_esker-cli"))
        .arg("pd")
        .arg("members")
        .arg("--pd")
        .arg(address)
        .output()
        .expect("`pd members` runs");
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The state file's lines as `(id, address)`, or `None` until the supervisor has written it.
fn state(data_dir: &Path) -> Option<Vec<(u64, String)>> {
    let text = std::fs::read_to_string(data_dir.join("cluster.state")).ok()?;
    let rows: Vec<(u64, String)> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split_whitespace();
            let id: u64 = fields.next().unwrap().parse().unwrap();
            (id, fields.next().unwrap().to_owned())
        })
        .collect();
    (!rows.is_empty()).then_some(rows)
}

fn said(log: &Path) -> String {
    std::fs::read_to_string(log).unwrap_or_default()
}

/// **The whole assertion**: three drivers that found one group, and a state file whose shape every
/// reader of it already expects.
#[test]
fn three_drivers_found_one_group_and_one_of_them_leads() {
    let data_dir = TempDir::new().unwrap();
    let base_port = free_port_run();
    let log = data_dir.path().join("cluster.log");
    let out = std::fs::File::create(&log).expect("the cluster log");
    let errors = out.try_clone().expect("the cluster log");

    let mut supervisor = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_esker-cli"))
            .arg("cluster")
            .arg("start")
            .arg("--nodes")
            .arg(NODES.to_string())
            .arg("--data-dir")
            .arg(data_dir.path())
            .arg("--base-port")
            .arg(base_port.to_string())
            .arg("--pd")
            .arg("--pd-nodes")
            .arg(DRIVERS.to_string())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("the cluster command starts"),
    );

    // The state file is written only after every child is serving, so its appearance *is* the
    // readiness signal — and it is the thing under test, so there is nothing circular in waiting
    // for it.
    let began = Instant::now();
    let rows = loop {
        if let Some(status) = supervisor.0.try_wait().expect("waiting on the command") {
            panic!(
                "`esker cluster start --pd-nodes {DRIVERS}` exited with {status} instead of \
                 supervising a cluster. What it said:\n{}",
                said(&log)
            );
        }
        if let Some(rows) = state(data_dir.path()) {
            break rows;
        }
        assert!(
            began.elapsed() < CEILING,
            "the cluster never wrote its state file in {CEILING:?}. What it said:\n{}",
            said(&log)
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    // **The shape `durability chaos` and `leader-kill.py` read.** Three driver lines, all id zero,
    // all on different addresses; the store keeps its own id.
    let drivers: Vec<&(u64, String)> = rows.iter().filter(|(id, _)| *id == 0).collect();
    assert_eq!(
        drivers.len() as u64,
        DRIVERS,
        "the state file does not name {DRIVERS} placement drivers: {rows:?}"
    );
    let mut addresses: Vec<&str> = drivers.iter().map(|(_, a)| a.as_str()).collect();
    addresses.sort_unstable();
    addresses.dedup();
    assert_eq!(
        addresses.len() as u64,
        DRIVERS,
        "two drivers were written on one address, so nothing can tell them apart: {rows:?}"
    );
    let stores: Vec<&(u64, String)> = rows.iter().filter(|(id, _)| *id != 0).collect();
    assert_eq!(
        stores.len() as u64,
        NODES,
        "the store lines are not what a chaos arm would find: {rows:?}"
    );

    // **One group, not three.** Every member is asked, and each has to name the same group id, the
    // same leader, and all three members — which is exactly what three drivers that each founded a
    // group of one would fail.
    let answers: Vec<String> = drivers
        .iter()
        .map(|(_, address)| members(address))
        .collect();
    for (at, answer) in answers.iter().enumerate() {
        for (_, address) in &drivers {
            assert!(
                answer.contains(address.as_str()),
                "driver {} does not name {address} as a member, so this is not one group:\n{answer}",
                at + 1
            );
        }
    }
    // `pd members` prints `group 0x…, term N`, so the id carries the comma the line puts after it.
    let group_ids: Vec<&str> = answers
        .iter()
        .filter_map(|answer| {
            answer
                .split_whitespace()
                .find(|word| word.starts_with("0x"))
                .map(|word| word.trim_end_matches(','))
        })
        .collect();
    assert_eq!(
        group_ids.len(),
        answers.len(),
        "a member did not report a group id at all:\n{}",
        answers.join("\n---\n")
    );
    assert!(
        group_ids.windows(2).all(|pair| pair[0] == pair[1]),
        "the members report different group ids, so they founded different groups: {group_ids:?}"
    );
    assert!(
        answers.iter().any(|answer| answer.contains("leader")),
        "no member says who leads:\n{}",
        answers.join("\n---\n")
    );

    // Stopped here rather than only by `Drop`, so a failure above still leaves the ports free for
    // the next run of this binary.
    let _ = supervisor.0.kill();
    let _ = supervisor.0.wait();
}
