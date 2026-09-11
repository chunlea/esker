//! `esker` — command line tools for the Esker key-value store.
//!
//! `server` runs a store; `raw` talks to one. `sst-dump`, `wal-dump` and `manifest-dump`
//! inspect the three on-disk formats, `bench` drives a workload against a real database,
//! `bench-mpp` measures a distributed aggregate on a cluster it starts, and
//! `region` arrives with the layer it inspects (`docs/DESIGN.md` §12).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod admin;
mod args;
mod bench;
mod bench_mpp;
mod bench_pd;
mod bench_remote;
mod bench_route;
mod bench_txn;
mod bytes;
mod cluster;
mod durability;
mod manifest_dump;
mod pd;
mod raw;
mod readiness;
mod reconcile;
mod region;
mod rpc_tls;
mod server;
mod sst_dump;
mod sst_store;
#[cfg(test)]
mod testserver;
mod wal_dump;

use std::process::ExitCode;

use args::{Command, USAGE};

/// Exit code for arguments that could not be parsed.
const EXIT_USAGE: u8 = 2;

/// Exit code for a command that ran and failed — a corrupt file, a missing one, or a key that
/// is not there.
const EXIT_FAILURE: u8 = 1;

/// Exit code for a request the server refused, or a server that could not be reached. Distinct
/// from [`EXIT_FAILURE`] so a script can tell "the key is absent" from "the cluster is down".
const EXIT_SERVER: u8 = 3;

/// The `sst-store` verbs, out of line so `main` stays a dispatch table.
fn run_sst_store(command: &args::SstStoreCommand) -> ExitCode {
    match command {
        args::SstStoreCommand::Reconcile(options) => {
            let mut stdout = std::io::stdout().lock();
            match reconcile::run(options, &mut stdout) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("esker sst-store reconcile: {error}");
                    ExitCode::from(EXIT_FAILURE)
                }
            }
        }
    }
}

/// `bench-mpp`, out of line for the same reason [`run_sst_store`] is: `main` stays a dispatch
/// table, and clippy holds it to a hundred lines.
/// One of the three durability verbs.
///
/// **A lost write leaves a non-zero exit**, so a harness that reads only the status still stops:
/// this is `CLAUDE.md` invariant 1 and it must not be possible to miss.
fn run_durability(command: &args::DurabilityCommand) -> ExitCode {
    let outcome = match command {
        args::DurabilityCommand::Record(options) => durability::record(options),
        args::DurabilityCommand::Chaos(options) => durability::chaos(options),
        args::DurabilityCommand::Verify(options) => durability::verify(options),
    };
    match outcome {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("{reason}");
            ExitCode::FAILURE
        }
    }
}

fn run_bench_mpp(options: &bench_mpp::BenchMppOptions) -> ExitCode {
    match bench_mpp::run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("esker bench-mpp: {reason}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

fn run_admin(options: &admin::AdminOptions) -> ExitCode {
    match admin::run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            eprintln!("esker admin: {reason}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

fn main() -> ExitCode {
    match args::parse(std::env::args().skip(1)) {
        Ok(Command::Version) => {
            println!("esker {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Help) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Command::Server(options)) => match server::run(&options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(reason) => {
                eprintln!("esker server: {reason}");
                ExitCode::from(EXIT_FAILURE)
            }
        },
        Ok(Command::Bench(options)) => match bench::run(&options) {
            Ok(report) => {
                report.print();
                ExitCode::SUCCESS
            }
            Err(reason) => {
                eprintln!("esker bench: {reason}");
                ExitCode::from(EXIT_FAILURE)
            }
        },
        Ok(Command::BenchMpp(options)) => run_bench_mpp(&options),
        Ok(Command::Durability(command)) => run_durability(&command),
        Ok(Command::SstDump(options)) => {
            let mut stdout = std::io::stdout().lock();
            match sst_dump::run(&options, &mut stdout) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("esker sst-dump: {error}");
                    ExitCode::from(EXIT_FAILURE)
                }
            }
        }
        Ok(Command::WalDump(options)) => {
            let mut stdout = std::io::stdout().lock();
            match wal_dump::run(&options, &mut stdout) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("esker wal-dump: {error}");
                    ExitCode::from(EXIT_FAILURE)
                }
            }
        }
        Ok(Command::ManifestDump(options)) => {
            let mut stdout = std::io::stdout().lock();
            match manifest_dump::run(&options, &mut stdout) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("esker manifest-dump: {error}");
                    ExitCode::from(EXIT_FAILURE)
                }
            }
        }
        Ok(Command::Raw(options)) => {
            let mut stdout = std::io::stdout().lock();
            match raw::run(&options, &mut stdout) {
                Ok(raw::Outcome::Done) => ExitCode::SUCCESS,
                Ok(raw::Outcome::NotFound) => ExitCode::from(EXIT_FAILURE),
                Err(reason) => {
                    eprintln!("esker raw: {reason}");
                    ExitCode::from(EXIT_SERVER)
                }
            }
        }
        Ok(Command::Admin(options)) => run_admin(&options),
        Ok(Command::Region(options)) => match region::run(&options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(reason) => {
                eprintln!("esker region: {reason}");
                ExitCode::from(EXIT_FAILURE)
            }
        },
        Ok(Command::Cluster(options)) => match cluster::run(&options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(reason) => {
                eprintln!("esker cluster: {reason}");
                ExitCode::from(EXIT_FAILURE)
            }
        },
        Ok(Command::SstStore(command)) => run_sst_store(&command),
        Ok(Command::Pd(command)) => match pd::run(&command) {
            Ok(()) => ExitCode::SUCCESS,
            Err(reason) => {
                eprintln!("esker pd: {reason}");
                ExitCode::from(EXIT_FAILURE)
            }
        },
        Err(error) => {
            eprintln!("esker: {error}\n");
            eprint!("{USAGE}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}
