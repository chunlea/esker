//! A placement driver is added to, and removed from, a group of **three real processes**.
//!
//! This is the last of `docs/plans/phase-15-pd-ha.md` §11.10's owed items, and the plan is precise
//! about why it was owed:
//!
//! > **A membership change over a real socket.** Every test of the rewiring path either drives the
//! > inherent method directly or uses an in-process transport. Nothing has yet added a member to a
//! > group of three real processes and watched a message reach it.
//!
//! `esker cluster start --pd-nodes 3` made the *founding* half cheap ([ADR 0108](../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)).
//! What is left is the half an operator actually runs under pressure, and it is the one with the
//! most to find: three processes, a fourth started separately with `--join`, `pd members add`, and
//! then `pd members remove` to put the group back.
//!
//! # What is actually asserted
//!
//! Not "the command exited 0" — that is a claim about one process's opinion. **Every member is
//! asked afterwards**, including the one just added, and they must agree: the same group id, the
//! same four members, one leader. A member that had joined a group of its own, or that had been
//! added to the leader's configuration without the others learning of it, passes the exit code and
//! fails this.
//!
//! # The order is forced, and it is the opposite of the obvious one
//!
//! My first draft started the fourth process and *then* added it, which is what you would guess.
//! `--join` refuses that, and says so in the sentence it refuses with:
//!
//! > the group at {address} has no member 4; run `esker pd members add 4@<this member's --listen>`
//! > against it first
//!
//! A joining member asks the group for the id it cannot derive (ADR 0061), and a group that has
//! never heard of it has nothing to tell it. So the add comes first — and `add` proposes
//! `AddLearner`, then **waits for that learner to catch up**, which it cannot do while its process
//! is not running (§11.3). The two halves are therefore concurrent by design, which is what the
//! command's own budget assumes: 120 s, with *"run the same command again — it picks up where it
//! left off"* on the way out.
//!
//! This test does what an operator does with two shells: start the `add`, bring the member up
//! beside it, and let the `add` finish. Finding that by reading `join_group` rather than by
//! watching a two-minute hang is the argument for this item having been owed at all — nobody had
//! walked the sequence end to end.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const NODES: u16 = 1;
/// The group `cluster start` founds. The fourth member is added at run time, which is the point.
const DRIVERS: u16 = 3;
const STARTUP_SECONDS: u64 = 120;

#[test]
fn a_fourth_driver_joins_three_real_processes_and_then_leaves() {
    let data_dir = TempDir::new().unwrap();
    // One port for the store, three for the founded group, one for the member that joins.
    let base = free_ports(NODES + DRIVERS + 1);
    let store_port = base;
    let founded: Vec<String> = (0..DRIVERS)
        .map(|at| format!("127.0.0.1:{}", base + NODES + at))
        .collect();
    let joiner_port = base + NODES + DRIVERS;
    let joiner = format!("127.0.0.1:{joiner_port}");
    warm();

    let mut cluster = start_the_cluster(data_dir.path(), store_port);
    agree_on(&founded, DRIVERS as usize, "the founded group");

    // **The add starts first and is not waited on yet**, because it proposes `AddLearner` and then
    // blocks until that learner catches up — which needs the member's process. Against a member
    // rather than against the leader: an operator naming any member should not have to know which
    // one leads, and `ask_the_leader` follows the redirect.
    let adding = Command::new(esker_cli())
        .args(["pd", "members", "add", &format!("4@{joiner}")])
        .args(["--pd", &founded[0]])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("`pd members add` starts");

    // **Wait until the group has heard of it**, which is the `AddLearner` committing. Starting the
    // process before that is a race it loses: `--join` asks the group for its membership, is told
    // there is no member 4, and exits — and `wait_for_port` would then report the symptom ("the
    // fourth driver exited") rather than the ordering.
    wait_until_listed(&founded, &joiner);

    // **Now the member itself**, which can only join once the group knows its id — `--join` refuses
    // a group that has never heard of it, by name and with the remedy in the sentence.
    let mut fourth = Supervisor(
        Command::new(esker_cli())
            .args(["pd", "serve", "--data-dir"])
            .arg(data_dir.path().join("pd-4"))
            .args(["--listen", &joiner, "--id", "4"])
            .args(["--join", &founded.join(",")])
            // **Kept, not discarded.** This process is the one with something to say when it
            // refuses to join, and the refusal is a sentence naming the remedy.
            .stdout(Stdio::from(
                std::fs::File::create(data_dir.path().join("pd-4.log")).expect("the joiner's log"),
            ))
            .stderr(Stdio::from(
                std::fs::File::options()
                    .append(true)
                    .open(data_dir.path().join("pd-4.log"))
                    .expect("the joiner's log"),
            ))
            .spawn()
            .expect("the fourth driver starts"),
    );
    let log = data_dir.path().join("pd-4.log");
    wait_for_port(
        "the fourth driver",
        joiner_port,
        &mut fourth,
        STARTUP_SECONDS,
        || {
            format!(
                ". What it said:\n{}",
                std::fs::read_to_string(&log).unwrap_or_default()
            )
        },
    );

    let added = adding
        .wait_with_output()
        .expect("waiting on `pd members add`");
    assert!(
        added.status.success(),
        "`pd members add` failed: {}{}",
        String::from_utf8_lossy(&added.stdout),
        String::from_utf8_lossy(&added.stderr)
    );

    // Everyone, the new member included, and they have to agree with each other.
    let mut all = founded.clone();
    all.push(joiner.clone());
    agree_on(&all, 4, "the group after the add");

    // And back again, which is the half `remove` owns: a group that can only grow is not a group
    // an operator can repair.
    let removed = members_command(&["remove", "4"], &founded[0]);
    assert!(
        removed.status.success(),
        "`pd members remove` failed: {}{}",
        String::from_utf8_lossy(&removed.stdout),
        String::from_utf8_lossy(&removed.stderr)
    );
    agree_on(&founded, DRIVERS as usize, "the group after the remove");

    let _ = fourth.0.kill();
    let _ = fourth.0.wait();
    let _ = cluster.0.kill();
    let _ = cluster.0.wait();
}

/// One store and [`DRIVERS`] placement drivers, every one of them listening before this returns.
fn start_the_cluster(data_dir: &std::path::Path, store_port: u16) -> Supervisor {
    let mut cluster = Supervisor(
        Command::new(esker_cli())
            .args([
                "cluster",
                "start",
                "--nodes",
                &NODES.to_string(),
                "--data-dir",
            ])
            .arg(data_dir)
            .args(["--base-port", &store_port.to_string(), "--pd"])
            .args(["--pd-nodes", &DRIVERS.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the cluster starts"),
    );
    let nothing = || String::new();
    wait_for_port(
        "the store",
        store_port,
        &mut cluster,
        STARTUP_SECONDS,
        nothing,
    );
    for at in 0..DRIVERS {
        wait_for_port(
            &format!("placement driver {}", at + 1),
            store_port + NODES + at,
            &mut cluster,
            STARTUP_SECONDS,
            nothing,
        );
    }
    cluster
}

/// Polls one member until its listing names `address` — the `AddLearner` has committed.
fn wait_until_listed(endpoints: &[String], address: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut said = String::new();
    loop {
        for endpoint in endpoints {
            let out = Command::new(esker_cli())
                .args(["pd", "members", "--pd", endpoint])
                .output()
                .expect("`pd members` runs");
            said = String::from_utf8_lossy(&out.stdout).into_owned();
            if said.contains(address) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no member listed {address} within a minute of `pd members add`, so the AddLearner \
             never committed. Last answer:\n{said}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Asks **every** address what its group is, and asserts they answer the same thing.
///
/// The count, the group id and the presence of a leader, from each member separately. That is what
/// separates one group of `expected` from `expected` members with their own opinions — which is the
/// failure an exit code cannot see.
fn agree_on(addresses: &[String], expected: usize, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let said = loop {
        let said: Vec<String> = addresses
            .iter()
            .map(|address| {
                let out = Command::new(esker_cli())
                    .args(["pd", "members", "--pd", address])
                    .output()
                    .expect("`pd members` runs");
                String::from_utf8_lossy(&out.stdout).into_owned()
            })
            .collect();
        let settled = said.iter().all(|answer| {
            addresses.iter().all(|a| answer.contains(a.as_str()))
                && answer.matches("voter").count() == expected
                && answer.contains("leader")
                && !answer.contains("no leader")
        });
        if settled {
            break said;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never settled at {expected} voters that all name each other and one leader. \
             What each member said:\n{}",
            said.join("\n---\n")
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    // One group id across every member, which is the check ADR 0059's protection rests on and the
    // one a member that founded its own group fails.
    let ids: Vec<&str> = said
        .iter()
        .filter_map(|answer| {
            answer
                .split_whitespace()
                .find(|word| word.starts_with("0x"))
                .map(|word| word.trim_end_matches(','))
        })
        .collect();
    assert_eq!(
        ids.len(),
        said.len(),
        "a member of {what} reported no group id:\n{}",
        said.join("\n---\n")
    );
    assert!(
        ids.windows(2).all(|pair| pair[0] == pair[1]),
        "{what} does not agree on a group id: {ids:?}"
    );
}

fn members_command(words: &[&str], at: &str) -> std::process::Output {
    let mut command = Command::new(esker_cli());
    command.args(["pd", "members"]);
    command.args(words);
    command.args(["--pd", at]);
    command.output().expect("`pd members` runs")
}

struct Supervisor(Child);

impl Drop for Supervisor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn esker_cli() -> &'static str {
    env!("CARGO_BIN_EXE_esker-cli")
}

fn warm() {
    let _unused = Command::new(esker_cli())
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// This file's own port band, for the reason `cluster_start.rs` gives: releasing a bound run and
/// returning the base races whoever binds next, and only a private band makes that harmless
/// between test binaries.
fn free_ports(span: u16) -> u16 {
    for base in (34_100_u16..35_000).step_by(span as usize) {
        let bound: Vec<TcpListener> = (0..span)
            .filter_map(|at| TcpListener::bind(("127.0.0.1", base.checked_add(at)?)).ok())
            .collect();
        if bound.len() == span as usize {
            return base;
        }
    }
    panic!("no run of {span} consecutive free ports in 34,100–35,000, this file's own band");
}

fn wait_for_port(
    what: &str,
    port: u16,
    child: &mut Supervisor,
    seconds: u64,
    said: impl Fn() -> String,
) {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if let Some(status) = child.0.try_wait().expect("waiting on the child") {
            panic!(
                "{what} exited with {status} instead of listening on {port}{}",
                said()
            );
        }
        assert!(
            Instant::now() < deadline,
            "{what} did not listen on {port} within {seconds}s"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
