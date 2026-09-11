//! `esker admin` — asking one store to do something to its own storage.
//!
//! [ADR 0109](../../../docs/adr/0109-an-operator-can-ask-a-store-to-flush-and-to-compact.md). Two
//! verbs, `flush` and `compact`, and the thing that makes them worth having is that **everything
//! else that decides when an SST appears is inside the store**: a memtable crosses
//! `write_buffer_size` and a flush job runs. `Store::flush` and `Store::compact_write_cf` have
//! existed since phase 2 with one caller between them — `esker bench --compact`, which opens its
//! own database rather than talking to a running store over a socket.
//!
//! # Addressed to a store, not routed through the placement driver
//!
//! `esker region` goes to PD first because a region lives somewhere and only the driver knows
//! where. A flush is about **this store's memtables**, so there is nothing to route: the operator
//! names the store and the store answers for itself. That also makes the verb usable on a store
//! that has no placement driver at all, which is what the acceptance test starts.
//!
//! # It answers when the work is done
//!
//! `Db::flush_all` blocks on the flush job and `compact_range` on the compaction it schedules, so
//! neither verb adds a cadence of its own. The answer is the store's SSTs afterwards — every
//! column family, every file, with the level each is at — because a verb that said only "done" is
//! useless to the arm that has to assert on what it produced.

use std::net::SocketAddr;

use esker_proto::{AdminReq, AdminResp, BlockingTransport, Request, TransportConfig};

/// What `esker admin` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdminCommand {
    /// Write every memtable out as an SST.
    Flush,
    /// Compact one column family, or every one.
    Compact {
        /// Which column family, or empty for all of them.
        cf: String,
    },
}

/// How `esker admin` was configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdminOptions {
    /// The store to ask.
    pub(crate) store: String,
    /// What to ask it.
    pub(crate) command: AdminCommand,
}

impl Default for AdminOptions {
    fn default() -> Self {
        Self {
            store: "127.0.0.1:20160".to_owned(),
            command: AdminCommand::Flush,
        }
    }
}

/// **Long, and deliberately so.** A compaction is unbounded work — it is bounded by how much the
/// store holds, not by how fast it answers — and this verb's whole contract is that it returns
/// when the work is done. A timeout short enough to be tidy would turn "the compaction is still
/// running" into "the command failed", which is the one answer that would make an operator do the
/// wrong thing next.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(900);

/// Runs one `esker admin` command.
pub(crate) fn run(options: &AdminOptions) -> Result<(), String> {
    let address: SocketAddr = options
        .store
        .parse()
        .map_err(|error| format!("`--store {}` is not an address: {error}", options.store))?;
    let store = BlockingTransport::connect_with(address, TransportConfig::new())
        .map_err(|error| format!("connecting to the store at {address}: {error}"))?;

    let (what, request) = match &options.command {
        AdminCommand::Flush => ("flushing", Request::Admin(AdminReq::Flush)),
        AdminCommand::Compact { cf } => (
            "compacting",
            Request::Admin(AdminReq::Compact { cf: cf.clone() }),
        ),
    };
    let response = store
        .call(request, std::time::Instant::now() + CALL_TIMEOUT)
        .map_err(|error| format!("{what} {address}: {error}"))?;

    let esker_proto::Response::Admin(
        AdminResp::Flushed { families } | AdminResp::Compacted { families },
    ) = response
    else {
        return Err(format!("the store answered {response:?}"));
    };
    report(&families);
    Ok(())
}

/// Prints what the store holds, one line per column family and a total.
///
/// The **per-level** counts rather than one number, because that is the distinction the
/// measurement this exists for turns on: files in L0 are versions that are all still there, and
/// files below it are versions that have been merged.
fn report(families: &[esker_proto::CfFiles]) {
    let mut total = 0;
    for family in families {
        total += family.files.len();
        if family.files.is_empty() {
            println!("{:>8}  no sst", family.cf);
            continue;
        }
        let mut levels: Vec<(u32, usize)> = Vec::new();
        for (level, _) in &family.files {
            match levels.iter_mut().find(|(at, _)| at == level) {
                Some((_, count)) => *count += 1,
                None => levels.push((*level, 1)),
            }
        }
        levels.sort_unstable();
        let by_level: Vec<String> = levels
            .iter()
            .map(|(level, count)| format!("L{level}:{count}"))
            .collect();
        println!(
            "{:>8}  {} sst  ({})",
            family.cf,
            family.files.len(),
            by_level.join(" ")
        );
    }
    println!("\n{total} sst in {} column families", families.len());
}
