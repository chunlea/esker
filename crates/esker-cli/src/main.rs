//! `esker` — command line tools for the Esker key-value store.
//!
//! `sst-dump`, `wal-dump` and `manifest-dump` inspect the three on-disk formats; `bench` is
//! still the phase-0 placeholder, and `region` arrives with the layer it inspects
//! (`docs/DESIGN.md` §12).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod args;
mod bytes;
mod manifest_dump;
mod sst_dump;
mod wal_dump;

use std::process::ExitCode;

use args::{Command, USAGE};

/// Exit code for arguments that could not be parsed.
const EXIT_USAGE: u8 = 2;

/// Exit code for a command that ran and failed — a corrupt file, a missing one.
const EXIT_FAILURE: u8 = 1;

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
        Ok(Command::Bench(options)) => {
            // TODO(phase-1): drive a real workload against esker-engine and record the
            // numbers as docs/bench/README.md describes.
            println!(
                "bench: not implemented (threads={}, value-size={}, duration-secs={})",
                options.threads, options.value_size, options.duration_secs
            );
            ExitCode::SUCCESS
        }
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
        Err(error) => {
            eprintln!("esker: {error}\n");
            eprint!("{USAGE}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}
