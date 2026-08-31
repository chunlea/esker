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
//! from under the merge. That is why [`RunSet::pending`] exists and why the sweep consults it.
//!
//! At **open** there is no such window: nothing is in flight, so an unnamed run is a merge that did
//! not commit and deleting it is right.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use esker_base::crc32c;
use esker_engine::fs::FileSystem;

use crate::error::{Result, StoreError};

/// The manifest's magic.
pub const RUNS_MAGIC: [u8; 8] = *b"ESKRRUNS";

/// The format version these bytes are written at.
pub const RUNS_FORMAT_VERSION: u8 = 1;

/// The manifest's name inside a region's columnar directory.
pub const RUNS_MANIFEST: &str = "RUNS";

/// Bytes before the run numbers: magic, version, `next`, count.
const HEADER_LEN: usize = 8 + 1 + 8 + 4;

/// So a corrupt count is refused rather than allocated.
const MAX_RUNS: u32 = 1 << 20;

/// The live set of runs for one region, and the manifest that names it.
#[derive(Debug)]
pub struct RunSet {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
    live: Vec<u64>,
    next: u64,
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

        let (live, next) = if fs
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
            (Vec::new(), 0)
        };

        let mut set = Self {
            fs,
            dir,
            live,
            next,
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

    /// Commits a freshly written run into the live set.
    ///
    /// The manifest write is the commit point, so this is where the run becomes real.
    pub fn commit(&mut self, number: u64) -> Result<()> {
        self.live.push(number);
        self.live.sort_unstable();
        self.write_manifest()?;
        self.pending.remove(&number);
        Ok(())
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
        let bytes = encode(&self.live, self.next);
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

/// The manifest's bytes: magic, version, `next`, count, the numbers, CRC32C over all of it.
#[must_use]
pub fn encode(live: &[u64], next: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + live.len() * 8 + 4);
    out.extend_from_slice(&RUNS_MAGIC);
    out.push(RUNS_FORMAT_VERSION);
    out.extend_from_slice(&next.to_le_bytes());
    out.extend_from_slice(&(u32::try_from(live.len()).unwrap_or(u32::MAX)).to_le_bytes());
    for number in live {
        out.extend_from_slice(&number.to_le_bytes());
    }
    let crc = crc32c::checksum(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// Reads a manifest, or says what is wrong with it. Never panics on any input (invariant 9).
pub fn decode(bytes: &[u8]) -> std::result::Result<(Vec<u64>, u64), String> {
    if bytes.len() < HEADER_LEN + 4 {
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
    if bytes[8] != RUNS_FORMAT_VERSION {
        return Err(format!(
            "format version {}, and this build understands {RUNS_FORMAT_VERSION}",
            bytes[8]
        ));
    }
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes[9..17]);
    let next = u64::from_le_bytes(word);
    let count = u32::from_le_bytes([bytes[17], bytes[18], bytes[19], bytes[20]]);
    if count > MAX_RUNS {
        return Err(format!("{count} runs is more than any region has"));
    }
    let count = count as usize;
    if body != HEADER_LEN + count * 8 {
        return Err(format!(
            "a manifest of {count} runs is {} bytes, not {body}",
            HEADER_LEN + count * 8
        ));
    }
    let mut live = Vec::with_capacity(count);
    for index in 0..count {
        let at = HEADER_LEN + index * 8;
        word.copy_from_slice(&bytes[at..at + 8]);
        live.push(u64::from_le_bytes(word));
    }
    Ok((live, next))
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
            encode(&[1, 2], 3),
            [
                0x45, 0x53, 0x4b, 0x52, 0x52, 0x55, 0x4e, 0x53, // "ESKRRUNS"
                0x01, // format version
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // next = 3
                0x02, 0x00, 0x00, 0x00, // two runs
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
                0xa9, 0x42, 0xd4, 0x32, // CRC32C
            ]
        );
    }

    #[test]
    fn a_manifest_round_trips() {
        assert_eq!(decode(&encode(&[7, 9], 12)).unwrap(), (vec![7, 9], 12));
        assert_eq!(decode(&encode(&[], 0)).unwrap(), (Vec::new(), 0));
    }

    /// Every byte matters, and a manifest that cannot be read must never be guessed at: it is the
    /// only thing that says which runs are real.
    #[test]
    fn a_single_flipped_bit_anywhere_is_refused() {
        let good = encode(&[3, 4], 5);
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
        let mut bytes = encode(&[1], 2);
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
            runs.commit(number).unwrap();
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
            runs.commit(live).unwrap();
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
        runs.commit(committed).unwrap();

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
            runs.commit(number).unwrap();
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
            runs.commit(number).unwrap();
            number
        };
        let mut runs = RunSet::open(fs(), dir.path()).unwrap();
        assert!(runs.reserve() > first, "a run number was handed out twice");
    }
}
