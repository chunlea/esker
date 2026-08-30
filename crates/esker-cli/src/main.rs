//! `esker` — command line tools for the Esker key-value store.
//!
//! `server` runs a store; `raw` talks to one. `sst-dump`, `wal-dump` and `manifest-dump`
//! inspect the three on-disk formats, `bench` drives a workload against a real database, and
//! `region` arrives with the layer it inspects (`docs/DESIGN.md` §12).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod args;
mod bench;
mod bench_remote;
mod bytes;
mod cluster;
mod manifest_dump;
mod raw;
mod server;
mod sst_dump;
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
        Ok(Command::Cluster(options)) => match cluster::run(&options) {
            Ok(()) => ExitCode::SUCCESS,
            Err(reason) => {
                eprintln!("esker cluster: {reason}");
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
