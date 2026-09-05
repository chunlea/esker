//! What each process spent, read from `/proc`.
//!
//! The question this benchmark exists to answer is *what share of a query's wall time is the SQL
//! node's finishing step*, and the SQL node reports nothing: `ScanStats` counts stripes, chunks
//! and rows (`esker_proto::fragment::ScanStats`) and `EXPLAIN ANALYZE` prints those and no clock
//! at all. Rather than add a counter to the crate being measured — a measurement must not change
//! what it measures — the split is read from outside, per process, from the kernel:
//!
//! * **CPU** (`/proc/<pid>/stat`, `utime + stime`). The stores' CPU is the scan; the SQL node's is
//!   planning, fragment encoding, decoding every partial and merging them. A query's wall time
//!   against those two sums is the decomposition, and it needs nothing from either binary.
//! * **Bytes read and written** (`/proc/<pid>/io`, `rchar`/`wchar`). A SQL node holds no engine, so
//!   what it reads is very nearly what arrived from the stores — the number ADR 0022 milestone 5
//!   turns on.
//! * **Peak resident memory** (`/proc/<pid>/status`, `VmHWM`). The two-level finish holds every
//!   region's partials for every group at once; whether that is a bounded cost or the whole answer
//!   materialised is a thing to read rather than assume.
//!
//! # Two things this is not
//!
//! It is **Linux only**, which the container is (`~/workspace/lab/esker-docker`). Every reader
//! returns an error rather than a zero where `/proc` is absent, because a decomposition silently
//! made of zeroes is worse than none.
//!
//! It has **clock-tick resolution**, conventionally 10 ms. That is why the workload is sized so a
//! statement takes something over a second: a decomposition of a 20 ms query out of a 10 ms tick
//! would be noise wearing a table's clothes.

use std::time::Duration;

/// The `USER_HZ` the kernel reports `utime` and `stime` in.
///
/// 100 on every Linux this runs on. It is a constant rather than a `sysconf(_SC_CLK_TCK)` call
/// because reaching that needs `libc`, which is a dependency this project does not have
/// (`CLAUDE.md`) — and being wrong about it would scale every CPU figure by the same factor,
/// which [`Sample::read`]'s own test pins against a known `/proc` layout.
const CLOCK_TICKS_PER_SECOND: u64 = 100;

/// One process's counters at one instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Sample {
    /// `utime + stime`, the process's own CPU.
    pub(crate) cpu: Duration,
    /// `rchar` — bytes this process has read, sockets included.
    pub(crate) read_bytes: u64,
    /// `wchar` — bytes this process has written.
    pub(crate) written_bytes: u64,
    /// `VmHWM` — the highest resident set this process has ever held, in bytes.
    pub(crate) peak_rss_bytes: u64,
}

impl Sample {
    /// Reads every counter for `pid`.
    pub(crate) fn read(pid: u32) -> Result<Self, String> {
        Ok(Self {
            cpu: cpu_of(pid)?,
            read_bytes: counter(pid, "io", "rchar")?,
            written_bytes: counter(pid, "io", "wchar")?,
            // `VmHWM` is in kibibytes and every other figure here is in bytes.
            peak_rss_bytes: counter(pid, "status", "VmHWM")? * 1024,
        })
    }

    /// What happened between `self` and a later sample.
    ///
    /// Saturating throughout: a process that exited mid-measurement reads as zero rather than
    /// underflowing into a number that would look like a very large one.
    pub(crate) fn since(&self, earlier: Self) -> Self {
        Self {
            cpu: self.cpu.saturating_sub(earlier.cpu),
            read_bytes: self.read_bytes.saturating_sub(earlier.read_bytes),
            written_bytes: self.written_bytes.saturating_sub(earlier.written_bytes),
            // A high-water mark is not a rate: the delta of two is how much *further* the peak
            // moved, which is the only reading of it that means anything across an interval.
            peak_rss_bytes: self.peak_rss_bytes.max(earlier.peak_rss_bytes),
        }
    }
}

/// `utime + stime` of `pid`, as a duration.
///
/// The comm field is in parentheses and may itself contain spaces and parentheses, so the fields
/// are counted from the **last** `)` — the parse every reader of this file has to get right.
fn cpu_of(pid: u32) -> Result<Duration, String> {
    let path = format!("/proc/{pid}/stat");
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("reading {path}: {error}"))?;
    let after_comm = text
        .rfind(')')
        .and_then(|at| text.get(at + 1..))
        .ok_or_else(|| format!("{path} has no comm field"))?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Field 3 (state) is the first here, so field N is at index N - 3: utime is 14, stime is 15.
    let ticks = |field: usize, name: &str| -> Result<u64, String> {
        fields
            .get(field - 3)
            .ok_or_else(|| format!("{path} has no {name} field"))?
            .parse::<u64>()
            .map_err(|error| format!("{path}'s {name} is not a number: {error}"))
    };
    let ticks = ticks(14, "utime")? + ticks(15, "stime")?;
    Ok(Duration::from_nanos(
        ticks * (1_000_000_000 / CLOCK_TICKS_PER_SECOND),
    ))
}

/// The first number on the `name` line of `/proc/<pid>/<file>`.
///
/// One reader for `io`'s `rchar: 1234` and `status`'s `VmHWM:  1234 kB`, which differ only in
/// the unit that follows and in nothing this needs.
fn counter(pid: u32, file: &str, name: &str) -> Result<u64, String> {
    let path = format!("/proc/{pid}/{file}");
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("reading {path}: {error}"))?;
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or_else(|| format!("{path} has no {name} line"))?
        .parse::<u64>()
        .map_err(|error| format!("{path}'s {name} is not a number: {error}"))
}

/// Whether this kernel publishes what [`Sample`] reads, named once so a run refuses early.
pub(crate) fn is_available() -> bool {
    std::path::Path::new("/proc/self/io").exists()
}

/// The memory and the CPUs this run had, for the record's header.
///
/// **Both memory figures, because they disagree and the disagreement is the point.**
/// `/proc/meminfo` in a container shows the *host's* (or the VM's) memory, while
/// `/sys/fs/cgroup/memory.max` is what this container may actually use — and a benchmark sized
/// against the wrong one either fits in a cache it does not have or swaps in a cache it does. A
/// limit of `max` means uncapped and says so rather than printing a number nobody set.
pub(crate) fn machine() -> String {
    let total = meminfo_kib("MemTotal").map_or_else(
        |why| format!("MemTotal unreadable ({why})"),
        |kib| format!("MemTotal {}", gibibytes(kib * 1024)),
    );
    let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max").map_or_else(
        |_| "no cgroup limit published".to_owned(),
        |text| match text.trim() {
            "max" => "cgroup limit max (uncapped)".to_owned(),
            number => number.parse::<u64>().map_or_else(
                |_| format!("cgroup limit {number}"),
                |bytes| format!("cgroup limit {}", gibibytes(bytes)),
            ),
        },
    );
    let cpus = std::thread::available_parallelism()
        .map_or_else(|_| "unknown".to_owned(), |count| count.to_string());
    format!("{total}, {limit}, {cpus} CPUs")
}

/// The first number on `/proc/meminfo`'s `name` line, in kibibytes.
fn meminfo_kib(name: &str) -> Result<u64, String> {
    let text = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("reading /proc/meminfo: {error}"))?;
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .and_then(|rest| rest.split_whitespace().next())
        .ok_or_else(|| format!("no {name} line"))?
        .parse::<u64>()
        .map_err(|error| format!("{name} is not a number: {error}"))
}

/// A byte count as gibibytes, to one decimal.
fn gibibytes(bytes: u64) -> String {
    // A machine with more than 2^53 bytes of memory is not one this runs on.
    #[allow(clippy::cast_precision_loss)]
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{gib:.1} GiB")
}

#[cfg(test)]
mod tests {
    use super::{Sample, is_available};

    /// Not `#[ignore]`d: on Linux it asserts a real parse, and elsewhere it asserts the refusal.
    /// A reader that returned zeroes off Linux is the failure this pins.
    #[test]
    fn a_process_reads_as_itself_or_as_a_refusal() {
        let mine = Sample::read(std::process::id());
        if is_available() {
            let Ok(mine) = mine else {
                panic!("/proc/self/io exists and the sample failed: {mine:?}")
            };
            assert!(
                mine.peak_rss_bytes > 0,
                "a running process with no peak resident set: {mine:?}"
            );
        } else {
            assert!(mine.is_err(), "no /proc, but a sample came back: {mine:?}");
        }
    }

    #[test]
    fn a_delta_of_a_process_that_went_backwards_is_zero_and_not_enormous() {
        let later = Sample {
            read_bytes: 10,
            ..Sample::default()
        };
        let earlier = Sample {
            read_bytes: 99,
            ..Sample::default()
        };
        assert_eq!(later.since(earlier).read_bytes, 0);
    }

    #[test]
    fn a_peak_is_carried_forward_and_not_subtracted() {
        let later = Sample {
            peak_rss_bytes: 20,
            ..Sample::default()
        };
        let earlier = Sample {
            peak_rss_bytes: 50,
            ..Sample::default()
        };
        assert_eq!(
            later.since(earlier).peak_rss_bytes,
            50,
            "a high-water mark is the larger of the two, never their difference"
        );
    }
}
