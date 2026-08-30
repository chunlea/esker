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
//! esker bench [<workload>] [--num N] [--value-size N] [--batch-size N] [--threads N]
//!             [--sync] [--dir PATH] [--duration-secs N] [--help]
//! esker sst-dump <path> [--verbose | -v] [--prefix-len N] [--help]
//! esker wal-dump <path> [--verbose | -v] [--help]
//! esker manifest-dump <dir> [--help]
//! ```
//!
//! Both `--flag value` and `--flag=value` are accepted, because both are what people type.

use std::fmt;
use std::path::PathBuf;

use crate::bench::{Run as BenchOptions, Workload};
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
    /// A workload name `bench` does not have.
    UnknownWorkload(String),
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
            ParseError::UnknownWorkload(workload) => write!(
                formatter,
                "unknown workload `{workload}`; expected fillseq, fillrandom, overwrite, \
                 readrandom or readseq"
            ),
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
  <workload>            fillseq | fillrandom | overwrite | readrandom | readseq
                        (default fillrandom)
      --num N           Keys in the database, and operations measured (default 100000)
      --value-size N    Value size in bytes (default 100)
      --batch-size N    Entries per write batch (default 1)
      --threads N       Concurrent workers; readseq always uses one (default 1)
      --sync            Wait for each write to be durable (default off)
      --dir PATH        Where to put the database (default a temporary directory)
      --duration-secs N Stop the measured phase early after this long (default 0, no limit)

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

/// Which numeric field a flag sets. Named so the parse loop can read a value once and assign
/// it once, rather than repeating the same three lines per flag.
enum Target {
    Num,
    ValueSize,
    BatchSize,
    Threads,
    DurationSecs,
}

fn parse_bench(arguments: &[String]) -> Result<Command, ParseError> {
    let mut options = BenchOptions::default();
    let mut index = 0;
    let mut chose_workload = false;

    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;

        if argument == "--help" || argument == "-h" {
            return Ok(Command::Help);
        }
        if argument == "--sync" {
            options.sync = true;
            continue;
        }
        if argument == "--no-sync" {
            options.sync = false;
            continue;
        }

        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (argument.as_str(), None),
        };

        if flag == "--dir" {
            let raw = take_value(arguments, &mut index, inline, "--dir")?;
            options.dir = Some(PathBuf::from(raw));
            continue;
        }

        let (target, name): (Target, &'static str) = match flag {
            "--num" => (Target::Num, "--num"),
            "--value-size" => (Target::ValueSize, "--value-size"),
            "--batch-size" => (Target::BatchSize, "--batch-size"),
            "--threads" => (Target::Threads, "--threads"),
            "--duration-secs" => (Target::DurationSecs, "--duration-secs"),
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            other => {
                // The one positional argument: which workload to run.
                if chose_workload {
                    return Err(ParseError::UnexpectedArgument(other.to_owned()));
                }
                let Some(workload) = Workload::parse(other) else {
                    return Err(ParseError::UnknownWorkload(other.to_owned()));
                };
                options.workload = workload;
                chose_workload = true;
                continue;
            }
        };

        let raw = take_value(arguments, &mut index, inline, name)?;
        match target {
            Target::Num => {
                options.num = raw.parse().ok().filter(|num| *num > 0).ok_or_else(|| {
                    ParseError::InvalidValue {
                        flag: name,
                        value: raw.clone(),
                    }
                })?;
            }
            Target::ValueSize => options.value_size = positive(name, &raw)?,
            Target::BatchSize => options.batch_size = positive(name, &raw)?,
            Target::Threads => options.threads = positive(name, &raw)?,
            // Zero is the only sensible "no limit", so it is allowed here and nowhere else.
            Target::DurationSecs => options.duration_secs = number(name, &raw)?,
        }
    }

    Ok(Command::Bench(options))
}

/// Reads a flag's value, whether it came after an `=` or as the next argument.
fn take_value(
    arguments: &[String],
    index: &mut usize,
    inline: Option<String>,
    name: &'static str,
) -> Result<String, ParseError> {
    if let Some(value) = inline {
        return Ok(value);
    }
    let Some(value) = arguments.get(*index) else {
        return Err(ParseError::MissingValue(name));
    };
    *index += 1;
    Ok(value.clone())
}

fn number(name: &'static str, raw: &str) -> Result<u32, ParseError> {
    raw.parse().map_err(|_| ParseError::InvalidValue {
        flag: name,
        value: raw.to_owned(),
    })
}

/// Like [`number`], but zero is nonsense: zero threads run nothing, and a zero-byte value or
/// batch measures nothing. A default quietly standing in for one of those would be a
/// benchmark of something the caller did not ask for.
fn positive(name: &'static str, raw: &str) -> Result<u32, ParseError> {
    let value = number(name, raw)?;
    if value == 0 {
        return Err(ParseError::InvalidValue {
            flag: name,
            value: raw.to_owned(),
        });
    }
    Ok(value)
}

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

    /// The workload is the one positional argument, and an unknown one is refused rather than
    /// silently replaced by the default.
    #[test]
    fn bench_takes_a_workload_and_the_acceptance_flags() {
        let Command::Bench(options) = parse_ok(&[
            "bench",
            "fillrandom",
            "--value-size",
            "100",
            "--num",
            "1000000",
        ]) else {
            panic!("expected a bench command");
        };
        assert_eq!(options.workload, Workload::FillRandom);
        assert_eq!(options.num, 1_000_000);
        assert_eq!(options.value_size, 100);

        assert_eq!(
            parse(["bench", "fillfast"]),
            Err(ParseError::UnknownWorkload("fillfast".to_owned()))
        );
        assert_eq!(
            parse(["bench", "fillseq", "readseq"]),
            Err(ParseError::UnexpectedArgument("readseq".to_owned()))
        );
    }

    #[test]
    fn bench_sync_is_a_flag_without_a_value() {
        let Command::Bench(options) = parse_ok(&["bench", "fillseq", "--sync"]) else {
            panic!("expected a bench command");
        };
        assert!(options.sync);
        assert_eq!(options.workload, Workload::FillSeq);

        let Command::Bench(options) = parse_ok(&["bench", "--sync", "--no-sync"]) else {
            panic!("expected a bench command");
        };
        assert!(!options.sync, "the last one wins");
    }

    #[test]
    fn bench_takes_a_directory() {
        let Command::Bench(options) = parse_ok(&["bench", "--dir=/tmp/esker"]) else {
            panic!("expected a bench command");
        };
        assert_eq!(options.dir, Some(PathBuf::from("/tmp/esker")));
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
        // `bench` now takes a workload as its one positional argument, so a bare word there
        // is a workload that does not exist rather than an argument that does not belong.
        assert_eq!(
            parse(["bench", "extra"]),
            Err(ParseError::UnknownWorkload("extra".to_owned()))
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
            ("--num", "0"),
            ("--batch-size", "0"),
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
