//! Hand-written argument parsing.
//!
//! `clap` is not on the dependency allowlist, and a tool with a handful of subcommands does
//! not need a parser generator. What it does need is to reject bad input clearly instead of
//! guessing, which is the same rule the wire protocol follows: an unknown flag is an error,
//! never something quietly ignored.
//!
//! The grammar is deliberately small:
//!
//! ```text
//! esker [--version | -V] [--help | -h]
//! esker bench [--threads N] [--value-size N] [--duration-secs N] [--help]
//! esker sst-dump <path> [--verbose | -v] [--prefix-len N] [--help]
//! esker wal-dump <path> [--verbose | -v] [--help]
//! esker manifest-dump <dir> [--help]
//! ```
//!
//! Both `--flag value` and `--flag=value` are accepted, because both are what people type.

use std::fmt;
use std::path::PathBuf;

use crate::manifest_dump::DumpOptions as ManifestDumpOptions;
use crate::sst_dump::DumpOptions;
use crate::wal_dump::DumpOptions as WalDumpOptions;

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Command {
    /// Print the version and exit.
    Version,
    /// Print usage and exit.
    Help,
    /// Run the benchmark driver.
    Bench(BenchOptions),
    /// Print the contents of a sorted string table.
    SstDump(DumpOptions),
    /// Print the contents of a write-ahead log segment.
    WalDump(WalDumpOptions),
    /// Print a database's manifest and the version it reconstructs to.
    ManifestDump(ManifestDumpOptions),
}

/// Options for the benchmark driver.
///
/// The driver itself is phase 1 (`docs/bench/README.md`); these are parsed and validated now
/// so that the shape of the command does not change under anyone later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BenchOptions {
    /// Concurrent writer threads.
    pub(crate) threads: u32,
    /// Value size in bytes.
    pub(crate) value_size: u32,
    /// How long to run, in seconds.
    pub(crate) duration_secs: u32,
}

impl Default for BenchOptions {
    fn default() -> Self {
        Self {
            threads: 4,
            value_size: 100,
            duration_secs: 10,
        }
    }
}

/// Why the arguments could not be understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParseError {
    /// A flag this build does not know.
    UnknownFlag(String),
    /// A subcommand this build does not know.
    UnknownCommand(String),
    /// A flag that takes a value was given none.
    MissingValue(&'static str),
    /// A value that is not a number, or is out of range.
    InvalidValue {
        /// The flag it belonged to.
        flag: &'static str,
        /// What the user wrote.
        value: String,
    },
    /// A bare word where none belongs.
    UnexpectedArgument(String),
    /// A required positional argument was not given.
    MissingArgument(&'static str),
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnknownFlag(flag) => write!(formatter, "unknown flag `{flag}`"),
            ParseError::UnknownCommand(command) => {
                write!(formatter, "unknown command `{command}`")
            }
            ParseError::MissingValue(flag) => write!(formatter, "`{flag}` needs a value"),
            ParseError::InvalidValue { flag, value } => {
                write!(formatter, "`{flag}` does not accept `{value}`")
            }
            ParseError::UnexpectedArgument(argument) => {
                write!(formatter, "unexpected argument `{argument}`")
            }
            ParseError::MissingArgument(name) => write!(formatter, "missing {name}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Usage text, printed for `--help` and alongside any parse error.
pub(crate) const USAGE: &str = "\
esker — tools for the Esker key-value store

Usage:
  esker [options]
  esker <command> [options]

Commands:
  bench                 Run the benchmark driver
  sst-dump <path>       Print the contents of a sorted string table
  wal-dump <path>       Print the fragments and records of a log segment
  manifest-dump <dir>   Print a database's manifest and reconstructed version

Options:
  -V, --version         Print the version
  -h, --help            Print this message

Bench options:
      --threads N       Concurrent writer threads (default 4)
      --value-size N    Value size in bytes (default 100)
      --duration-secs N How long to run (default 10)

Sst-dump options:
  -v, --verbose         Print every key and value, not just the summary
      --prefix-len N    Rebuild a StripSuffix prefix extractor of this length, so
                        that a prefix-built bloom filter can be used

Wal-dump options:
  -v, --verbose         Print entry values as well as keys

A torn record at the tail of a log or manifest is what a crash looks like, not
damage: it is reported with a notice and exit 0. Corruption anywhere exits 1.

Exit codes:
  0  success       1  the file could not be read or is corrupt       2  bad usage
";

/// Parses arguments, which must **not** include the program name.
pub(crate) fn parse<I, S>(arguments: I) -> Result<Command, ParseError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let arguments: Vec<String> = arguments.into_iter().map(Into::into).collect();
    let Some(first) = arguments.first() else {
        return Ok(Command::Help);
    };

    match first.as_str() {
        "--version" | "-V" => Ok(Command::Version),
        "--help" | "-h" | "help" => Ok(Command::Help),
        "bench" => parse_bench(&arguments[1..]),
        "sst-dump" => parse_sst_dump(&arguments[1..]),
        "wal-dump" => parse_wal_dump(&arguments[1..]),
        "manifest-dump" => parse_manifest_dump(&arguments[1..]),
        other if other.starts_with('-') => Err(ParseError::UnknownFlag(other.to_owned())),
        other => Err(ParseError::UnknownCommand(other.to_owned())),
    }
}

fn parse_bench(arguments: &[String]) -> Result<Command, ParseError> {
    let mut options = BenchOptions::default();
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;

        if argument == "--help" || argument == "-h" {
            return Ok(Command::Help);
        }

        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (argument.as_str(), None),
        };

        let (target, name): (&mut u32, &'static str) = match flag {
            "--threads" => (&mut options.threads, "--threads"),
            "--value-size" => (&mut options.value_size, "--value-size"),
            "--duration-secs" => (&mut options.duration_secs, "--duration-secs"),
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            other => return Err(ParseError::UnexpectedArgument(other.to_owned())),
        };

        let raw = if let Some(value) = inline {
            value
        } else {
            let value = arguments.get(index).ok_or(ParseError::MissingValue(name))?;
            index += 1;
            value.clone()
        };

        *target = raw.parse::<u32>().ok().filter(|parsed| *parsed > 0).ok_or(
            ParseError::InvalidValue {
                flag: name,
                value: raw,
            },
        )?;
    }

    Ok(Command::Bench(options))
}

/// `sst-dump <path> [--verbose] [--prefix-len N]`.
///
/// The path is positional and required: a dump with nothing to dump is a usage error, not an
/// empty report.
fn parse_sst_dump(arguments: &[String]) -> Result<Command, ParseError> {
    let mut path: Option<PathBuf> = None;
    let mut verbose = false;
    let mut prefix_len = None;
    let mut index = 0;

    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;

        if argument == "--help" || argument == "-h" {
            return Ok(Command::Help);
        }
        if argument == "--verbose" || argument == "-v" {
            verbose = true;
            continue;
        }

        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (argument.as_str(), None),
        };

        match flag {
            "--prefix-len" => {
                let raw = if let Some(value) = inline {
                    value
                } else {
                    let value = arguments
                        .get(index)
                        .ok_or(ParseError::MissingValue("--prefix-len"))?;
                    index += 1;
                    value.clone()
                };
                prefix_len = Some(raw.parse::<usize>().map_err(|_| ParseError::InvalidValue {
                    flag: "--prefix-len",
                    value: raw,
                })?);
            }
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            // The first bare word is the path; a second one is a mistake worth naming.
            _ if path.is_none() => path = Some(PathBuf::from(argument)),
            other => return Err(ParseError::UnexpectedArgument(other.to_owned())),
        }
    }

    Ok(Command::SstDump(DumpOptions {
        path: path.ok_or(ParseError::MissingArgument("<path>"))?,
        verbose,
        prefix_len,
    }))
}

/// `wal-dump <path> [--verbose]`.
fn parse_wal_dump(arguments: &[String]) -> Result<Command, ParseError> {
    let mut path = None;
    let mut verbose = false;

    for argument in arguments {
        match argument.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--verbose" | "-v" => verbose = true,
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            other if path.is_none() => path = Some(PathBuf::from(other)),
            other => return Err(ParseError::UnexpectedArgument(other.to_owned())),
        }
    }

    Ok(Command::WalDump(WalDumpOptions {
        path: path.ok_or(ParseError::MissingArgument("<path>"))?,
        verbose,
    }))
}

/// `manifest-dump <dir>`.
///
/// The argument is the database directory, not the manifest: `CURRENT` is what says which
/// manifest is in force, and naming one directly would invite reading a stale one.
fn parse_manifest_dump(arguments: &[String]) -> Result<Command, ParseError> {
    let mut path = None;

    for argument in arguments {
        match argument.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            other if path.is_none() => path = Some(PathBuf::from(other)),
            other => return Err(ParseError::UnexpectedArgument(other.to_owned())),
        }
    }

    Ok(Command::ManifestDump(ManifestDumpOptions {
        path: path.ok_or(ParseError::MissingArgument("<dir>"))?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(arguments: &[&str]) -> Command {
        parse(arguments.iter().copied()).expect("expected the arguments to parse")
    }

    #[test]
    fn no_arguments_prints_usage() {
        assert_eq!(parse(Vec::<String>::new()).unwrap(), Command::Help);
    }

    #[test]
    fn version_and_help_have_short_forms() {
        assert_eq!(parse_ok(&["--version"]), Command::Version);
        assert_eq!(parse_ok(&["-V"]), Command::Version);
        assert_eq!(parse_ok(&["--help"]), Command::Help);
        assert_eq!(parse_ok(&["-h"]), Command::Help);
        assert_eq!(parse_ok(&["help"]), Command::Help);
    }

    #[test]
    fn bench_uses_defaults_when_given_nothing() {
        assert_eq!(
            parse_ok(&["bench"]),
            Command::Bench(BenchOptions::default())
        );
    }

    #[test]
    fn bench_accepts_both_flag_forms() {
        let separate = parse_ok(&["bench", "--threads", "8", "--value-size", "4096"]);
        let inline = parse_ok(&["bench", "--threads=8", "--value-size=4096"]);
        assert_eq!(separate, inline);
        let Command::Bench(options) = separate else {
            panic!("expected a bench command");
        };
        assert_eq!(options.threads, 8);
        assert_eq!(options.value_size, 4096);
        assert_eq!(options.duration_secs, BenchOptions::default().duration_secs);
    }

    /// An unknown flag is an error, not something silently dropped. The same rule the wire
    /// protocol follows (`docs/DESIGN.md` §9).
    #[test]
    fn unknown_input_is_rejected() {
        assert_eq!(
            parse(["--frobnicate"]),
            Err(ParseError::UnknownFlag("--frobnicate".to_owned()))
        );
        assert_eq!(
            parse(["compact"]),
            Err(ParseError::UnknownCommand("compact".to_owned()))
        );
        assert_eq!(
            parse(["bench", "--jitter"]),
            Err(ParseError::UnknownFlag("--jitter".to_owned()))
        );
        assert_eq!(
            parse(["bench", "extra"]),
            Err(ParseError::UnexpectedArgument("extra".to_owned()))
        );
    }

    #[test]
    fn a_flag_without_its_value_is_an_error() {
        assert_eq!(
            parse(["bench", "--threads"]),
            Err(ParseError::MissingValue("--threads"))
        );
    }

    /// Zero threads, a zero-second run and a non-numeric value are all nonsense, and none of
    /// them should turn into a default that quietly measures something else.
    #[test]
    fn nonsense_values_are_rejected() {
        for (flag, value) in [
            ("--threads", "0"),
            ("--threads", "-1"),
            ("--threads", "many"),
            ("--value-size", "0"),
            ("--duration-secs", "99999999999999999999"),
        ] {
            let result = parse(["bench", flag, value]);
            assert!(
                matches!(result, Err(ParseError::InvalidValue { .. })),
                "`{flag} {value}` was accepted: {result:?}"
            );
        }
    }

    #[test]
    fn errors_say_what_was_wrong() {
        let message = parse(["--frobnicate"]).unwrap_err().to_string();
        assert!(
            message.contains("--frobnicate"),
            "unhelpful message: {message}"
        );
    }

    #[test]
    fn usage_lists_every_command_and_flag() {
        for expected in [
            "bench",
            "--version",
            "--help",
            "--threads",
            "--value-size",
            "--duration-secs",
        ] {
            assert!(
                USAGE.contains(expected),
                "usage does not mention {expected}"
            );
        }
    }
}
