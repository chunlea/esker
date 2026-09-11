//! `<data-dir>/cluster.state`: the one line per child that lets `stop` find what `start` launched.
//!
//! Split out of `super` on 2026-09-11, the second seam after `probes`. A move and not a redesign:
//! every line here was in `cluster/mod.rs` and does the same thing.
//!
//! # Why it is a format with a module and not two functions in a supervisor
//!
//! **It has readers outside this process, and one outside this repository.** `esker durability
//! chaos --state` picks its victims from it, `esker-rails-harness/leader-kill.py` resolves a census
//! `store=N` against it, and four test files in this crate parse it. That is what made its shape a
//! decision rather than a detail when `--pd-nodes` arrived: a driver's line keeps id **zero** and
//! a cluster may now have several of them, so *zero means a driver* and the **address** is what
//! tells two apart
//! ([ADR 0108](../../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)).
//! Every one of those readers kept working without being edited, and that was the point.
//!
//! **It is written by hand**, like every other format in this project: one function writes it and
//! one reads it, there is no `serde` anywhere in the workspace (`CLAUDE.md`), and a line that is
//! not `id address pid` is refused rather than guessed at — acting on half a state file would stop
//! the wrong process.

use std::io::Write;
use std::path::Path;

/// Where `start` records what it launched, under the cluster's data directory.
pub(super) const STATE_FILE: &str = "cluster.state";

/// One node's identity, as the state file records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Node {
    pub(super) id: u64,
    pub(super) address: String,
    pub(super) pid: u32,
}

/// `id address pid`, one node per line.
pub(super) fn write_state(data_dir: &Path, nodes: &[Node]) -> Result<(), String> {
    let path = data_dir.join(STATE_FILE);
    let mut file = std::fs::File::create(&path)
        .map_err(|error| format!("writing {}: {error}", path.display()))?;
    for node in nodes {
        writeln!(file, "{} {} {}", node.id, node.address, node.pid)
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
    }
    Ok(())
}

pub(super) fn read_state(data_dir: &Path) -> Result<Vec<Node>, String> {
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
    use super::{Node, read_state, write_state};

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
