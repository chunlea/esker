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
//!             [--sync] [--dir PATH] [--duration-secs N] [--remote HOST:PORT]
//!             [--write-buffer-size N] [--target-file-size N] [--block-size N]
//!             [--compact] [--help]
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
use crate::bench_mpp::BenchMppOptions;
use crate::cluster::ClusterOptions;
use crate::manifest_dump::DumpOptions as ManifestDumpOptions;
use crate::pd::{
    InspectOptions, MembersChange, MembersOptions, PdCommand, ServeOptions, StatusOptions,
};
use crate::raw::{RawCommand, RawOptions, from_hex};
use crate::region::{RegionCommand, RegionOptions};
use crate::server::ServerOptions;
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
    /// Measure what a distributed aggregate costs and where its time goes.
    BenchMpp(BenchMppOptions),
    /// Print the contents of a sorted string table.
    SstDump(DumpOptions),
    /// Print the contents of a write-ahead log segment.
    WalDump(WalDumpOptions),
    /// Print a database's manifest and the version it reconstructs to.
    ManifestDump(ManifestDumpOptions),
    /// Read or write keys over the network.
    Raw(RawOptions),
    /// Open a store and serve it.
    Server(ServerOptions),
    /// Start or stop a local cluster of stores replicating one region.
    Cluster(ClusterOptions),
    /// Run the placement driver, or print what it has stored.
    Pd(PdCommand),
    /// Look at, split, or hand over a region.
    Region(RegionOptions),
    /// Compare an SST store prefix against a database's manifest.
    SstStore(SstStoreCommand),
    /// Prove that no acknowledged write is lost when a store is killed
    /// (`CLAUDE.md` invariant 1).
    Durability(DurabilityCommand),
}

/// The `durability` verbs. Three, and separate processes on purpose: the verdict is computed after
/// the run, from a file, rather than by something that may itself have been killed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DurabilityCommand {
    /// Write, and record every acknowledged commit.
    Record(crate::durability::DurabilityOptions),
    /// Kill a store at random, on a cadence, with its own deadline.
    Chaos(crate::durability::DurabilityOptions),
    /// Read every recorded write back at the timestamp it was acknowledged at.
    Verify(crate::durability::DurabilityOptions),
}

/// The `sst-store` verbs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SstStoreCommand {
    /// List what the prefix holds and what nothing references.
    Reconcile(crate::reconcile::ReconcileOptions),
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
    /// A `pd` verb this build does not know.
    UnknownPdCommand(String),
    /// A `region` verb this build does not know.
    UnknownRegionCommand(String),
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
                 readrandom, readmissing, readseq, txnput, txnget, tso or allocid"
            ),
            ParseError::MissingArgument(name) => write!(formatter, "missing {name}"),
            ParseError::UnknownRawCommand(verb) => write!(
                formatter,
                "unknown raw command `{verb}`; expected get, put, delete or scan"
            ),
            ParseError::UnknownPdCommand(verb) => write!(
                formatter,
                "unknown pd command `{verb}`; expected serve, inspect, status or members"
            ),
            ParseError::UnknownRegionCommand(verb) => write!(
                formatter,
                "unknown region command `{verb}`; expected ls, split or transfer-leader"
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
  bench-mpp             Measure a distributed aggregate on a cluster this command
                        starts, and say what share of it one SQL node did
  sst-dump <path>       Print the contents of a sorted string table
  wal-dump <path>       Print the fragments and records of a log segment
  manifest-dump <dir>   Print a database's manifest and reconstructed version
  raw <verb> ...        Read or write keys over the network
  server                Open a store and serve the RawKV API
  cluster start|stop    Start or stop a local cluster replicating one region
                        (--nodes, --data-dir, --base-port, --seed, --sst-store,
                        --write-buffer-size; each node gets its own prefix under
                        the one given). --pd also starts a placement driver on
                        the port above the nodes and points every node at it,
                        which is what a SQL node needs to be given with --pd
  pd serve|inspect|status
                        Run the placement driver, print what a stopped one has
                        stored, or ask a running one what it is doing
  region <verb> ...     Look at, split, or hand over a region
  sst-store reconcile <url>
                        Compare an SST store prefix against a database's manifest
                        and say what nothing references any more
  durability record|chaos|verify
                        Prove no acknowledged write is lost when a store is
                        killed (--pd, --out/--in, --clients, --for, --every,
                        --pids, --keyspace)

Options:
  -V, --version         Print the version
  -h, --help            Print this message

Bench options:
  <workload>            fillseq | fillrandom | overwrite | readrandom | readmissing
                        | readseq | scanrange | txnput | txnget | tso | allocid
                        (default fillrandom)
      --num N           Keys in the database, and operations measured (default 100000)
      --value-size N    Value size in bytes (default 100)
      --batch-size N    Entries per write batch; entries read per scan for scanrange;
                        for tso and allocid, values per
                        call to the placement driver (default 1)
      --threads N       Concurrent workers; readseq always uses one (default 1)
      --sync            Wait for each write to be durable (default off)
      --dir PATH        Where to put the database (default a temporary directory)
      --duration-secs N Stop the measured phase early after this long (default 0, no limit)
      --bloom-bits N    Bloom filter bits per key; 0 builds none (default 10)
      --sst-store URL   Tier SSTs into s3://bucket/prefix; the endpoint and credentials
                        come from ESKER_S3_ENDPOINT, ESKER_S3_KEY, ESKER_S3_SECRET and
                        ESKER_S3_REGION, never from a flag
      --sst-cache-bytes N
                        Local SST bytes the tier may keep; 0 is a cold cache. Needs
                        --sst-store
      --write-buffer-size N
                        Memtable bytes before a flush (default 64 MiB). Small values
                        make a fill produce many L0 files
      --target-file-size N
                        Bytes per compaction output file. This is what decides how
                        many files a level below L0 holds
      --block-size N    Bytes per SST data block (default 4 KiB). With --sst-store
                        this is the round-trip granularity of a cold read: one
                        ranged GET per block
      --compact         Compact the whole database before the measured phase, so the
                        read workload runs against levels rather than against L0.
                        Untimed, like the fill
      --adopt-sst-store Claim an --sst-store prefix that already holds objects but no
                        claim marker, instead of refusing. A benchmark's database is a
                        temporary directory, so its claim id is new every run and every
                        re-run against a named prefix meets objects it did not write.
                        Off by default: a benchmark pointed at a stale prefix should get
                        a fresh one
      --pd HOST:PORT    Drive the workload through a placement driver, which routes
                        every key to whichever store holds it. This is how a
                        cluster is measured; --remote names one store and routes
                        nothing, and the two are refused together
      --remote HOST:PORT  Drive the workload over the network against a running
                        server instead of an in-process database. The engine
                        options above belong to that server and are ignored.
                        Required for txnput and txnget, which speak TxnKv to a
                        store: a transaction's decisions happen at apply and
                        there is no in-process form of one. Refused for tso and
                        allocid, which measure a placement driver in this
                        process.

Bench-mpp options:
      --stores N        Stores in the cluster (default 6). Four is the minimum: a
                        region has three voters and a columnar learner goes on a
                        store with no peer of it
      --rows N          Rows in the fact table (default 2000000)
      --groups-high N   Distinct values of the high-cardinality grouping key
                        (default 100000). Groups approaching the row count is the
                        case a two-level aggregate finishes worst
      --region-split-size N
                        Approximate region bytes before a leader splits (default
                        32 MiB). This is what chooses the number of fragments: one
                        per region
      --repeats N       Timed repeats of the whole interleaved set (default 3)
      --region-heartbeat-ms N, --heartbeat-tick-ms N
                        How often a region's leader reports to PD, and the tick that
                        interval is counted in (default 2000 / 250; the shipped
                        defaults are 60000 / 1000). This decides how fast placement
                        happens and how often PD acts on a region
      --batch N         Rows per INSERT during the load (default 500)
      --base-port P     The lowest port the cluster uses (default 24160); the driver
                        sits above the stores and the SQL node above that
      --seed N          Seed for the generated values (default 20260904)
      --dir PATH        Where the cluster's data lives (default a temporary directory)
      --keep            Keep the data directory after the run
      --diagnose        Report what each statement does on each engine, and the region
                        map it ran against, instead of timing anything. Never fails on a
                        statement: what the path does across a region boundary is the
                        result
      --no-join         Leave the join out of the timed set. It costs tens of seconds
                        where the aggregates cost tens of milliseconds and cannot reach
                        the columnar path at all, so it is the first thing to drop under
                        a time budget

Linux only: the decomposition is read from /proc, per process. It needs an
esker-sql binary beside this one.

Sst-dump options:
  -v, --verbose         Print every key and value, not just the summary
      --prefix-len N    Rebuild a StripSuffix prefix extractor of this length, so
                        that a prefix-built bloom filter can be used

Server options:
      --data-dir PATH   Where the database lives; created if absent
                        (default ./esker-data)
      --listen HOST:PORT  Address to serve on (default 127.0.0.1:20160)
      --store-id N      This store's id, reported in the handshake (default 1)
      --peer-id N       This store's Raft peer id (default: the store id)
      --peer ID@ADDR    A peer of the region, repeatable, this store's included
      --write-buffer-size N
                        Memtable bytes before a flush (default 64 MiB)
      --region-split-size N
                        Approximate region bytes past which a leader looks for a split
                        key (default 96 MiB). A store with no --pd never splits,
                        whatever this says: a split needs cluster-unique ids
      --store-heartbeat-ms N
                        How often this store reports itself to PD (default 10000)
      --region-heartbeat-ms N
                        How often each region's leader reports it absent a change
                        (default 60000). Also the latency of an operator: PD answers
                        a region heartbeat and has no other way to reach a store
      --heartbeat-tick-ms N
                        The resolution of the two intervals above, which are counted
                        in these ticks (default 1000). An interval below one tick is
                        rounded up to one, so shortening an interval without also
                        shortening the tick does nothing
      --sst-store URL   Tier this store's SSTs into s3://bucket/prefix, keeping the
                        WAL and the Raft log local. The endpoint and credentials
                        come from ESKER_S3_ENDPOINT, ESKER_S3_KEY, ESKER_S3_SECRET
                        and ESKER_S3_REGION, never from a flag. One prefix per
                        store: the first database to open one claims it, and any
                        other is refused at startup rather than overwriting it
      --adopt-sst-store Claim an --sst-store prefix that already holds objects but
                        no claim marker, instead of refusing. Only do this when you
                        know no other database is using those objects
      --rpc-tls-cert PATH, --rpc-tls-key PATH, --rpc-tls-ca PATH
                        Speak TLS on the RPC port: this store's PEM certificate and
                        key, and the roots that verify whoever connects. All three
                        or none — two of them is refused rather than serving in the
                        clear on a port you believe is encrypted (ADR 0055). Needs a
                        binary built with --features tls
      --rpc-tls-mutual  Also require a certificate from whoever connects, and present
                        one when connecting out. For links where both ends are yours.
                        It authenticates the peer; it does not yet authorise it
      --pd LIST         The placement driver to register with and report to, as one
                        HOST:PORT or several separated by commas — a placement
                        driver is a Raft group of up to three and only its leader
                        answers, so a store given all three follows the redirect
                        when one takes over (ADR 0059). With
                        one, PD decides which store creates region 1 and this store
                        reports its regions on the schedule of DESIGN.md §14.
                        Without one, the store bootstraps a region of its own and
                        reports to nobody, which is what a single node wants

Ctrl-C stops the listener, lets in-flight requests finish and closes the
database. A second one does not wait.

Pd options:
  pd serve                  Run the placement driver. --id and --peers make it one
                            member of a group of three; without them it is the single
                            durable placement driver, which is a single point of failure
  pd members                Ask any member who is in its group and which one leads.
                            Answered by a follower too, which is the point: it is what
                            you reach for when the leader is what is missing
  pd members add ID@ADDR    Add a placement driver. It joins as a LEARNER, catches up,
                            and is promoted — one step per round trip, so running the
                            command again after any failure picks up where it left off
  pd members remove ID      Remove one. Refused if it would leave the group without a
                            quorum of members it has heard from, or if it is the last
  pd inspect                Print what a **stopped** PD has stored: the cluster, the
                            allocator, the oracle's mark, every store and region, and
                            the operator history
  pd status                 Ask a **running** PD what it has in flight. The in-flight
                            set is memory and dies with the process, so `inspect`
                            cannot show it and this is the only thing that can
      --data-dir PATH       PD's database, for serve and inspect (default ./esker-pd)
      --rpc-tls-cert PATH, --rpc-tls-key PATH, --rpc-tls-ca PATH, --rpc-tls-mutual
                            Speak TLS between the members of the group: this member's
                            PEM certificate and key, and the roots that verify the
                            others. All three or none. --rpc-tls-mutual requires a
                            certificate from peers as well as presenting one, which is
                            what a group whose every end is yours should use. Needs a
                            binary built with --features tls (ADR 0055)
      --id N                This member's id in its group, for serve (default 1)
      --peers LIST          The whole group as id@host:port, comma-separated, this
                            member included, when FOUNDING a group. Every member is
                            given the SAME list: the group's id is derived from it once
                            and then written down, so two members given different lists
                            found two groups and will not talk. Ignored once this member
                            has a database — after a change the flag is the stale half
      --join ADDR           Join an existing group instead of founding one: ask the
                            member at ADDR for the group's id and members. Run
                            `pd members add` against the group first
      --listen HOST:PORT    Address to serve on, for serve (default 127.0.0.1:2379)
      --pd HOST:PORT        The placement driver to ask, for status and members
                            (default 127.0.0.1:2379)

Sst-store options:
  sst-store reconcile s3://bucket/prefix
                            List the prefix, compare it against the manifest in
                            --data-dir, and print what nothing references. A
                            DeleteObject that failed leaks its object on purpose;
                            this is where the leak is found. Offline: run it
                            against a database that is not running
      --data-dir PATH       The database whose manifest says what is live
                            (default .)
      --delete              Actually remove what is unreferenced. Without it
                            nothing is deleted. Never removes an object newer
                            than the manifest, and refuses a prefix whose claim
                            marker names another database

Region options:
  region ls                 Print every region in the cluster, in key order
  region split <key>        Split the region covering <key>, at <key>
  region transfer-leader <region-id> <peer-id>
                            Hand a region's leadership to one of its peers
      --pd HOST:PORT        The placement driver to route through
                            (default 127.0.0.1:2379)
      --hex                 Read <key> as hex, and print keys as hex

Routing goes through the placement driver; the work goes to the region's
leader, which refuses rather than forwarding if it is not the leader. A
transfer is *asked for*: what completes it is an election.

Raw options:
  raw get <key>             Print the value, or exit 1 if the key is absent
  raw put <key> <value>     Write one key
  raw delete <key>          Remove one key; removing an absent key succeeds
  raw scan [<start>]        Print key<TAB>value for a run of keys
      --addr HOST:PORT      A store to talk to (default 127.0.0.1:20160). Repeat it
                            once per node of a replicated region, so a NotLeader
                            redirect has an address to follow
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
        "bench-mpp" => parse_bench_mpp(&arguments[1..]),
        "sst-dump" => parse_sst_dump(&arguments[1..]),
        "wal-dump" => parse_wal_dump(&arguments[1..]),
        "manifest-dump" => parse_manifest_dump(&arguments[1..]),
        "raw" => parse_raw(&arguments[1..]),
        "server" => parse_server(&arguments[1..]),
        "cluster" => parse_cluster(&arguments[1..]),
        "pd" => parse_pd(&arguments[1..]),
        "region" => parse_region(&arguments[1..]),
        "sst-store" => parse_sst_store(&arguments[1..]),
        "durability" => parse_durability(&arguments[1..]),
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
        if flag == "--pd" {
            options.pd = Some(take_value(arguments, &mut index, inline, "--pd")?);
            continue;
        }
        if flag == "--sst-store" {
            options.sst_store = Some(take_value(arguments, &mut index, inline, "--sst-store")?);
            continue;
        }
        if flag == "--adopt-sst-store" {
            options.adopt_sst_store = true;
            continue;
        }
        if flag == "--compact" {
            options.compact = true;
            continue;
        }
        if let Some(name) = shape_flag(flag) {
            let raw = take_value(arguments, &mut index, inline, name)?;
            apply_shape_flag(&mut options, name, &raw)?;
            continue;
        }

        if flag == "--sst-cache-bytes" {
            let raw = take_value(arguments, &mut index, inline, "--sst-cache-bytes")?;
            // Zero is the point of the flag — it is what makes the cache cold — so this
            // parses a plain `u64` and does not reject it.
            options.sst_cache_bytes = Some(raw.parse().map_err(|_| ParseError::InvalidValue {
                flag: "--sst-cache-bytes",
                value: raw.clone(),
            })?);
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

/// Sets one of the shape flags. Split out of [`parse_bench`] only for its length.
fn apply_shape_flag(
    options: &mut BenchOptions,
    name: &'static str,
    raw: &str,
) -> Result<(), ParseError> {
    match name {
        "--write-buffer-size" => options.write_buffer_size = Some(positive_bytes(name, raw)?),
        "--target-file-size" => options.target_file_size = Some(positive_bytes(name, raw)?),
        _ => options.block_size = Some(positive_bytes(name, raw)?),
    }
    Ok(())
}

/// The three flags that describe the *shape* of the tree a benchmark builds, rather than the
/// workload run against it. Grouped because they are parsed identically and because a reader
/// looking for "how do I make this database have many files at a level" should find them together.
fn shape_flag(flag: &str) -> Option<&'static str> {
    match flag {
        "--write-buffer-size" => Some("--write-buffer-size"),
        "--target-file-size" => Some("--target-file-size"),
        "--block-size" => Some("--block-size"),
        _ => None,
    }
}

/// A byte count that must be greater than zero.
///
/// Zero is refused rather than clamped: a memtable or an output file of no bytes is not a shape
/// the engine has, so a run asking for one has a typo in it and should be told.
fn positive_bytes<T>(name: &'static str, raw: &str) -> Result<T, ParseError>
where
    T: std::str::FromStr + PartialOrd + Default,
{
    raw.parse::<T>()
        .ok()
        .filter(|bytes| *bytes > T::default())
        .ok_or_else(|| ParseError::InvalidValue {
            flag: name,
            value: raw.to_owned(),
        })
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
/// `esker region ls | split <key> | transfer-leader <region> <peer>`.
fn parse_region(arguments: &[String]) -> Result<Command, ParseError> {
    let Some(verb) = arguments.first() else {
        return Err(ParseError::MissingArgument("a region command"));
    };
    if verb == "--help" || verb == "-h" {
        return Ok(Command::Help);
    }

    let mut options = RegionOptions::default();
    let mut words: Vec<String> = Vec::new();
    let mut index = 1;
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
        match flag {
            "--pd" => options.pd = take_value(arguments, &mut index, inline, "--pd")?,
            "--hex" => options.hex = true,
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            other => words.push(other.to_owned()),
        }
    }

    // `--hex` has to be known before a key is read, which is why the words are collected first
    // and interpreted afterwards rather than as they arrive.
    options.command = match verb.as_str() {
        "ls" | "list" => {
            if !words.is_empty() {
                return Err(ParseError::UnexpectedArgument(words[0].clone()));
            }
            RegionCommand::Ls
        }
        "split" => {
            let [key] = words.as_slice() else {
                return Err(ParseError::MissingArgument("region split <key>"));
            };
            RegionCommand::Split {
                key: read_key(key, options.hex)?,
            }
        }
        "transfer-leader" => {
            let [region, peer] = words.as_slice() else {
                return Err(ParseError::MissingArgument(
                    "region transfer-leader <region-id> <peer-id>",
                ));
            };
            RegionCommand::TransferLeader {
                region_id: positive_id(region, "region-id")?,
                to_peer_id: positive_id(peer, "peer-id")?,
            }
        }
        other => return Err(ParseError::UnknownRegionCommand(other.to_owned())),
    };
    Ok(Command::Region(options))
}

/// A positive id from a bare word, or the error naming which one was wrong.
fn positive_id(word: &str, flag: &'static str) -> Result<u64, ParseError> {
    word.parse()
        .ok()
        .filter(|id| *id > 0)
        .ok_or(ParseError::InvalidValue {
            flag,
            value: word.to_owned(),
        })
}

/// A key from the command line, hex-decoded when `--hex` was given.
fn read_key(word: &str, hex: bool) -> Result<bytes::Bytes, ParseError> {
    if hex {
        from_hex(word)
            .map(bytes::Bytes::from)
            .ok_or_else(|| ParseError::InvalidHex(word.to_owned()))
    } else {
        Ok(bytes::Bytes::copy_from_slice(word.as_bytes()))
    }
}

/// `esker pd serve|inspect [--data-dir PATH] [--listen HOST:PORT]`.
/// `sst-store reconcile <s3://bucket/prefix> --data-dir DIR [--delete]`.
/// `durability record|chaos|verify`, which share their options.
fn parse_durability(arguments: &[String]) -> Result<Command, ParseError> {
    let Some(verb) = arguments.first() else {
        return Err(ParseError::MissingArgument("a durability command"));
    };
    if verb == "--help" || verb == "-h" || verb == "help" {
        return Ok(Command::Help);
    }
    let mut options = crate::durability::DurabilityOptions::new();
    let mut index = 1;
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
        let mut value = |flag: &'static str| -> Result<String, ParseError> {
            if let Some(inline) = inline.clone() {
                return Ok(inline);
            }
            let Some(next) = arguments.get(index) else {
                return Err(ParseError::MissingValue(flag));
            };
            index += 1;
            Ok(next.clone())
        };
        match flag {
            "--pd" => options.pd = value("--pd")?,
            "--out" | "--in" => options.file = value("--out")?,
            "--keyspace" => options.keyspace = value("--keyspace")?,
            "--clients" => {
                options.clients =
                    value("--clients")?
                        .parse()
                        .map_err(|_| ParseError::InvalidValue {
                            flag: "--clients",
                            value: String::new(),
                        })?;
            }
            "--for" => {
                let text = value("--for")?;
                options.run_for = parse_duration(&text).ok_or(ParseError::InvalidValue {
                    flag: "--for",
                    value: text.clone(),
                })?;
            }
            "--every" => {
                let text = value("--every")?;
                options.every = parse_duration(&text).ok_or(ParseError::InvalidValue {
                    flag: "--every",
                    value: text.clone(),
                })?;
            }
            "--pids" => {
                let text = value("--pids")?;
                options.pids = text
                    .split(',')
                    .filter(|part| !part.is_empty())
                    .map(|part| {
                        part.parse::<u32>().map_err(|_| ParseError::InvalidValue {
                            flag: "--pids",
                            value: part.to_owned(),
                        })
                    })
                    .collect::<Result<_, _>>()?;
            }
            other => return Err(ParseError::UnknownFlag(other.to_owned())),
        }
    }
    if options.pd.is_empty() {
        return Err(ParseError::MissingArgument("--pd"));
    }
    match verb.as_str() {
        "record" => Ok(Command::Durability(DurabilityCommand::Record(options))),
        "chaos" => Ok(Command::Durability(DurabilityCommand::Chaos(options))),
        "verify" => Ok(Command::Durability(DurabilityCommand::Verify(options))),
        other => Err(ParseError::UnknownCommand(format!("durability {other}"))),
    }
}

/// `120s`, `2m`, `500ms` — the same shapes `bench --for` takes.
fn parse_duration(text: &str) -> Option<std::time::Duration> {
    let (digits, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    let count: u64 = digits.parse().ok()?;
    match unit {
        "ms" => Some(std::time::Duration::from_millis(count)),
        "s" => Some(std::time::Duration::from_secs(count)),
        "m" => Some(std::time::Duration::from_secs(count * 60)),
        _ => None,
    }
}

fn parse_sst_store(arguments: &[String]) -> Result<Command, ParseError> {
    let Some(verb) = arguments.first() else {
        return Err(ParseError::MissingArgument("an sst-store command"));
    };
    if verb == "--help" || verb == "-h" || verb == "help" {
        return Ok(Command::Help);
    }
    if verb != "reconcile" {
        return Err(ParseError::UnknownCommand(format!("sst-store {verb}")));
    }

    let mut options = crate::reconcile::ReconcileOptions::default();
    let mut url: Option<String> = None;
    let mut index = 1;
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

        match flag {
            "--data-dir" => {
                options.data_dir =
                    PathBuf::from(take_value(arguments, &mut index, inline, "--data-dir")?);
            }
            "--delete" => options.delete = true,
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            other if url.is_none() => url = Some(other.to_owned()),
            other => return Err(ParseError::UnexpectedArgument(other.to_owned())),
        }
    }

    let Some(url) = url else {
        return Err(ParseError::MissingArgument("an s3://bucket/prefix URL"));
    };
    options.store_url = url;
    Ok(Command::SstStore(SstStoreCommand::Reconcile(options)))
}

fn parse_pd(arguments: &[String]) -> Result<Command, ParseError> {
    let Some(verb) = arguments.first() else {
        return Err(ParseError::MissingArgument("a pd command"));
    };
    if verb == "--help" || verb == "-h" || verb == "help" {
        return Ok(Command::Help);
    }

    let mut serve = ServeOptions::default();
    let mut inspect = InspectOptions::default();
    let mut status = StatusOptions::default();
    let mut members = MembersOptions::default();
    let mut words: Vec<String> = Vec::new();
    let mut index = 1;
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

        match flag {
            "--data-dir" => {
                let path = PathBuf::from(take_value(arguments, &mut index, inline, "--data-dir")?);
                serve.data_dir.clone_from(&path);
                inspect.data_dir = path;
            }
            "--listen" => {
                serve.listen = take_value(arguments, &mut index, inline, "--listen")?;
            }
            "--id" => {
                let raw = take_value(arguments, &mut index, inline, "--id")?;
                serve.id = raw.parse().map_err(|_| ParseError::InvalidValue {
                    flag: "--id",
                    value: raw.clone(),
                })?;
                if serve.id == 0 {
                    return Err(ParseError::InvalidValue {
                        flag: "--id",
                        value: raw,
                    });
                }
            }
            "--rpc-tls-cert" => {
                serve.tls.cert =
                    Some(take_value(arguments, &mut index, inline, "--rpc-tls-cert")?.into());
            }
            "--rpc-tls-key" => {
                serve.tls.key =
                    Some(take_value(arguments, &mut index, inline, "--rpc-tls-key")?.into());
            }
            "--rpc-tls-ca" => {
                serve.tls.ca =
                    Some(take_value(arguments, &mut index, inline, "--rpc-tls-ca")?.into());
            }
            "--rpc-tls-mutual" => serve.tls.mutual = true,
            "--peers" => {
                serve.peers = take_value(arguments, &mut index, inline, "--peers")?;
            }
            "--join" => {
                serve.join = take_value(arguments, &mut index, inline, "--join")?;
            }
            "--pd" => {
                let value = take_value(arguments, &mut index, inline, "--pd")?;
                status.pd.clone_from(&value);
                members.pd = value;
            }
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            // `members add 4@host:port` and `members remove 4`: two bare words after the verb.
            // Positional rather than flags, because they are what the command is *about* and a
            // `--id` beside a `--address` reads like two independent options rather than one
            // member.
            other => words.push(other.to_owned()),
        }
    }

    if verb == "members" {
        members.change = parse_member_change(&words)?;
    } else if let Some(stray) = words.first() {
        return Err(ParseError::UnexpectedArgument(stray.clone()));
    }

    match verb.as_str() {
        "serve" => Ok(Command::Pd(PdCommand::Serve(serve))),
        "inspect" => Ok(Command::Pd(PdCommand::Inspect(inspect))),
        "status" => Ok(Command::Pd(PdCommand::Status(status))),
        "members" => Ok(Command::Pd(PdCommand::Members(members))),
        other => Err(ParseError::UnknownPdCommand(other.to_owned())),
    }
}

/// Reads `add <id>@<host:port>` or `remove <id>` after `pd members`, or `None` for a listing.
fn parse_member_change(words: &[String]) -> Result<Option<MembersChange>, ParseError> {
    let Some(verb) = words.first() else {
        return Ok(None);
    };
    let Some(subject) = words.get(1) else {
        return Err(ParseError::MissingArgument(
            "a member, as `id@host:port` to add or `id` to remove",
        ));
    };
    if let Some(stray) = words.get(2) {
        return Err(ParseError::UnexpectedArgument(stray.clone()));
    }
    match verb.as_str() {
        "add" => {
            let mut parsed =
                crate::pd::parse_peers(subject).map_err(|_| ParseError::InvalidValue {
                    flag: "members add",
                    value: subject.clone(),
                })?;
            match parsed.len() {
                1 => Ok(Some(MembersChange::Add(parsed.remove(0)))),
                // One member at a time, and it is not a limitation of the parser: `esker-raft`
                // makes single-server changes, and two at once is what produces two disjoint
                // majorities (dissertation §4.1).
                _ => Err(ParseError::InvalidValue {
                    flag: "members add",
                    value: subject.clone(),
                }),
            }
        }
        "remove" => subject
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .map(|id| Some(MembersChange::Remove(id)))
            .ok_or_else(|| ParseError::InvalidValue {
                flag: "members remove",
                value: subject.clone(),
            }),
        other => Err(ParseError::UnknownPdCommand(format!("members {other}"))),
    }
}

/// Sets one of `esker server`'s numeric knobs, refusing zero.
///
/// Zero is a typo rather than "the default": a split size of zero would ask a leader to split every
/// region for ever, and a heartbeat interval of zero would beat on every tick.
fn set_server_knob(
    options: &mut ServerOptions,
    flag: &'static str,
    raw: &str,
) -> Result<(), ParseError> {
    let value = Some(positive_u64(raw, flag)?);
    match flag {
        "--region-split-size" => options.region_split_size = value,
        "--store-heartbeat-ms" => options.store_heartbeat_ms = value,
        "--region-heartbeat-ms" => options.region_heartbeat_ms = value,
        _ => options.heartbeat_tick_ms = value,
    }
    Ok(())
}

/// The `'static` name of one of `esker server`'s numeric knobs.
///
/// `ParseError::InvalidValue` carries a `&'static str`, and `flag` here is borrowed from the
/// argument vector — so the arm that handles four flags at once has to map back to the literal.
fn server_knob(flag: &str) -> &'static str {
    match flag {
        "--region-split-size" => "--region-split-size",
        "--store-heartbeat-ms" => "--store-heartbeat-ms",
        "--region-heartbeat-ms" => "--region-heartbeat-ms",
        _ => "--heartbeat-tick-ms",
    }
}

/// A `u64` flag value that must be above zero.
fn positive_u64(raw: &str, flag: &'static str) -> Result<u64, ParseError> {
    raw.parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(ParseError::InvalidValue {
            flag,
            value: raw.to_owned(),
        })
}

/// A flat dispatch over eighteen flags, which is the shape it should be: grouping them into
/// helpers to satisfy a line count would put the flag and what it sets in two different places.
#[allow(clippy::too_many_lines)]
fn parse_server(arguments: &[String]) -> Result<Command, ParseError> {
    let mut options = ServerOptions::default();
    let mut index = 0;
    // `--store-id` sets the peer id too unless `--peer-id` was given explicitly, so a
    // single-region cluster needs one flag rather than two.
    let mut saw_peer_id = false;

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

        match flag {
            "--data-dir" => {
                options.data_dir =
                    PathBuf::from(take_value(arguments, &mut index, inline, "--data-dir")?);
            }
            "--listen" => {
                options.listen = take_value(arguments, &mut index, inline, "--listen")?;
            }
            "--sst-store" => {
                options.sst_store = Some(take_value(arguments, &mut index, inline, "--sst-store")?);
            }
            // A bare switch: nothing to configure, only a decision to make out loud.
            "--adopt-sst-store" => options.adopt_sst_store = true,
            "--write-buffer-size" => {
                let raw = take_value(arguments, &mut index, inline, "--write-buffer-size")?;
                options.write_buffer_size = Some(raw.parse().ok().filter(|size| *size > 0).ok_or(
                    ParseError::InvalidValue {
                        flag: "--write-buffer-size",
                        value: raw.clone(),
                    },
                )?);
            }
            // The four `StoreOptions` knobs `docs/bench/phase-4.md` had to wrap this binary to
            // reach. Each takes the same shape, so they take one arm.
            "--region-split-size"
            | "--store-heartbeat-ms"
            | "--region-heartbeat-ms"
            | "--heartbeat-tick-ms" => {
                let named = server_knob(flag);
                let raw = take_value(arguments, &mut index, inline, named)?;
                set_server_knob(&mut options, named, &raw)?;
            }
            "--store-id" => {
                let raw = take_value(arguments, &mut index, inline, "--store-id")?;
                options.store_id =
                    raw.parse()
                        .ok()
                        .filter(|id| *id > 0)
                        .ok_or(ParseError::InvalidValue {
                            flag: "--store-id",
                            value: raw.clone(),
                        })?;
                if !saw_peer_id {
                    options.peer_id = options.store_id;
                }
            }
            "--peer-id" => {
                let raw = take_value(arguments, &mut index, inline, "--peer-id")?;
                options.peer_id =
                    raw.parse()
                        .ok()
                        .filter(|id| *id > 0)
                        .ok_or(ParseError::InvalidValue {
                            flag: "--peer-id",
                            value: raw.clone(),
                        })?;
                saw_peer_id = true;
            }
            "--peer" => {
                // `id@address`, repeatable. Splitting on the last `@` rather than the first
                // leaves an IPv6 address usable, since those contain colons but no `@`.
                let raw = take_value(arguments, &mut index, inline, "--peer")?;
                let (id, address) =
                    raw.split_once('@')
                        .ok_or_else(|| ParseError::InvalidValue {
                            flag: "--peer",
                            value: raw.clone(),
                        })?;
                let id: u64 = id.parse().ok().filter(|id| *id > 0).ok_or_else(|| {
                    ParseError::InvalidValue {
                        flag: "--peer",
                        value: raw.clone(),
                    }
                })?;
                options.peers.push((id, address.to_owned()));
            }
            "--seed" => {
                let raw = take_value(arguments, &mut index, inline, "--seed")?;
                options.seed = raw.parse().map_err(|_| ParseError::InvalidValue {
                    flag: "--seed",
                    value: raw.clone(),
                })?;
            }
            "--rpc-tls-cert" => {
                options.tls.cert =
                    Some(take_value(arguments, &mut index, inline, "--rpc-tls-cert")?.into());
            }
            "--rpc-tls-key" => {
                options.tls.key =
                    Some(take_value(arguments, &mut index, inline, "--rpc-tls-key")?.into());
            }
            "--rpc-tls-ca" => {
                options.tls.ca =
                    Some(take_value(arguments, &mut index, inline, "--rpc-tls-ca")?.into());
            }
            "--rpc-tls-mutual" => options.tls.mutual = true,
            "--pd" => {
                let raw = take_value(arguments, &mut index, inline, "--pd")?;
                if raw.is_empty() {
                    return Err(ParseError::InvalidValue {
                        flag: "--pd",
                        value: raw.clone(),
                    });
                }
                options.pd = Some(raw);
            }
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            other => return Err(ParseError::UnexpectedArgument(other.to_owned())),
        }
    }

    Ok(Command::Server(options))
}

/// `esker bench-mpp [--stores N] [--rows N] …`.
fn parse_bench_mpp(arguments: &[String]) -> Result<Command, ParseError> {
    let mut options = BenchMppOptions::default();
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
        // Every count here must be positive: a zero would be a run with no rows, no repeats or no
        // stores, which is a configuration error dressed as an empty report.
        let positive = |flag: &'static str, index: &mut usize| -> Result<u64, ParseError> {
            let raw = take_value(arguments, index, inline.clone(), flag)?;
            raw.parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or(ParseError::InvalidValue {
                    flag,
                    value: raw.clone(),
                })
        };
        match flag {
            "--stores" => options.stores = positive("--stores", &mut index)?,
            "--rows" => options.rows = positive("--rows", &mut index)?,
            "--groups-high" => options.groups_high = positive("--groups-high", &mut index)?,
            "--region-split-size" => {
                options.region_split_size = positive("--region-split-size", &mut index)?;
            }
            "--repeats" => options.repeats = positive("--repeats", &mut index)?,
            "--region-heartbeat-ms" => {
                options.region_heartbeat_ms = positive("--region-heartbeat-ms", &mut index)?;
            }
            "--heartbeat-tick-ms" => {
                options.heartbeat_tick_ms = positive("--heartbeat-tick-ms", &mut index)?;
            }
            "--batch" => options.batch = positive("--batch", &mut index)?,
            "--seed" => {
                let raw = take_value(arguments, &mut index, inline, "--seed")?;
                options.seed = raw.parse().map_err(|_| ParseError::InvalidValue {
                    flag: "--seed",
                    value: raw.clone(),
                })?;
            }
            "--base-port" => {
                let raw = take_value(arguments, &mut index, inline, "--base-port")?;
                options.base_port = raw.parse().map_err(|_| ParseError::InvalidValue {
                    flag: "--base-port",
                    value: raw.clone(),
                })?;
            }
            "--dir" => {
                options.dir = Some(PathBuf::from(take_value(
                    arguments, &mut index, inline, "--dir",
                )?));
            }
            "--keep" => options.keep = true,
            "--no-join" => options.no_join = true,
            "--diagnose" => options.diagnose = true,
            other => return Err(ParseError::UnknownFlag(other.to_owned())),
        }
    }
    Ok(Command::BenchMpp(options))
}

fn parse_cluster(arguments: &[String]) -> Result<Command, ParseError> {
    let Some(action) = arguments.first() else {
        return Err(ParseError::MissingArgument("cluster <start|stop>"));
    };
    if action == "--help" || action == "-h" {
        return Ok(Command::Help);
    }

    let rest = &arguments[1..];
    let mut nodes = 3_u64;
    let mut data_dir = PathBuf::from("esker-cluster");
    let mut base_port = crate::cluster::DEFAULT_BASE_PORT;
    let mut seed = 0_u64;
    let mut sst_store: Option<String> = None;
    let mut write_buffer_size: Option<usize> = None;
    let mut pd = false;
    let mut index = 0;

    while index < rest.len() {
        let argument = &rest[index];
        index += 1;
        if argument == "--help" || argument == "-h" {
            return Ok(Command::Help);
        }
        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (argument.as_str(), None),
        };
        match flag {
            "--nodes" => {
                let raw = take_value(rest, &mut index, inline, "--nodes")?;
                nodes = raw.parse().ok().filter(|count| *count > 0).ok_or(
                    ParseError::InvalidValue {
                        flag: "--nodes",
                        value: raw.clone(),
                    },
                )?;
            }
            "--data-dir" => {
                data_dir = PathBuf::from(take_value(rest, &mut index, inline, "--data-dir")?);
            }
            "--base-port" => {
                let raw = take_value(rest, &mut index, inline, "--base-port")?;
                base_port = raw.parse().map_err(|_| ParseError::InvalidValue {
                    flag: "--base-port",
                    value: raw.clone(),
                })?;
            }
            "--seed" => {
                let raw = take_value(rest, &mut index, inline, "--seed")?;
                seed = raw.parse().map_err(|_| ParseError::InvalidValue {
                    flag: "--seed",
                    value: raw.clone(),
                })?;
            }
            "--sst-store" => {
                sst_store = Some(take_value(rest, &mut index, inline, "--sst-store")?);
            }
            "--write-buffer-size" => {
                let raw = take_value(rest, &mut index, inline, "--write-buffer-size")?;
                write_buffer_size = Some(raw.parse().ok().filter(|size| *size > 0).ok_or(
                    ParseError::InvalidValue {
                        flag: "--write-buffer-size",
                        value: raw.clone(),
                    },
                )?);
            }
            // A switch, not an address: the port is derived from `--base-port` so that it
            // cannot collide with the nodes', and it is printed. A cluster this command starts is
            // one it also has to be able to stop.
            "--pd" => pd = true,
            other if other.starts_with('-') => {
                return Err(ParseError::UnknownFlag(other.to_owned()));
            }
            other => return Err(ParseError::UnexpectedArgument(other.to_owned())),
        }
    }

    match action.as_str() {
        "start" => Ok(Command::Cluster(ClusterOptions::Start {
            nodes,
            data_dir,
            base_port,
            seed,
            sst_store,
            write_buffer_size,
            pd,
        })),
        "stop" => Ok(Command::Cluster(ClusterOptions::Stop { data_dir })),
        other => Err(ParseError::UnknownCommand(format!("cluster {other}"))),
    }
}

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
    // The first `--addr` is the store to try; every later one is a redirect target.
    let mut saw_addr = false;
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
            "--addr" => {
                // Repeatable: the first is the store to try, the rest are where a redirect may
                // send the request. A replicated region needs the whole list, because a
                // `NotLeader` hint names a peer and reaching it needs an address.
                let value = take_value(arguments, &mut index, inline, "--addr")?;
                if saw_addr {
                    options.extra_addrs.push(value);
                } else {
                    options.addr = value;
                    saw_addr = true;
                }
            }
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

    /// The 4a shape has to keep meaning what it meant: no `--id`, no `--peers`, one member.
    #[test]
    fn pd_serve_without_a_group_is_still_the_single_placement_driver() {
        let Command::Pd(PdCommand::Serve(serve)) = parse_ok(&["pd", "serve"]) else {
            panic!("pd serve did not parse");
        };
        assert_eq!(serve.id, 1);
        assert!(serve.peers.is_empty());
        assert_eq!(serve.listen, crate::pd::DEFAULT_LISTEN);
    }

    #[test]
    fn pd_serve_takes_a_member_id_and_the_whole_group() {
        let Command::Pd(PdCommand::Serve(serve)) = parse_ok(&[
            "pd",
            "serve",
            "--id",
            "2",
            "--peers",
            "1@127.0.0.1:2379,2@127.0.0.1:2380,3@127.0.0.1:2381",
            "--listen",
            "127.0.0.1:2380",
        ]) else {
            panic!("pd serve did not parse");
        };
        assert_eq!(serve.id, 2);
        assert_eq!(serve.listen, "127.0.0.1:2380");
        let members = crate::pd::parse_peers(&serve.peers).unwrap();
        assert_eq!(members.len(), 3);
        assert_eq!(members[1].id, 2);
        assert_eq!(members[1].address, "127.0.0.1:2380");
    }

    /// Member id zero is `esker-raft`'s "no node", so a typo must be refused rather than
    /// producing a member nothing can address.
    #[test]
    fn a_member_id_of_zero_is_refused() {
        assert!(parse(["pd", "serve", "--id", "0"].into_iter()).is_err());
        assert!(parse(["pd", "serve", "--id", "two"].into_iter()).is_err());
    }

    #[test]
    fn pd_members_asks_a_running_placement_driver() {
        let Command::Pd(PdCommand::Members(members)) =
            parse_ok(&["pd", "members", "--pd", "10.0.0.1:2379"])
        else {
            panic!("pd members did not parse");
        };
        assert_eq!(members.pd, "10.0.0.1:2379");
        assert_eq!(members.change, None, "listing is not a change");
        assert!(parse(["pd", "wat"].into_iter()).is_err());
    }

    #[test]
    fn pd_members_adds_and_removes_one_member() {
        let Command::Pd(PdCommand::Members(add)) = parse_ok(&[
            "pd",
            "members",
            "add",
            "4@127.0.0.1:2382",
            "--pd",
            "127.0.0.1:2379,127.0.0.1:2380",
        ]) else {
            panic!("pd members add did not parse");
        };
        assert_eq!(add.pd, "127.0.0.1:2379,127.0.0.1:2380");
        let Some(MembersChange::Add(member)) = add.change else {
            panic!("pd members add parsed as something else");
        };
        assert_eq!(member.id, 4);
        assert_eq!(member.address, "127.0.0.1:2382");

        let Command::Pd(PdCommand::Members(remove)) = parse_ok(&["pd", "members", "remove", "2"])
        else {
            panic!("pd members remove did not parse");
        };
        assert_eq!(remove.change, Some(MembersChange::Remove(2)));
    }

    /// **One member at a time**, and it is not a limitation of the parser: `esker-raft` makes
    /// single-server changes, and two at once is what produces two disjoint majorities.
    #[test]
    fn pd_members_refuses_anything_but_one_member() {
        for bad in [
            vec!["pd", "members", "add"],
            vec!["pd", "members", "add", "127.0.0.1:2382"],
            vec!["pd", "members", "add", "4@127.0.0.1:2382,5@127.0.0.1:2383"],
            vec!["pd", "members", "add", "4@127.0.0.1:2382", "5@x:1"],
            vec!["pd", "members", "remove", "0"],
            vec!["pd", "members", "remove", "two"],
            vec!["pd", "members", "wat", "4"],
        ] {
            assert!(
                parse(bad.iter().copied()).is_err(),
                "`{}` was accepted",
                bad.join(" ")
            );
        }
    }

    /// Founding a group and joining one are different acts, and the flags say which.
    #[test]
    fn pd_serve_joins_a_group_it_did_not_found() {
        let Command::Pd(PdCommand::Serve(serve)) =
            parse_ok(&["pd", "serve", "--id", "4", "--join", "127.0.0.1:2379"])
        else {
            panic!("pd serve did not parse");
        };
        assert_eq!(serve.id, 4);
        assert_eq!(serve.join, "127.0.0.1:2379");
        assert!(serve.peers.is_empty(), "a joining member founds nothing");
    }

    /// A `--peers` entry says which member it is; a list whose meaning came from its order would
    /// be two different groups the moment two operators wrote it differently.
    #[test]
    fn a_peers_entry_without_a_member_id_is_refused() {
        assert!(crate::pd::parse_peers("127.0.0.1:2379").is_err());
        assert!(crate::pd::parse_peers("x@127.0.0.1:2379").is_err());
        assert!(crate::pd::parse_peers("").unwrap().is_empty());
    }

    /// `--pd` takes the whole group, and one address still means one address — every existing
    /// invocation has to keep working.
    #[test]
    fn a_server_takes_one_placement_driver_or_several() {
        let Command::Server(one) = parse_ok(&["server", "--pd", "127.0.0.1:2379"]) else {
            panic!("server did not parse");
        };
        assert_eq!(one.pd.as_deref(), Some("127.0.0.1:2379"));

        let Command::Server(three) = parse_ok(&[
            "server",
            "--pd",
            "127.0.0.1:2379,127.0.0.1:2380,127.0.0.1:2381",
        ]) else {
            panic!("server did not parse");
        };
        assert_eq!(
            three.pd.as_deref(),
            Some("127.0.0.1:2379,127.0.0.1:2380,127.0.0.1:2381")
        );
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
        let Command::Bench(options) = parse_ok(&["bench", "scanrange", "--pd", "127.0.0.1:2379"])
        else {
            panic!("not a bench command");
        };
        assert_eq!(options.workload, Workload::ScanRange);
        assert_eq!(options.pd.as_deref(), Some("127.0.0.1:2379"));
        assert_eq!(options.remote, None);

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

    /// The tiering flags, in both forms. `--sst-cache-bytes 0` is the cold-cache run
    /// `docs/bench/phase-6b.md` records, so zero has to parse rather than be rejected as a
    /// nonsense size the way `--threads 0` is.
    #[test]
    fn bench_takes_the_tiering_flags() {
        let Command::Bench(options) = parse_ok(&[
            "bench",
            "readrandom",
            "--sst-store=s3://esker/tier",
            "--sst-cache-bytes",
            "0",
        ]) else {
            panic!("expected a bench command");
        };
        assert_eq!(options.sst_store.as_deref(), Some("s3://esker/tier"));

        assert_eq!(options.sst_cache_bytes, Some(0));

        let Command::Bench(options) = parse_ok(&["bench", "--sst-cache-bytes=1048576"]) else {
            panic!("expected a bench command");
        };
        assert_eq!(options.sst_cache_bytes, Some(1024 * 1024));

        // Absent means local SSTs, which is every run before this phase.
        let Command::Bench(options) = parse_ok(&["bench"]) else {
            panic!("expected a bench command");
        };
        assert_eq!(options.sst_store, None);
        assert_eq!(options.sst_cache_bytes, None);

        assert_eq!(
            parse(["bench", "--sst-cache-bytes", "lots"]),
            Err(ParseError::InvalidValue {
                flag: "--sst-cache-bytes",
                value: "lots".to_owned()
            })
        );
        assert_eq!(
            parse(["bench", "--sst-store"]),
            Err(ParseError::MissingValue("--sst-store"))
        );
    }

    /// `bench` has the same switch as `server`, with the same default.
    ///
    /// Without it the refusal names a flag the command did not have, which is a dead end with
    /// instructions on it — and it bites `bench` hardest, because a benchmark's database is a
    /// temporary directory and its claim id is therefore new on every run.
    #[test]
    fn the_bench_takes_the_same_adoption_switch_as_the_server() {
        let Command::Bench(options) = parse_ok(&["bench", "--sst-store", "s3://esker/tier"]) else {
            panic!("expected a bench command");
        };
        assert!(
            !options.adopt_sst_store,
            "a benchmark adopts somebody else's objects by default"
        );
        let Command::Bench(options) =
            parse_ok(&["bench", "--sst-store=s3://esker/tier", "--adopt-sst-store"])
        else {
            panic!("expected a bench command");
        };
        assert!(options.adopt_sst_store);
    }

    /// `--sst-store` on a server, and on a cluster where every node must get a *different*
    /// prefix — two databases sharing one silently overwrite each other's `000007.sst`.
    #[test]
    fn the_server_and_the_cluster_both_take_a_store_url() {
        let Command::Server(options) = parse_ok(&["server", "--sst-store", "s3://esker/tier"])
        else {
            panic!("expected a server command");
        };
        assert_eq!(options.sst_store.as_deref(), Some("s3://esker/tier"));
        assert!(
            !options.adopt_sst_store,
            "adopting somebody else's objects is never the default"
        );

        // The escape hatch is a bare switch, and it has to be asked for.
        let Command::Server(options) = parse_ok(&[
            "server",
            "--sst-store",
            "s3://esker/tier",
            "--adopt-sst-store",
        ]) else {
            panic!("expected a server command");
        };
        assert!(options.adopt_sst_store);

        let Command::Server(options) = parse_ok(&["server", "--write-buffer-size", "262144"])
        else {
            panic!("expected a server command");
        };
        assert_eq!(options.write_buffer_size, Some(256 * 1024));
        assert_eq!(
            parse(["server", "--write-buffer-size", "0"]),
            Err(ParseError::InvalidValue {
                flag: "--write-buffer-size",
                value: "0".to_owned()
            }),
            "a zero-byte memtable would flush forever"
        );

        let Command::Server(options) = parse_ok(&["server"]) else {
            panic!("expected a server command");
        };
        assert_eq!(options.sst_store, None, "absent means local SSTs");

        // The four knobs `docs/bench/phase-4.md` had to wrap this binary to reach, in both
        // spellings, because `--flag value` and `--flag=value` are two code paths.
        let Command::Server(options) = parse_ok(&[
            "server",
            "--region-split-size",
            "1048576",
            "--store-heartbeat-ms=2000",
            "--region-heartbeat-ms",
            "1000",
            "--heartbeat-tick-ms=100",
        ]) else {
            panic!("expected a server command");
        };
        assert_eq!(options.region_split_size, Some(1024 * 1024));
        assert_eq!(options.store_heartbeat_ms, Some(2_000));
        assert_eq!(options.region_heartbeat_ms, Some(1_000));
        assert_eq!(options.heartbeat_tick_ms, Some(100));

        // Absent is the store's default, not zero, and each of them refuses a zero: a split size
        // of zero splits for ever and a heartbeat of zero beats on every tick.
        let Command::Server(options) = parse_ok(&["server"]) else {
            panic!("expected a server command");
        };
        assert_eq!(options.region_split_size, None);
        assert_eq!(options.store_heartbeat_ms, None);
        assert_eq!(options.region_heartbeat_ms, None);
        assert_eq!(options.heartbeat_tick_ms, None);
        for flag in [
            "--region-split-size",
            "--store-heartbeat-ms",
            "--region-heartbeat-ms",
            "--heartbeat-tick-ms",
        ] {
            assert!(
                matches!(
                    parse(["server", flag, "0"]),
                    Err(ParseError::InvalidValue { .. })
                ),
                "{flag} accepted zero"
            );
            assert!(
                matches!(
                    parse(["server", flag, "not-a-number"]),
                    Err(ParseError::InvalidValue { .. })
                ),
                "{flag} accepted a word"
            );
        }

        let Command::Cluster(ClusterOptions::Start { sst_store, .. }) =
            parse_ok(&["cluster", "start", "--sst-store=s3://esker/c1"])
        else {
            panic!("expected a cluster start");
        };
        assert_eq!(sst_store.as_deref(), Some("s3://esker/c1"));

        assert_eq!(
            parse(["server", "--sst-store"]),
            Err(ParseError::MissingValue("--sst-store"))
        );
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
    /// A replicated region needs every node's address: the client learns *which* peer leads from
    /// a `NotLeader` hint and needs somewhere to send the retry.
    #[test]
    fn raw_takes_one_address_per_node() {
        let Command::Raw(options) = parse_ok(&[
            "raw",
            "get",
            "k",
            "--addr",
            "127.0.0.1:1",
            "--addr",
            "127.0.0.1:2",
            "--addr=127.0.0.1:3",
        ]) else {
            panic!("expected a raw command");
        };
        assert_eq!(options.addr, "127.0.0.1:1");
        assert_eq!(options.extra_addrs, vec!["127.0.0.1:2", "127.0.0.1:3"]);
    }

    #[test]
    fn raw_still_defaults_to_one_address() {
        let Command::Raw(options) = parse_ok(&["raw", "get", "k"]) else {
            panic!("expected a raw command");
        };
        assert_eq!(options.addr, crate::raw::DEFAULT_ADDR);
        assert!(options.extra_addrs.is_empty());
    }

    /// `--pd` is what turns a cluster of stores that bootstrap their own region into one with a
    /// placement driver — and therefore into one a SQL node can hold a schema lease against.
    #[test]
    fn cluster_start_takes_a_placement_driver() {
        let Command::Cluster(ClusterOptions::Start { pd, base_port, .. }) =
            parse_ok(&["cluster", "start", "--pd"])
        else {
            panic!("expected a cluster start");
        };
        assert!(pd);
        assert_eq!(
            base_port,
            crate::cluster::DEFAULT_BASE_PORT,
            "the driver's own port is derived, so this one does not move",
        );
    }

    #[test]
    fn cluster_start_and_stop_parse() {
        let Command::Cluster(ClusterOptions::Start {
            nodes,
            data_dir,
            base_port,
            seed,
            sst_store,
            write_buffer_size,
            pd,
        }) = parse_ok(&[
            "cluster",
            "start",
            "--nodes",
            "5",
            "--data-dir",
            "/tmp/c",
            "--base-port",
            "30000",
            "--seed=9",
        ])
        else {
            panic!("expected a cluster start");
        };
        assert_eq!((nodes, base_port, seed), (5, 30_000, 9));
        assert_eq!(data_dir, PathBuf::from("/tmp/c"));
        assert_eq!(sst_store, None, "a cluster tiers nothing unless asked");
        assert_eq!(
            write_buffer_size, None,
            "and keeps the engine's memtable size"
        );
        assert!(!pd, "a cluster starts no placement driver unless asked");

        let Command::Cluster(ClusterOptions::Stop { data_dir }) =
            parse_ok(&["cluster", "stop", "--data-dir", "/tmp/c"])
        else {
            panic!("expected a cluster stop");
        };
        assert_eq!(data_dir, PathBuf::from("/tmp/c"));
    }

    #[test]
    fn cluster_defaults_to_three_nodes() {
        let Command::Cluster(ClusterOptions::Start {
            nodes, base_port, ..
        }) = parse_ok(&["cluster", "start"])
        else {
            panic!("expected a cluster start");
        };
        assert_eq!(nodes, 3, "the smallest group that tolerates a failure");
        assert_eq!(base_port, crate::cluster::DEFAULT_BASE_PORT);
    }

    #[test]
    fn a_cluster_action_that_does_not_exist_is_an_error() {
        assert!(parse(&["cluster".to_owned(), "restart".to_owned()]).is_err());
        assert!(parse(&["cluster".to_owned()]).is_err());
        assert!(
            parse(&[
                "cluster".to_owned(),
                "start".to_owned(),
                "--nodes".to_owned(),
                "0".to_owned()
            ])
            .is_err(),
            "a cluster of nothing is not a cluster"
        );
    }

    /// `--store-id` sets the peer id too, so a single-region cluster needs one flag, and an
    /// explicit `--peer-id` still wins.
    #[test]
    fn server_peers_and_ids_parse() {
        let Command::Server(options) = parse_ok(&[
            "server",
            "--store-id",
            "2",
            "--peer",
            "1@127.0.0.1:1",
            "--peer=3@127.0.0.1:3",
            "--seed",
            "7",
        ]) else {
            panic!("expected a server command");
        };
        assert_eq!(options.store_id, 2);
        assert_eq!(options.peer_id, 2, "the store id sets the peer id");
        assert_eq!(options.seed, 7);
        assert_eq!(
            options.peers,
            vec![(1, "127.0.0.1:1".to_owned()), (3, "127.0.0.1:3".to_owned())]
        );

        let Command::Server(explicit) = parse_ok(&["server", "--peer-id", "9", "--store-id", "2"])
        else {
            panic!("expected a server command");
        };
        assert_eq!(
            explicit.peer_id, 9,
            "an explicit peer id is not overwritten"
        );
    }

    /// Every `region` verb, its words, and the two flags that change how a key is read.
    #[test]
    fn region_takes_a_verb_and_the_words_it_needs() {
        let Command::Region(ls) = parse_ok(&["region", "ls"]) else {
            panic!("not a region command");
        };
        assert_eq!(ls.command, RegionCommand::Ls);
        assert_eq!(ls.pd, "127.0.0.1:2379");

        let Command::Region(split) = parse_ok(&["region", "split", "m", "--pd", "10.0.0.1:2379"])
        else {
            panic!("not a region command");
        };
        assert_eq!(
            split.command,
            RegionCommand::Split {
                key: bytes::Bytes::from_static(b"m")
            }
        );
        assert_eq!(split.pd, "10.0.0.1:2379");

        // `--hex` decides how the key is read, and it works whichever side of the word it is on:
        // the words are collected first and interpreted afterwards for exactly this reason.
        for arguments in [
            vec!["region", "split", "--hex", "6d"],
            vec!["region", "split", "6d", "--hex"],
        ] {
            let Command::Region(options) = parse_ok(&arguments) else {
                panic!("not a region command");
            };
            assert_eq!(
                options.command,
                RegionCommand::Split {
                    key: bytes::Bytes::from_static(b"m")
                },
                "{arguments:?}"
            );
        }

        let Command::Region(transfer) = parse_ok(&["region", "transfer-leader", "3", "7"]) else {
            panic!("not a region command");
        };
        assert_eq!(
            transfer.command,
            RegionCommand::TransferLeader {
                region_id: 3,
                to_peer_id: 7
            }
        );
    }

    /// A region command that cannot be carried out is refused here rather than sent: an operator
    /// finds out from their shell, not from a cluster.
    #[test]
    fn region_refuses_what_it_cannot_carry_out() {
        for bad in [
            vec!["region"],
            vec!["region", "nonsense"],
            vec!["region", "split"],
            vec!["region", "transfer-leader", "3"],
            vec!["region", "transfer-leader", "0", "7"],
            vec!["region", "transfer-leader", "3", "0"],
            vec!["region", "transfer-leader", "three", "7"],
            vec!["region", "ls", "extra"],
            vec!["region", "split", "--hex", "odd"],
            vec!["region", "ls", "--nonsense"],
        ] {
            assert!(parse(bad.iter().copied()).is_err(), "{bad:?} was accepted");
        }
        assert_eq!(parse_ok(&["region", "--help"]), Command::Help);
    }

    /// `--pd` is what turns a store from "bootstraps its own region 1" into "asks the placement
    /// driver whether it is the one that should". Absent by default, because that is what phase
    /// 2's single node and phase 3e's static cluster are.
    #[test]
    fn the_placement_driver_address_is_optional_and_must_not_be_empty() {
        let Command::Server(plain) = parse_ok(&["server"]) else {
            panic!("not a server command");
        };
        assert_eq!(plain.pd, None);

        let Command::Server(with_pd) = parse_ok(&["server", "--pd", "127.0.0.1:2379"]) else {
            panic!("not a server command");
        };
        assert_eq!(with_pd.pd.as_deref(), Some("127.0.0.1:2379"));

        let Command::Server(inline) = parse_ok(&["server", "--pd=127.0.0.1:2379"]) else {
            panic!("not a server command");
        };
        assert_eq!(inline.pd.as_deref(), Some("127.0.0.1:2379"));

        assert!(matches!(
            parse(["server", "--pd", ""].iter().copied()),
            Err(ParseError::InvalidValue { flag: "--pd", .. })
        ));
        assert!(parse(["server", "--pd"].iter().copied()).is_err());
    }

    #[test]
    fn a_malformed_peer_is_an_error() {
        for bad in ["noatsign", "0@127.0.0.1:1", "x@127.0.0.1:1"] {
            assert!(
                parse(&["server".to_owned(), "--peer".to_owned(), bad.to_owned()]).is_err(),
                "accepted `--peer {bad}`"
            );
        }
    }
}
