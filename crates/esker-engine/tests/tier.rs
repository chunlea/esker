//! SST tiering: what the engine does when its files live in object storage.
//!
//! Every test here runs against `esker_s3::MemoryStore` rather than a container. That is
//! deliberate: none of this is about HTTP. It is about *when* an upload happens relative to a
//! manifest edit, what happens when one fails, what the governor is allowed to delete, and
//! whether a database that has lost its local SSTs can still read every key. A container would
//! make those tests slow and conditional on Docker, which is how the interesting cases end up
//! behind `#[ignore]`. The HTTP half is covered by `esker-s3`'s own `MinIO` test, and the two
//! meet in `tests/tier_minio.rs`.
//!
//! `background: false` throughout. With the uploader on its own thread, "has it uploaded yet"
//! is a question only a sleep can answer, and a suite that sleeps is a suite that is flaky on a
//! loaded machine. Here `Db::tier_maintenance` is the only thing that moves a byte.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use esker_engine::fs::tier::{TierOptions, TieredFileSystem};
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_engine::version::FileLocation;
use esker_engine::{Db, Options, ReadOptions};
use esker_s3::{MemoryStore, ObjectStore};

/// A database whose SSTs tier into a `MemoryStore` we keep a handle on.
struct Fixture {
    dir: tempfile::TempDir,
    store: Arc<MemoryStore>,
    db: Option<Db>,
}

impl Fixture {
    fn with_options(tier_options: TierOptions) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(MemoryStore::new());
        let db = Self::open_at(dir.path(), &store, tier_options);
        Self {
            dir,
            store,
            db: Some(db),
        }
    }

    fn new() -> Self {
        Self::with_options(TierOptions {
            background: false,
            ..TierOptions::default()
        })
    }

    fn open_at(path: &std::path::Path, store: &Arc<MemoryStore>, tier: TierOptions) -> Db {
        let fs = TieredFileSystem::new(
            Arc::new(LocalFileSystem::new()),
            Arc::clone(store) as Arc<dyn ObjectStore>,
            path,
            tier,
        )
        .unwrap();
        let options = Options {
            create_if_missing: true,
            // One compaction thread is enough and keeps the file numbers predictable.
            compaction_threads: 1,
            ..Options::default()
        };
        Db::open_with(path, options, fs as Arc<dyn FileSystem>, &["default"]).unwrap()
    }

    fn db(&self) -> &Db {
        self.db.as_ref().unwrap()
    }

    /// Writes `count` keys and flushes them into one SST.
    fn write_and_flush(&self, prefix: &str, count: u32) {
        for index in 0..count {
            self.db()
                .put(
                    "default",
                    format!("{prefix}{index:06}").as_bytes(),
                    format!("value-{index}").as_bytes(),
                )
                .unwrap();
        }
        self.db().flush("default").unwrap();
    }

    fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.db()
            .get("default", key.as_bytes(), &ReadOptions::default())
            .unwrap()
            .map(|value| value.to_vec())
    }

    /// Local `*.sst` files, by number.
    fn local_ssts(&self) -> BTreeSet<u64> {
        LocalFileSystem::new()
            .list(self.dir.path())
            .unwrap()
            .into_iter()
            .filter_map(|path| match esker_engine::filename::classify_path(&path) {
                Some(esker_engine::filename::FileKind::Sst(number)) => Some(number),
                _ => None,
            })
            .collect()
    }

    /// Objects in the store, by file number.
    fn objects(&self) -> BTreeSet<u64> {
        self.store
            .keys()
            .into_iter()
            .filter_map(|key| key.strip_suffix(".sst")?.parse().ok())
            .collect()
    }

    fn close(&mut self) {
        self.db.take();
    }

    fn reopen(&mut self, tier: TierOptions) {
        self.close();
        self.db = Some(Self::open_at(self.dir.path(), &self.store, tier));
    }

    fn reopen_default(&mut self) {
        self.reopen(TierOptions {
            background: false,
            ..TierOptions::default()
        });
    }
}

/// The order ADR 0024 decision 1 fixes: the SST is durable and named by an edit *before*
/// anything is uploaded. The flush returns having touched the object store zero times.
#[test]
fn a_flush_completes_without_uploading_anything() {
    let fixture = Fixture::new();
    fixture.write_and_flush("k", 100);

    assert_eq!(fixture.local_ssts().len(), 1, "the flush wrote an SST");
    assert!(
        fixture.objects().is_empty(),
        "the flush must not have waited on an upload"
    );
    let (puts, _, _) = fixture.store.counts();
    assert_eq!(puts, 0, "the object store was touched during a flush");

    // And the data is readable from the local file, with no tier involvement at all.
    assert_eq!(fixture.get("k000042").unwrap(), b"value-42");
}

/// Maintenance uploads it and records the promotion in the manifest, which survives a reopen.
#[test]
fn maintenance_uploads_and_the_manifest_remembers() {
    let mut fixture = Fixture::new();
    fixture.write_and_flush("k", 100);
    let number = *fixture.local_ssts().iter().next().unwrap();

    assert_eq!(fixture.db().tier_maintenance().unwrap(), 1);
    assert_eq!(fixture.objects(), BTreeSet::from([number]));

    let locations = fixture.db().file_locations();
    assert_eq!(
        locations.get(&number),
        Some(&FileLocation::Tiered),
        "the promotion did not reach the manifest"
    );

    fixture.reopen_default();
    assert_eq!(
        fixture.db().file_locations().get(&number),
        Some(&FileLocation::Tiered),
        "the location did not survive a reopen"
    );
    assert_eq!(fixture.get("k000007").unwrap(), b"value-7");
}

/// **The acceptance shape.** The database loses every local SST and reads every key back.
#[test]
fn losing_every_local_sst_loses_no_data() {
    let mut fixture = Fixture::new();
    for batch in 0..3 {
        fixture.write_and_flush(&format!("batch{batch}-"), 200);
        assert_eq!(fixture.db().tier_maintenance().unwrap(), 1);
    }
    fixture.close();

    // The disk this database was on is gone; the bucket is not.
    let local = LocalFileSystem::new();
    let mut deleted = 0;
    for path in local.list(fixture.dir.path()).unwrap() {
        if matches!(
            esker_engine::filename::classify_path(&path),
            Some(esker_engine::filename::FileKind::Sst(_))
        ) {
            local.delete(&path).unwrap();
            deleted += 1;
        }
    }
    assert_eq!(deleted, 3, "the test deleted the wrong thing");

    fixture.reopen_default();
    assert!(fixture.local_ssts().is_empty(), "nothing was left locally");
    for batch in 0..3 {
        for index in [0u32, 7, 199] {
            let key = format!("batch{batch}-{index:06}");
            assert_eq!(
                fixture
                    .get(&key)
                    .unwrap_or_else(|| panic!("{key} was lost")),
                format!("value-{index}").as_bytes(),
                "{key}"
            );
        }
    }
    let stats = fixture.db().tier_stats().unwrap();
    assert!(
        stats.cache_misses > 0,
        "the reads must have gone to the tier"
    );
    assert!(stats.ranged_reads > 0, "and they must have been ranged");
}

/// A cold read is **ranged**, not a whole-file fetch. An 8 MiB download to answer a point read
/// is the difference between a usable p99 and an unusable one, so this pins the byte count.
#[test]
fn a_cold_point_read_does_not_download_the_whole_file() {
    let mut fixture = Fixture::new();
    fixture.write_and_flush("k", 2_000);
    fixture.db().tier_maintenance().unwrap();
    let number = *fixture.local_ssts().iter().next().unwrap();
    let sst_size = LocalFileSystem::new()
        .size(&esker_engine::filename::sst(fixture.dir.path(), number))
        .unwrap();
    fixture.close();

    LocalFileSystem::new()
        .delete(&esker_engine::filename::sst(fixture.dir.path(), number))
        .unwrap();
    // No fetching: the point is to measure what one cold read costs.
    fixture.reopen(TierOptions {
        background: false,
        batch: 0,
        ..TierOptions::default()
    });

    assert_eq!(fixture.get("k001234").unwrap(), b"value-1234");
    let stats = fixture.db().tier_stats().unwrap();
    assert!(
        stats.ranged_reads > 0 && stats.ranged_reads < 32,
        "a point read took {} ranged reads of a {sst_size}-byte file",
        stats.ranged_reads
    );
    assert_eq!(stats.fetches, 0, "nothing should have been fetched whole");
}

/// An upload that fails fails nothing else: the write succeeded, the file is local and
/// readable, and the number goes back on the queue (ADR 0024 decision 2).
#[test]
fn a_failing_upload_fails_nothing_and_is_retried() {
    let fixture = Fixture::new();
    fixture.store.fail_next_puts(3);

    fixture.write_and_flush("k", 100);
    let number = *fixture.local_ssts().iter().next().unwrap();

    for attempt in 0..3 {
        assert_eq!(
            fixture.db().tier_maintenance().unwrap(),
            0,
            "attempt {attempt} should have failed, not landed"
        );
        assert!(fixture.objects().is_empty(), "nothing should have landed");
        // The whole point: reads keep working off the local copy.
        assert_eq!(fixture.get("k000042").unwrap(), b"value-42");
        assert_eq!(
            fixture.db().file_locations().get(&number),
            Some(&FileLocation::Local),
            "a file that failed to upload must not be recorded as tiered"
        );
    }

    // Each pass tried exactly once: the retry backoff is one attempt per pass, so three
    // passes are three failures and not the twenty-four a batch of eight would have made.
    assert_eq!(fixture.db().tier_stats().unwrap().upload_failures, 3);

    // The fourth pass finds the store working again.
    assert_eq!(fixture.db().tier_maintenance().unwrap(), 1);
    assert_eq!(fixture.objects(), BTreeSet::from([number]));
    let stats = fixture.db().tier_stats().unwrap();
    assert_eq!(stats.upload_failures, 3);
    assert_eq!(stats.uploads, 1);
}

/// The governor evicts a local file whose bytes are safe in the tier, and the next read
/// fetches it back. A budget of zero makes every uploaded file a candidate.
#[test]
fn the_governor_evicts_uploaded_files_and_reads_refill_them() {
    let mut fixture = Fixture::with_options(TierOptions {
        background: false,
        local_budget: Some(0),
        ..TierOptions::default()
    });
    fixture.write_and_flush("k", 500);
    let number = *fixture.local_ssts().iter().next().unwrap();

    fixture.db().tier_maintenance().unwrap();
    assert_eq!(fixture.objects(), BTreeSet::from([number]));
    assert!(
        fixture.local_ssts().is_empty(),
        "an uploaded file over budget should have been evicted"
    );
    assert_eq!(fixture.db().tier_stats().unwrap().evictions, 1);

    // Still readable, now over the tier.
    assert_eq!(fixture.get("k000123").unwrap(), b"value-123");

    // And a maintenance pass pulls it back to disk, so the next open is a hit. It is evicted
    // again immediately afterwards, this budget being zero — which is itself the test that
    // eviction and refill do not fight to a standstill or lose the file.
    fixture.db().tier_maintenance().unwrap();
    assert_eq!(fixture.db().tier_stats().unwrap().fetches, 1);
    assert_eq!(fixture.get("k000123").unwrap(), b"value-123");

    fixture.reopen_default();
    assert_eq!(fixture.get("k000499").unwrap(), b"value-499");
}

/// **The governor never evicts a file the tier does not hold**, whatever the pressure. This is
/// the one thing the whole design exists to make impossible.
#[test]
fn a_file_that_failed_to_upload_is_never_evicted() {
    let fixture = Fixture::with_options(TierOptions {
        background: false,
        local_budget: Some(0),
        ..TierOptions::default()
    });
    fixture.store.fail_next_puts(50);
    fixture.write_and_flush("k", 200);

    for _ in 0..5 {
        fixture.db().tier_maintenance().unwrap();
        assert_eq!(
            fixture.local_ssts().len(),
            1,
            "the only copy of a live file was evicted"
        );
    }
    assert_eq!(fixture.db().tier_stats().unwrap().evictions, 0);
    assert_eq!(fixture.get("k000199").unwrap(), b"value-199");
}

/// The WAL, the manifest and `CURRENT` are the log, and the log stays local. Nothing that is
/// not an SST may ever reach the bucket.
#[test]
fn only_ssts_are_tiered() {
    let fixture = Fixture::new();
    fixture.write_and_flush("k", 100);
    fixture.db().tier_maintenance().unwrap();

    for key in fixture.store.keys() {
        // The engine only ever writes lowercase `.sst`, so the case-sensitive comparison is
        // the strict one and therefore the one this assertion wants.
        assert!(
            std::path::Path::new(&key)
                .extension()
                .is_some_and(|ext| ext == "sst"),
            "{key} reached object storage and is not an SST"
        );
    }
    // And the local directory still has its log and its pointer.
    let names: Vec<String> = LocalFileSystem::new()
        .list(fixture.dir.path())
        .unwrap()
        .iter()
        .filter_map(|path| Some(path.file_name()?.to_str()?.to_string()))
        .collect();
    assert!(names.iter().any(|name| name == "CURRENT"), "{names:?}");
    assert!(
        names.iter().any(|name| std::path::Path::new(name)
            .extension()
            .is_some_and(|e| e == "wal")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|name| name.starts_with("MANIFEST-")),
        "{names:?}"
    );
}

/// **The sweep a caller outside the engine has must reclaim the objects too.**
///
/// `Db::purge_obsolete_files` is the public sweep, and it is the one `Db::open` itself calls to
/// clear whatever the last process left behind. It listed the directory and deleted the files no
/// version named — and stopped there, so the *object* of every one of them stayed in the store
/// for the life of the bucket. A listing can never drive object reclamation, which is
/// [ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 5's whole point: a
/// file the tier has evicted is not in the listing at all.
///
/// The open iterator is what makes this deterministic. It pins the version the compaction starts
/// from, so the compaction's own sweep sees its inputs as live and correctly leaves their objects
/// alone; dropping it afterwards makes the *next* sweep the one that has to do the work, and the
/// next sweep here is the public one and nothing else.
#[test]
fn the_public_sweep_reclaims_objects_and_not_only_files() {
    let fixture = Fixture::new();
    for batch in 0..5 {
        fixture.write_and_flush(&format!("b{batch}-"), 100);
        fixture.db().tier_maintenance().unwrap();
    }
    let uploaded = fixture.objects();
    assert!(
        uploaded.len() >= 5,
        "nothing reached the store, so this measures nothing: {uploaded:?}"
    );

    let pinned = fixture
        .db()
        .iter("default", &ReadOptions::default())
        .unwrap();
    fixture.db().compact_range("default", None, None).unwrap();
    fixture.db().tier_maintenance().unwrap();
    drop(pinned);

    fixture.db().purge_obsolete_files().unwrap();

    let live: BTreeSet<u64> = fixture.db().file_locations().into_keys().collect();
    let leaked: BTreeSet<u64> = fixture.objects().difference(&live).copied().collect();
    assert!(
        leaked.is_empty(),
        "the public sweep left the objects of {leaked:?} in the store; the version names {live:?}"
    );
}

/// An object is deleted only when no live version names its number. A compaction makes its
/// inputs obsolete; the sweep then reclaims both the local files and the objects.
#[test]
fn objects_are_reclaimed_when_and_only_when_the_files_are() {
    let fixture = Fixture::new();
    // Enough L0 files to trigger a compaction (the default trigger is four).
    for batch in 0..5 {
        fixture.write_and_flush(&format!("b{batch}-"), 100);
        fixture.db().tier_maintenance().unwrap();
    }
    let uploaded_before = fixture.objects();
    assert!(uploaded_before.len() >= 5);

    fixture.db().compact_range("default", None, None).unwrap();
    fixture.db().tier_maintenance().unwrap();
    fixture.db().purge_obsolete_files().unwrap();
    fixture.db().tier_maintenance().unwrap();

    let live: BTreeSet<u64> = fixture.local_ssts();
    let objects = fixture.objects();
    assert!(
        objects.is_subset(&live) || objects.iter().all(|number| live.contains(number)),
        "objects {objects:?} outlived the files {live:?} that named them"
    );
    assert!(
        fixture.db().tier_stats().unwrap().object_deletes > 0,
        "the compaction's inputs should have had their objects reclaimed"
    );

    // Nothing was lost in the process.
    for batch in 0..5 {
        assert_eq!(
            fixture.get(&format!("b{batch}-000050")).unwrap(),
            b"value-50"
        );
    }
}

/// An object that changed under us is caught by its `ETag` rather than surfacing three layers
/// up as a block checksum failure (ADR 0024 decision 6).
#[test]
fn an_object_that_changed_underneath_us_is_an_error_not_wrong_bytes() {
    let mut fixture = Fixture::with_options(TierOptions {
        background: false,
        local_budget: Some(0),
        batch: 1,
        ..TierOptions::default()
    });
    fixture.write_and_flush("k", 300);
    fixture.db().tier_maintenance().unwrap();
    let number = *fixture.objects().iter().next().unwrap();
    fixture.close();

    // Somebody overwrote the object with something else entirely.
    fixture
        .store
        .corrupt(&format!("{number:06}.sst"), vec![0xAB; 4096]);
    fixture.reopen(TierOptions {
        background: false,
        local_budget: Some(0),
        batch: 0,
        ..TierOptions::default()
    });

    let result = fixture
        .db()
        .get("default", b"k000123", &ReadOptions::default());
    assert!(
        result.is_err(),
        "reading a replaced object returned {result:?} instead of failing"
    );
}

/// **Regression: an idle column family must not pin the write-ahead log.**
///
/// `roll_log_and_switch` updates `active_log` only for the families it switches, so a family
/// that goes idle keeps a stale one — and `oldest_log` used to answer it unconditionally, which
/// pinned `advance_log_number` for ever. A store with four families and traffic to one then
/// retained every WAL segment it had ever written.
///
/// Two things go wrong when it does, and the second is worse than the first: the data directory
/// grows without bound, and every write ever made stays in the log, so a node that loses its
/// SSTs loses nothing and a tier that is doing its job cannot be told from one that is not.
/// This test lives here rather than beside the other flush tests because that is how it was
/// found — `losing_every_local_sst_loses_no_data` at the store level quietly proved nothing.
#[test]
fn an_idle_column_family_does_not_pin_the_write_ahead_log() {
    let dir = tempfile::tempdir().unwrap();
    let options = Options {
        create_if_missing: true,
        compaction_threads: 1,
        cf_options: esker_engine::options::CfOptions {
            // Small, so a few hundred keys roll the log many times.
            write_buffer_size: 64 * 1024,
            ..esker_engine::options::CfOptions::default()
        },
        ..Options::default()
    };
    // Two families; only one is ever written to. `idle` is `esker-store`'s `lock` and `write`.
    let db = Db::open_with(
        dir.path(),
        options,
        Arc::new(LocalFileSystem::new()) as Arc<dyn FileSystem>,
        &["default", "idle"],
    )
    .unwrap();

    let value = vec![b'v'; 512];
    for round in 0..40 {
        for index in 0..100u32 {
            db.put(
                "default",
                format!("k{round:02}{index:04}").as_bytes(),
                &value,
            )
            .unwrap();
        }
        db.flush("default").unwrap();
    }

    let wal_segments = LocalFileSystem::new()
        .list(dir.path())
        .unwrap()
        .into_iter()
        .filter(|path| {
            matches!(
                esker_engine::filename::classify_path(path),
                Some(esker_engine::filename::FileKind::Wal(_))
            )
        })
        .count();
    assert!(
        wal_segments <= 3,
        "{wal_segments} WAL segments survived 40 flushes — an idle family is pinning the log"
    );

    // And the data is all still there, which is the half that matters more.
    for round in [0u32, 20, 39] {
        assert_eq!(
            db.get(
                "default",
                format!("k{round:02}0050").as_bytes(),
                &ReadOptions::default()
            )
            .unwrap()
            .unwrap()
            .as_ref(),
            value.as_slice(),
            "round {round}"
        );
    }
}
