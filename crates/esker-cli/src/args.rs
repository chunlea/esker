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
//!             [--sync] [--dir PATH] [--duration-secs N] [--remote HOST:PORT] [--help]
//! esker sst-dump <path> [--verbose | -v] [--prefix-len N] [--help]
//! esker wal-dump <path> [--verbose | -v] [--help]
//! esker manifest-dump <dir> [--help]
//! esker raw get <key> [--addr HOST:PORT] [--hex] [--help]
//! esker raw put <key> <value> [--addr HOST:PORT] [--hex] [--no-sync] [--help]
//! esker raw delete <key> [--addr HOST:PORT] [--hex] [--no-sync] [--help]
//! esker raw scan [<start>] [--end E] [--limit N] [--reverse] [--keys-only]
//!                [--addr HOST:PORT] [--hex] [--help]
//! ```
//!
//! Both `--flag value` and `--flag=value` are accepted, because both are what people type.

use std::fmt;
use std::path::PathBuf;

use crate::bench::{Run as BenchOptions, Workload};
use crate::manifest_dump::DumpOptions as ManifestDumpOptions;
use crate::raw::{RawCommand, RawOptions, from_hex};
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
    /// Read or write keys over the network.
    Raw(RawOptions),
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
    /// A `raw` verb this build does not know.
    UnknownRawCommand(String),
    /// `--hex` was given and an argument is not hex.
    InvalidHex(String),
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
                 readrandom, readmissing or readseq"
            ),
            ParseError::MissingArgument(name) => write!(formatter, "missing {name}"),
            ParseError::UnknownRawCommand(verb) => write!(
                formatter,
                "unknown raw command `{verb}`; expected get, put, delete or scan"
            ),
            ParseError::InvalidHex(value) => write!(
                formatter,
                "`{value}` is not hex; --hex needs an even number of hex digits"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// How many pairs `raw scan` prints when `--limit` is not given. Small on purpose: the
/// command is for looking, and a terminal is not where a million rows belong.
const DEFAULT_SCAN_ROWS: u32 = 100;

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
  raw <verb> ...        Read or write keys over the network

Options:
  -V, --version         Print the version
  -h, --help            Print this message

Bench options:
  <workload>            fillseq | fillrandom | overwrite | readrandom | readmissing
                        | readseq (default fillrandom)
      --num N           Keys in the database, and operations measured (default 100000)
      --value-size N    Value size in bytes (default 100)
      --batch-size N    Entries per write batch (default 1)
      --threads N       Concurrent workers; readseq always uses one (default 1)
      --sync            Wait for each write to be durable (default off)
      --dir PATH        Where to put the database (default a temporary directory)
      --duration-secs N Stop the measured phase early after this long (default 0, no limit)
      --bloom-bits N    Bloom filter bits per key; 0 builds none (default 10)
      --remote HOST:PORT  Drive the workload over the network against a running
                        server instead of an in-process database. The engine
                        options above belong to that server and are ignored.

Sst-dump options:
  -v, --verbose         Print every key and value, not just the summary
      --prefix-len N    Rebuild a StripSuffix prefix extractor of this length, so
                        that a prefix-built bloom filter can be used

Raw options:
  raw get <key>             Print the value, or exit 1 if the key is absent
  raw put <key> <value>     Write one key
  raw delete <key>          Remove one key; removing an absent key succeeds
  raw scan [<start>]        Print key<TAB>value for a run of keys
      --addr HOST:PORT      The store to talk to (default 127.0.0.1:20160)
      --hex                 Read arguments as hex and print results as hex
      --no-sync             Do not wait for a write to be durable
      --end E               Exclusive upper bound of a scan (default unbounded)
      --limit N             Most pairs a scan prints (default 100)
      --reverse             Scan from the high end of the range down
      --keys-only           Print keys without their values

A value that is not printable text is printed as hex whether or not --hex was
given, with a note on stderr; stdout is always exactly the value.

Wal-dump options:
  -v, --verbose         Print entry values as well as keys

A torn record at the tail of a log or manifest is what a crash looks like, not
damage: it is reported with a notice and exit 0. Corruption anywhere exits 1.

Exit codes:
  0  success       1  the file could not be read, is corrupt, or the key
                       was not found                                2  bad usage
  3  the server refused the request, or could not be reached
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
        "raw" => parse_raw(&arguments[1..]),
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
    BloomBits,
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
        if flag == "--remote" {
            options.remote = Some(take_value(arguments, &mut index, inline, "--remote")?);
            continue;
        }

        let (target, name): (Target, &'static str) = match flag {
            "--num" => (Target::Num, "--num"),
            "--value-size" => (Target::ValueSize, "--value-size"),
            "--batch-size" => (Target::BatchSize, "--batch-size"),
            "--threads" => (Target::Threads, "--threads"),
            "--duration-secs" => (Target::DurationSecs, "--duration-secs"),
            "--bloom-bits" => (Target::BloomBits, "--bloom-bits"),
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
            // Zero is meaningful for both of these — no time limit, and no filter — so they
            // take `number` rather than `positive`.
            Target::DurationSecs => options.duration_secs = number(name, &raw)?,
            Target::BloomBits => options.bloom_bits = number(name, &raw)?,
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

/// `raw <verb> [<positional>...] [flags]`.
///
/// The verb decides how many bare words are expected, so a missing value is named rather than
/// silently defaulted — the same rule the rest of this parser follows.
fn parse_raw(arguments: &[String]) -> Result<Command, ParseError> {
    let Some(verb) = arguments.first() else {
        return Err(ParseError::MissingArgument("<verb>"));
    };
    if verb == "--help" || verb == "-h" {
        return Ok(Command::Help);
    }
    if !matches!(verb.as_str(), "get" | "put" | "delete" | "scan") {
        return Err(ParseError::UnknownRawCommand(verb.clone()));
    }

    let mut options = RawOptions::default();
    let mut words: Vec<String> = Vec::new();
    let mut end: Option<String> = None;
    let mut limit: u32 = DEFAULT_SCAN_ROWS;
    let mut reverse = false;
    let mut keys_only = false;
    let mut index = 1;

    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;

        match argument.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--hex" => {
                options.hex = true;
                continue;
            }
            "--sync" => {
                options.sync = true;
                continue;
            }
            "--no-sync" => {
                options.sync = false;
                continue;
            }
            "--reverse" => {
                reverse = true;
                continue;
            }
            "--keys-only" => {
                keys_only = true;
                continue;
            }
            _ => {}
        }

        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (argument.as_str(), None),
        };

        match flag {
            "--addr" => options.addr = take_value(arguments, &mut index, inline, "--addr")?,
            "--end" => end = Some(take_value(arguments, &mut index, inline, "--end")?),
            "--limit" => {
                let raw = take_value(arguments, &mut index, inline, "--limit")?;
                limit = number("--limit", &raw)?;
            }
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            // A bare word is a key, a value or a scan bound, depending on the verb.
            _ => words.push(argument.clone()),
        }
    }

    // `--hex` applies to every argument or to none, so the decoding happens in one place and
    // a mixed-encoding mistake is impossible.
    let decode = |text: &String| -> Result<Vec<u8>, ParseError> {
        if options.hex {
            from_hex(text).ok_or_else(|| ParseError::InvalidHex(text.clone()))
        } else {
            Ok(text.clone().into_bytes())
        }
    };

    let command = raw_command(
        verb,
        &words,
        &decode,
        &ScanFlags {
            end,
            limit,
            reverse,
            keys_only,
        },
    )?;
    Ok(Command::Raw(RawOptions { command, ..options }))
}

/// The `scan` flags, gathered so the verb dispatch takes one argument rather than four.
struct ScanFlags {
    end: Option<String>,
    limit: u32,
    reverse: bool,
    keys_only: bool,
}

/// Turns a verb and its bare words into a command, refusing a word too many or too few.
fn raw_command(
    verb: &str,
    words: &[String],
    decode: &dyn Fn(&String) -> Result<Vec<u8>, ParseError>,
    scan: &ScanFlags,
) -> Result<RawCommand, ParseError> {
    let expected = match verb {
        "put" => 2,
        _ => 1,
    };
    if words.len() > expected {
        return Err(ParseError::UnexpectedArgument(words[expected].clone()));
    }

    let word = |position: usize, name: &'static str| -> Result<Vec<u8>, ParseError> {
        words
            .get(position)
            .ok_or(ParseError::MissingArgument(name))
            .and_then(decode)
    };

    Ok(match verb {
        "get" => RawCommand::Get {
            key: word(0, "<key>")?,
        },
        "put" => RawCommand::Put {
            key: word(0, "<key>")?,
            value: word(1, "<value>")?,
        },
        "delete" => RawCommand::Delete {
            key: word(0, "<key>")?,
        },
        // A scan with no bound starts at the beginning of the key space, and an unbounded end
        // runs to the end of it — both useful defaults rather than errors.
        _ => RawCommand::Scan {
            start: match words.first() {
                Some(text) => decode(text)?,
                None => Vec::new(),
            },
            end: match &scan.end {
                Some(text) => decode(text)?,
                None => Vec::new(),
            },
            limit: scan.limit,
            reverse: scan.reverse,
            keys_only: scan.keys_only,
        },
    })
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

    // -- raw ---------------------------------------------------------------------------

    fn raw_of(arguments: &[&str]) -> RawOptions {
        match parse_ok(arguments) {
            Command::Raw(options) => options,
            other => panic!("expected a raw command, got {other:?}"),
        }
    }

    #[test]
    fn raw_takes_a_verb_and_its_words() {
        assert_eq!(
            raw_of(&["raw", "get", "k"]).command,
            RawCommand::Get { key: b"k".to_vec() }
        );
        assert_eq!(
            raw_of(&["raw", "put", "k", "v"]).command,
            RawCommand::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            }
        );
        assert_eq!(
            raw_of(&["raw", "delete", "k"]).command,
            RawCommand::Delete { key: b"k".to_vec() }
        );
    }

    /// A scan with no bounds is the whole key space, which is what someone looking around
    /// wants; the row limit keeps that from filling a terminal.
    #[test]
    fn a_scan_defaults_to_the_whole_key_space_with_a_row_limit() {
        assert_eq!(
            raw_of(&["raw", "scan"]).command,
            RawCommand::Scan {
                start: Vec::new(),
                end: Vec::new(),
                limit: DEFAULT_SCAN_ROWS,
                reverse: false,
                keys_only: false,
            }
        );
        assert_eq!(
            raw_of(&[
                "raw",
                "scan",
                "a",
                "--end",
                "m",
                "--limit=5",
                "--reverse",
                "--keys-only"
            ])
            .command,
            RawCommand::Scan {
                start: b"a".to_vec(),
                end: b"m".to_vec(),
                limit: 5,
                reverse: true,
                keys_only: true,
            }
        );
    }

    #[test]
    fn raw_has_an_address_and_writes_are_durable_by_default() {
        let options = raw_of(&["raw", "get", "k"]);
        assert_eq!(options.addr, "127.0.0.1:20160");
        assert!(options.sync, "durability is an opt-out, never a default");

        let options = raw_of(&["raw", "put", "k", "v", "--addr=example:1", "--no-sync"]);
        assert_eq!(options.addr, "example:1");
        assert!(!options.sync);
    }

    /// `--hex` applies to every argument, so a key and a value cannot end up in different
    /// encodings by accident.
    #[test]
    fn hex_decodes_every_argument_or_none_of_them() {
        assert_eq!(
            raw_of(&["raw", "put", "6b", "ff00", "--hex"]).command,
            RawCommand::Put {
                key: vec![0x6b],
                value: vec![0xff, 0x00],
            }
        );
        // Without it, the same text is the text.
        assert_eq!(
            raw_of(&["raw", "put", "6b", "ff00"]).command,
            RawCommand::Put {
                key: b"6b".to_vec(),
                value: b"ff00".to_vec(),
            }
        );
    }

    #[test]
    fn raw_refuses_what_it_cannot_understand() {
        assert_eq!(parse(["raw"]), Err(ParseError::MissingArgument("<verb>")));
        assert_eq!(
            parse(["raw", "increment", "k"]),
            Err(ParseError::UnknownRawCommand("increment".to_owned()))
        );
        assert_eq!(
            parse(["raw", "get"]),
            Err(ParseError::MissingArgument("<key>"))
        );
        assert_eq!(
            parse(["raw", "put", "k"]),
            Err(ParseError::MissingArgument("<value>"))
        );
        assert_eq!(
            parse(["raw", "get", "k", "extra"]),
            Err(ParseError::UnexpectedArgument("extra".to_owned()))
        );
        assert_eq!(
            parse(["raw", "scan", "a", "b"]),
            Err(ParseError::UnexpectedArgument("b".to_owned()))
        );
        assert_eq!(
            parse(["raw", "get", "k", "--colour"]),
            Err(ParseError::UnknownFlag("--colour".to_owned()))
        );
        assert_eq!(
            parse(["raw", "get", "k", "--addr"]),
            Err(ParseError::MissingValue("--addr"))
        );
        // Bad hex is refused rather than silently taken as text, which would write a key
        // nobody asked for.
        assert_eq!(
            parse(["raw", "get", "zz", "--hex"]),
            Err(ParseError::InvalidHex("zz".to_owned()))
        );
        assert!(matches!(
            parse(["raw", "scan", "--limit", "nope"]),
            Err(ParseError::InvalidValue { .. })
        ));
    }

    #[test]
    fn raw_help_is_help_wherever_it_appears() {
        assert_eq!(parse_ok(&["raw", "--help"]), Command::Help);
        assert_eq!(parse_ok(&["raw", "get", "-h"]), Command::Help);
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
            "raw",
            "--addr",
            "--hex",
            "--keys-only",
        ] {
            assert!(
                USAGE.contains(expected),
                "usage does not mention {expected}"
            );
        }
    }
}
