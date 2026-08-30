//! `esker` — command line tools for the Esker key-value store.
//!
//! Phase 0 ships the shell: version, usage, and a `bench` subcommand that says it is not
//! implemented yet. Inspection commands (`sst-dump`, `wal-dump`, `manifest-dump`, `region`)
//! arrive with the layers they inspect (`docs/DESIGN.md` §12).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod args;

use std::process::ExitCode;

use args::{Command, USAGE};

/// Exit code for arguments that could not be parsed.
const EXIT_USAGE: u8 = 2;

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
        Err(error) => {
            eprintln!("esker: {error}\n");
            eprint!("{USAGE}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}
