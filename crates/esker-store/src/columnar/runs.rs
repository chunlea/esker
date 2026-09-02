//! Which runs are live, and the merge that changes the answer.
//!
//! A region's columnar copy is a set of immutable runs. Sealing the memtable adds one; compaction
//! replaces several with one. Neither ever modifies a file, so the only mutable thing here is the
//! answer to *which runs count* — and by `CLAUDE.md` invariant 3 that answer lives in one pointer
//! replaced by atomic rename, exactly as the row engine's `CURRENT` does.
//!
//! # Why a manifest and not the directory listing
//!
//! The listing cannot answer it. Consider a merge of runs 1–4 into run 5, and a crash:
//!
//! * **before the output is renamed** — the listing shows 1–4 and a `.tmp`. Fine either way.
//! * **after the output is renamed, before anything records it** — the listing shows 1, 2, 3, 4
//!   *and* 5. Every row appears **twice**. A reader that trusted the listing would double every
//!   count in the region, silently, and only for the window between a rename and a delete.
//!
//! So the rename is not the commit point; **the manifest write is**. A run is live when the
//! manifest names it and at no other time, which makes both crash windows harmless: an output
//! nothing names is swept and the merge is retried, and inputs nothing names are swept whether or
//! not the delete got to run.
//!
//! # The two-instant race
//!
//! Phase 2 taught the row engine that a file is obsolete only if no live version names it **and it
//! is not a pending output** — the second half being the one that costs data, because a sweep that
//! runs between a merge allocating its number and the manifest naming it would delete the file out
//! from under the merge. That is why [`RunSet`] tracks the numbers a merge has reserved, and why
//! the sweep consults them.
//!
//! At **open** there is no such window: nothing is in flight, so an unnamed run is a merge that did
//! not commit and deleting it is right.
//!
//! # The applied index, and why it is in *this* pointer
//!
//! Version 2 adds one number: the region apply index the live runs are complete to
//! ([ADR 0038](../../../../docs/adr/0038-a-run-manifest-names-the-index-its-runs-are-complete-to.md)).
//! It has to live here rather than in a file beside here, because "these runs hold every version
//! through index N" is a statement about the live set and nothing else — a second file could be
//! written a moment after the manifest and name an index the runs do not hold, which is precisely
//! the claim that must never be made. One atomic rename says both things or neither.
//!
//! The number is only ever advanced **by a seal**, in the same manifest write that makes the run
//! live, and it names the last *entry* whose versions are all in that run or an older one. A
//! manifest that understates costs a replay; one that overstates loses versions silently, so every
//! choice here is the understating one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_base::crc32c;
use esker_engine::fs::FileSystem;

use crate::error::{Result, StoreError};

/// The manifest's magic.
pub const RUNS_MAGIC: [u8; 8] = *b"ESKRRUNS";

/// The format version these bytes are written at.
///
/// Version 1 is still **read** — it is the same manifest without an applied index, so it decodes
/// as one complete to index zero, which is the value that asks for a full rebuild. Nothing is
/// written at version 1 any more.
pub const RUNS_FORMAT_VERSION: u8 = 2;

/// The first version, which named no applied index.
pub const RUNS_FORMAT_VERSION_1: u8 = 1;

/// The manifest's name inside a region's columnar directory.
pub const RUNS_MANIFEST: &str = "RUNS";

/// Bytes before the run numbers at version 2: magic, version, `next`, `applied`, count.
const HEADER_LEN: usize = 8 + 1 + 8 + 8 + 4;

/// The same, at version 1, which carried no applied index.
const HEADER_LEN_V1: usize = 8 + 1 + 8 + 4;

/// So a corrupt count is refused rather than allocated.
const MAX_RUNS: u32 = 1 << 20;

/// The live set of runs for one region, and the manifest that names it.
#[derive(Debug)]
pub struct RunSet {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    live: Vec<u64>,
    next: u64,
    /// The region apply index every version in `live` is complete to; `0` for "unknown".
    ///
    /// Advanced only by [`RunSet::commit`], which is the manifest write that makes a run live —
    /// see this module's header for why the two travel together.
    applied: u64,
    /// Numbers a merge or a seal has taken and not yet committed. Never swept.
    pending: BTreeSet<u64>,
}

impl RunSet {
    /// Opens the run set under `dir`, recovering the live set and sweeping what nothing names.
    pub fn open(fs: Arc<dyn FileSystem>, dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs.create_dir_all(&dir)
            .map_err(|error| bootstrap(&dir, &error.to_string()))?;
        let path = dir.join(RUNS_MANIFEST);

        let (live, next, applied) = if fs
            .exists(&path)
            .map_err(|e| bootstrap(&path, &e.to_string()))?
        {
            let file = fs
                .open(&path)
                .map_err(|e| bootstrap(&path, &e.to_string()))?;
            let size = file.size().map_err(|e| bootstrap(&path, &e.to_string()))?;
            let mut bytes = vec![0u8; usize::try_from(size).unwrap_or(usize::MAX)];
            esker_engine::fs::read_exact_at(file.as_ref(), 0, &mut bytes)
                .map_err(|e| bootstrap(&path, &e.to_string()))?;
            decode(&bytes).map_err(|detail| bootstrap(&path, &detail))?
        } else {
            (Vec::new(), 0, 0)
        };

        let mut set = Self {
            fs,
            dir,
            live,
            next,
            applied,
            pending: BTreeSet::new(),
        };
        // Nothing is in flight at open, so an unnamed run is a merge that did not commit.
        set.sweep()?;
        Ok(set)
    }

    /// The live runs, oldest first.
    #[must_use]
    pub fn live(&self) -> &[u64] {
        &self.live
    }

    /// The region apply index the live runs are complete to; `0` when nothing says.
    ///
    /// A reopen that trusts this replays the log from here rather than re-walking the region, so
    /// the number must never be larger than what the runs durably hold — see the module header.
    #[must_use]
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// The path of one run.
    #[must_use]
    pub fn path_of(&self, number: u64) -> PathBuf {
        self.dir.join(super::run_name(number))
    }

    /// Takes the next run number, marking it pending so no sweep may delete its file.
    pub fn reserve(&mut self) -> u64 {
        let number = self.next;
        self.next += 1;
        self.pending.insert(number);
        number
    }

    /// Commits a freshly written run into the live set, complete to `applied`.
    ///
    /// The manifest write is the commit point, so this is where the run becomes real **and** where
    /// the applied index moves: one rename publishes the run and the claim about it together.
    ///
    /// `applied` never goes backwards. A compaction commits no new versions and a seal of an older
    /// buffer cannot un-hold what a newer one held, so the highest claim any commit has made is
    /// still true.
    pub fn commit(&mut self, number: u64, applied: u64) -> Result<()> {
        self.live.push(number);
        self.live.sort_unstable();
        self.applied = self.applied.max(applied);
        self.write_manifest()?;
        self.pending.remove(&number);
        Ok(())
    }

    /// Republishes the manifest with a higher applied index and the live set it already has.
    ///
    /// The one caller is a seal that found nothing buffered because a mid-entry seal had already
    /// taken the entry's rows: the runs are unchanged and durable, and what is missing is only the
    /// claim that they cover that entry. Writing an index the runs do not hold is the one thing
    /// this must never do, which is why it refuses to move without a caller that has just sealed.
    pub fn republish(&mut self, applied: u64) -> Result<()> {
        if applied <= self.applied {
            return Ok(());
        }
        self.applied = applied;
        self.write_manifest()
    }

    /// Abandons a reserved number whose run was never written.
    pub fn abandon(&mut self, number: u64) {
        self.pending.remove(&number);
    }

    /// Replaces `inputs` with `output`, atomically as far as any reader is concerned.
    ///
    /// The manifest write is the instant the swap happens. Before it, the inputs are the truth and
    /// the output is a file nothing names; after it, the reverse. There is no moment at which both
    /// are live, which is the whole point — a reader that saw both would count every row twice.
    pub fn replace(&mut self, inputs: &[u64], output: u64) -> Result<()> {
        let dropped: BTreeSet<u64> = inputs.iter().copied().collect();
        self.live.retain(|number| !dropped.contains(number));
        self.live.push(output);
        self.live.sort_unstable();
        self.write_manifest()?;
        self.pending.remove(&output);

        // Only now, and a failure here leaks a file rather than losing one. The sweep will get it.
        for number in inputs {
            let path = self.path_of(*number);
            if let Err(error) = self.fs.delete(&path) {
                tracing::warn!(path = %path.display(), %error, "a merged input outlived its merge");
            }
        }
        Ok(())
    }

    /// Deletes every `*.col` in the directory that is neither live nor pending.
    pub fn sweep(&mut self) -> Result<()> {
        let live: BTreeSet<u64> = self.live.iter().copied().collect();
        let entries = self
            .fs
            .list(&self.dir)
            .map_err(|error| bootstrap(&self.dir, &error.to_string()))?;
        for path in entries {
            let Some(number) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(super::run_number)
            else {
                continue;
            };
            if live.contains(&number) || self.pending.contains(&number) {
                continue;
            }
            if let Err(error) = self.fs.delete(&path) {
                tracing::warn!(path = %path.display(), %error, "could not sweep an orphaned run");
            } else {
                tracing::debug!(path = %path.display(), "swept a run nothing names");
            }
        }
        Ok(())
    }

    /// Writes the manifest through a temporary and renames it into place.
    ///
    /// Write, `sync_data`, rename, `fsync` the directory — the row engine's `CURRENT` discipline
    /// (invariant 3), because this pointer has exactly the same job.
    fn write_manifest(&self) -> Result<()> {
        let bytes = encode(&self.live, self.next, self.applied);
        let temp = self.dir.join(format!("{RUNS_MANIFEST}.tmp"));
        {
            let mut file = self
                .fs
                .create(&temp)
                .map_err(|e| bootstrap(&temp, &e.to_string()))?;
            file.append(&bytes)
                .map_err(|e| bootstrap(&temp, &e.to_string()))?;
            file.sync_data()
                .map_err(|e| bootstrap(&temp, &e.to_string()))?;
        }
        let path = self.dir.join(RUNS_MANIFEST);
        if let Err(error) = self.fs.rename(&temp, &path) {
            let _ = self.fs.delete(&temp);
            return Err(bootstrap(&path, &error.to_string()));
        }
        self.fs
            .fsync_dir(&self.dir)
            .map_err(|e| bootstrap(&self.dir, &e.to_string()))
    }
}

/// The manifest's bytes: magic, version, `next`, `applied`, count, the numbers, CRC32C over all
/// of it.
#[must_use]
pub fn encode(live: &[u64], next: u64, applied: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + live.len() * 8 + 4);
    out.extend_from_slice(&RUNS_MAGIC);
    out.push(RUNS_FORMAT_VERSION);
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&applied.to_le_bytes());
    out.extend_from_slice(&(u32::try_from(live.len()).unwrap_or(u32::MAX)).to_le_bytes());
    for number in live {
        out.extend_from_slice(&number.to_le_bytes());
    }
    let crc = crc32c::checksum(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Reads a manifest — the live runs, the next number, the applied index — or says what is wrong
/// with it. Never panics on any input (invariant 9).
///
/// A **version 1** manifest is read rather than refused, and answers applied index `0`: it is this
/// format without the number, and `0` is exactly "nothing says how far these runs go", which is
/// what makes the caller rebuild. Refusing it instead would make an upgrade fail to open a copy
/// that is perfectly good, and the fallback it takes is the behaviour every build before version 2
/// had.
pub fn decode(bytes: &[u8]) -> std::result::Result<(Vec<u64>, u64, u64), String> {
    if bytes.len() < HEADER_LEN_V1 + 4 {
        return Err(format!("{} bytes is shorter than a manifest", bytes.len()));
    }
    if bytes[0..8] != RUNS_MAGIC {
        return Err("wrong magic: this is not a columnar run manifest".into());
    }
    let body = bytes.len() - 4;
    let stored = u32::from_le_bytes([
        bytes[body],
        bytes[body + 1],
        bytes[body + 2],
        bytes[body + 3],
    ]);
    let computed = crc32c::checksum(&bytes[..body]);
    if stored != computed {
        return Err(format!(
            "the manifest's CRC32C is {stored:#010x} but its bytes hash to {computed:#010x}"
        ));
    }
    let header = match bytes[8] {
        RUNS_FORMAT_VERSION => HEADER_LEN,
        RUNS_FORMAT_VERSION_1 => HEADER_LEN_V1,
        other => {
            return Err(format!(
                "format version {other}, and this build understands {RUNS_FORMAT_VERSION_1} \
                 and {RUNS_FORMAT_VERSION}"
            ));
        }
    };
    if bytes.len() < header + 4 {
        return Err(format!("{} bytes is shorter than a manifest", bytes.len()));
    }
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[9..17]);
    let next = u64::from_le_bytes(word);
    let applied = if header == HEADER_LEN {
        word.copy_from_slice(&bytes[17..25]);
        u64::from_le_bytes(word)
    } else {
        0
    };
    let at = header - 4;
    let count = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    if count > MAX_RUNS {
        return Err(format!("{count} runs is more than any region has"));
    }
    let count = count as usize;
    if body != header + count * 8 {
        return Err(format!(
            "a manifest of {count} runs is {} bytes, not {body}",
            header + count * 8
        ));
    }
    let mut live = Vec::with_capacity(count);
    for index in 0..count {
        let at = header + index * 8;
        word.copy_from_slice(&bytes[at..at + 8]);
        live.push(u64::from_le_bytes(word));
    }
    Ok((live, next, applied))
}

/// The applied index a table's manifest names, without opening the runs.
///
/// What a reopen asks **before** deciding between a resume and a rebuild, because the rebuild's
/// first move is to delete this file. A directory with no manifest, and a manifest that cannot be
/// read, both answer `0`: they are the two shapes of "nothing here says how far the runs go", and
/// the caller's answer to both is the full walk.
pub fn applied_index_of(fs: &dyn FileSystem, dir: &Path) -> Result<u64> {
    let path = dir.join(RUNS_MANIFEST);
    if !fs
        .exists(&path)
        .map_err(|error| bootstrap(&path, &error.to_string()))?
    {
        return Ok(0);
    }
    let file = fs
        .open(&path)
        .map_err(|error| bootstrap(&path, &error.to_string()))?;
    let size = file
        .size()
        .map_err(|error| bootstrap(&path, &error.to_string()))?;
    let mut bytes = vec![0u8; usize::try_from(size).unwrap_or(usize::MAX)];
    esker_engine::fs::read_exact_at(file.as_ref(), 0, &mut bytes)
        .map_err(|error| bootstrap(&path, &error.to_string()))?;
    match decode(&bytes) {
        Ok((_, _, applied)) => Ok(applied),
        Err(detail) => {
            tracing::warn!(path = %path.display(), detail, "a run manifest could not be read");
            Ok(0)
        }
    }
}

fn bootstrap(path: &Path, detail: &str) -> StoreError {
    StoreError::Bootstrap(format!("{}: {detail}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Arc;

    use esker_engine::fs::{FileSystem, LocalFileSystem};

    use super::{RUNS_FORMAT_VERSION, RUNS_MANIFEST, RunSet, decode, encode};

    fn fs() -> Arc<dyn FileSystem> {
        Arc::new(LocalFileSystem::new())
    }

    fn touch(dir: &std::path::Path, number: u64) {
        std::fs::write(
            dir.join(super::super::run_name(number)),
            b"not really a run",
        )
        .unwrap();
    }

    /// The golden bytes. A change to them is a format change and needs a version bump, not an
    /// edit to this array (`CLAUDE.md`, "ask before doing").
    #[test]
    fn the_manifest_bytes_are_exactly_these() {
        assert_eq!(
            encode(&[1, 2], 3, 41),
            [
                0x45, 0x53, 0x4b, 0x52, 0x52, 0x55, 0x4e, 0x53, // "ESKRRUNS"
                0x02, // format version
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // next = 3
                0x29, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // applied = 41
                0x02, 0x00, 0x00, 0x00, // two runs
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
                0xfa, 0x18, 0x2d, 0x2b, // CRC32C
            ]
        );
    }

    /// Version 1's bytes, written out longhand so that a build which stopped reading them fails
    /// here rather than on somebody's data directory.
    ///
    /// It answers applied index **zero**, which is the value that asks for the full rebuild — the
    /// behaviour every build before version 2 had, which is exactly what an old manifest deserves.
    #[test]
    fn a_version_one_manifest_still_reads_and_asks_for_a_rebuild() {
        let mut bytes = vec![
            0x45, 0x53, 0x4b, 0x52, 0x52, 0x55, 0x4e, 0x53, // "ESKRRUNS"
            0x01, // format version 1
            0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // next = 3
            0x02, 0x00, 0x00, 0x00, // two runs
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
        ];
        let crc = esker_base::crc32c::checksum(&bytes);
        bytes.extend_from_slice(&crc.to_le_bytes());
        assert_eq!(decode(&bytes).unwrap(), (vec![1, 2], 3, 0));
    }

    #[test]
    fn a_manifest_round_trips() {
        assert_eq!(
            decode(&encode(&[7, 9], 12, 900)).unwrap(),
            (vec![7, 9], 12, 900)
        );
        assert_eq!(decode(&encode(&[], 0, 0)).unwrap(), (Vec::new(), 0, 0));
    }

    /// The claim and the runs travel in one rename, so a reopen cannot see one without the other.
    #[test]
    fn the_applied_index_survives_a_reopen_and_never_goes_backwards() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut runs = RunSet::open(fs(), dir.path()).unwrap();
            assert_eq!(runs.applied(), 0, "a fresh set claims nothing");
            let number = runs.reserve();
            touch(dir.path(), number);
            runs.commit(number, 17).unwrap();
            let second = runs.reserve();
            touch(dir.path(), second);
            // An older claim from a later seal leaves the higher one standing.
            runs.commit(second, 4).unwrap();
            assert_eq!(runs.applied(), 17);
        }
        assert_eq!(RunSet::open(fs(), dir.path()).unwrap().applied(), 17);
    }

    /// Every byte matters, and a manifest that cannot be read must never be guessed at: it is the
    /// only thing that says which runs are real.
    #[test]
    fn a_single_flipped_bit_anywhere_is_refused() {
        let good = encode(&[3, 4], 5, 6);
        for byte in 0..good.len() {
            for bit in 0..8 {
                let mut bad = good.clone();
                bad[byte] ^= 1 << bit;
                assert!(
                    decode(&bad).is_err(),
                    "a flip at byte {byte} bit {bit} was accepted"
                );
            }
        }
    }

    #[test]
    fn a_newer_format_version_is_named_rather_than_guessed_at() {
        let mut bytes = encode(&[1], 2, 3);
        bytes[8] = RUNS_FORMAT_VERSION + 1;
        let body = bytes.len() - 4;
        let crc = esker_base::crc32c::checksum(&bytes[..body]);
        bytes[body..].copy_from_slice(&crc.to_le_bytes());
        let error = decode(&bytes).unwrap_err();
        assert!(error.contains("format version"), "{error}");
    }

    /// A run is live when the manifest names it and at no other time.
    #[test]
    fn the_live_set_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let number = {
            let mut runs = RunSet::open(fs(), dir.path()).unwrap();
            let number = runs.reserve();
            touch(dir.path(), number);
            runs.commit(number, 0).unwrap();
            number
        };
        let runs = RunSet::open(fs(), dir.path()).unwrap();
        assert_eq!(runs.live(), [number]);
        assert!(dir.path().join(super::super::run_name(number)).exists());
    }

    /// **The crash between rename and manifest.** The output file is on disk and nothing names
    /// it, which is a merge that did not commit — so the file goes and the inputs stay. A build
    /// that trusted the directory listing would instead see inputs *and* output and count every
    /// row twice.
    #[test]
    fn a_run_the_manifest_does_not_name_is_swept_at_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut runs = RunSet::open(fs(), dir.path()).unwrap();
            let live = runs.reserve();
            touch(dir.path(), live);
            runs.commit(live, 0).unwrap();
        }
        // The merge output that never made it into the manifest.
        touch(dir.path(), 99);
        let runs = RunSet::open(fs(), dir.path()).unwrap();
        assert_eq!(runs.live(), [0], "the committed run was swept");
        assert!(
            !dir.path().join(super::super::run_name(99)).exists(),
            "an uncommitted merge output survived"
        );
    }

    /// The sweep must never take the manifest itself, nor a run a merge has in flight — the
    /// second is phase 2's two-instant race, which costs data rather than space.
    #[test]
    fn the_sweep_spares_the_manifest_and_anything_pending() {
        let dir = tempfile::tempdir().unwrap();
        let mut runs = RunSet::open(fs(), dir.path()).unwrap();
        // A committed run, so there is a manifest for the sweep to spare.
        let committed = runs.reserve();
        touch(dir.path(), committed);
        runs.commit(committed, 0).unwrap();

        let in_flight = runs.reserve();
        touch(dir.path(), in_flight);
        runs.sweep().unwrap();
        assert!(
            dir.path().join(super::super::run_name(in_flight)).exists(),
            "the sweep deleted a run a merge was still writing"
        );
        assert!(dir.path().join(RUNS_MANIFEST).exists());
    }

    /// The swap is the manifest write, and it is where the inputs stop being real.
    #[test]
    fn replacing_inputs_makes_the_output_live_and_deletes_them() {
        let dir = tempfile::tempdir().unwrap();
        let mut runs = RunSet::open(fs(), dir.path()).unwrap();
        let mut inputs = Vec::new();
        for _ in 0..3 {
            let number = runs.reserve();
            touch(dir.path(), number);
            runs.commit(number, 0).unwrap();
            inputs.push(number);
        }
        let output = runs.reserve();
        touch(dir.path(), output);
        runs.replace(&inputs, output).unwrap();

        assert_eq!(runs.live(), [output]);
        for number in inputs {
            assert!(!dir.path().join(super::super::run_name(number)).exists());
        }
        // And it survives the reopen, which is the half that makes it durable rather than merely
        // in memory.
        assert_eq!(RunSet::open(fs(), dir.path()).unwrap().live(), [output]);
    }

    /// Numbers are never reused, so a swept orphan can never be confused with a live run that
    /// happens to have taken its name.
    #[test]
    fn run_numbers_are_never_reused_across_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let first = {
            let mut runs = RunSet::open(fs(), dir.path()).unwrap();
            let number = runs.reserve();
            touch(dir.path(), number);
            runs.commit(number, 0).unwrap();
            number
        };
        let mut runs = RunSet::open(fs(), dir.path()).unwrap();
        assert!(runs.reserve() > first, "a run number was handed out twice");
    }
}
