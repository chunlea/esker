//! The engine's tiering against a real object store.
//!
//! `tests/tier.rs` covers the behaviour — when an upload happens, what the governor may evict,
//! what a failure does — against an in-memory store, because none of that is about HTTP. This
//! file covers the part that is: real `SigV4`, real ranged `GET`s, a real endpoint that can be
//! turned off. It is small on purpose, and everything it asserts is something an in-memory
//! store genuinely cannot tell us.
//!
//! # The recipe
//!
//! ```sh
//! docker run -d --name esker-minio -p 19000:9000 \
//!     -e MINIO_ROOT_USER=eskertest -e MINIO_ROOT_PASSWORD=eskertest123 \
//!     minio/minio:latest server /data
//! docker exec esker-minio mc alias set local http://127.0.0.1:9000 eskertest eskertest123
//! docker exec esker-minio mc mb --ignore-existing local/esker
//!
//! cargo test -p esker-engine --test tier_minio -- --ignored --test-threads=1
//! ```
//!
//! Port 19000, not 9000, so it cannot collide with a `MinIO` somebody is already running. The
//! `mc` client ships inside the image, which is why the bucket is created with `docker exec`
//! and not with a fifth S3 call this crate would otherwise never need.
//!
//! `ESKER_S3_ENDPOINT`, `ESKER_S3_BUCKET`, `ESKER_S3_KEY` and `ESKER_S3_SECRET` override the
//! defaults, so the same tests run against any S3-compatible endpoint reachable over HTTP.
//!
//! Verified against `minio/minio` at digest
//! `sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e`.
//!
//! **`--test-threads=1`**: the outage test stops and restarts the shared container, and two
//! tests sharing it would see each other's outage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use esker_engine::fs::claim::{Identity, id_for_directory};
use esker_engine::fs::tier::{TierOptions, TieredFileSystem};
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_engine::{Db, Options, ReadOptions};

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

/// A tiered filesystem pointed at the container, under a prefix unique to this test.
fn tiered_fs(dir: &std::path::Path, prefix: &str, budget: Option<u64>) -> Arc<dyn FileSystem> {
    let endpoint =
        esker_s3::Endpoint::parse(&env_or("ESKER_S3_ENDPOINT", "http://localhost:19000"))
            .expect("the endpoint must parse");
    let config = esker_s3::Config::from_store_url(
        &format!(
            "s3://{}/engine/{prefix}",
            env_or("ESKER_S3_BUCKET", "esker")
        ),
        endpoint,
        "us-east-1",
        esker_s3::Credentials::new(
            env_or("ESKER_S3_KEY", "eskertest"),
            env_or("ESKER_S3_SECRET", "eskertest123"),
        ),
    )
    .expect("the store URL must parse");
    let key_prefix = config.prefix.clone();

    TieredFileSystem::new(
        Arc::new(LocalFileSystem::new()),
        Arc::new(esker_s3::S3Client::new(config)),
        dir,
        TierOptions {
            key_prefix,
            local_budget: budget,
            background: false,
            ..TierOptions::default()
        },
    )
    .expect("opening the tier")
}

/// The same, but claiming the prefix as `identity` — the real-S3 half of the claim marker.
fn claiming_fs(
    dir: &std::path::Path,
    prefix: &str,
    identity: Identity,
    adopt: bool,
) -> std::io::Result<Arc<TieredFileSystem>> {
    let endpoint =
        esker_s3::Endpoint::parse(&env_or("ESKER_S3_ENDPOINT", "http://localhost:19000"))
            .expect("the endpoint must parse");
    let config = esker_s3::Config::from_store_url(
        &format!(
            "s3://{}/engine/{prefix}",
            env_or("ESKER_S3_BUCKET", "esker")
        ),
        endpoint,
        "us-east-1",
        esker_s3::Credentials::new(
            env_or("ESKER_S3_KEY", "eskertest"),
            env_or("ESKER_S3_SECRET", "eskertest123"),
        ),
    )
    .expect("the store URL must parse");
    let key_prefix = config.prefix.clone();

    TieredFileSystem::new(
        Arc::new(LocalFileSystem::new()),
        Arc::new(esker_s3::S3Client::new(config)),
        dir,
        TierOptions {
            key_prefix,
            background: false,
            identity: Some(identity),
            adopt_unclaimed: adopt,
            ..TierOptions::default()
        },
    )
}

/// A unique prefix per run, so a re-run does not meet its own claim from last time.
fn unique_prefix(what: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!("{what}-{nanos:x}")
}

fn open(dir: &std::path::Path, prefix: &str, budget: Option<u64>) -> Db {
    let options = Options {
        create_if_missing: true,
        compaction_threads: 1,
        ..Options::default()
    };
    Db::open_with(dir, options, tiered_fs(dir, prefix, budget), &["default"]).unwrap()
}

fn write_and_flush(db: &Db, prefix: &str, count: u32) {
    for index in 0..count {
        db.put(
            "default",
            format!("{prefix}{index:06}").as_bytes(),
            format!("value-{index}").as_bytes(),
        )
        .unwrap();
    }
    db.flush("default").unwrap();
}

fn get(db: &Db, key: &str) -> Option<Vec<u8>> {
    db.get("default", key.as_bytes(), &ReadOptions::default())
        .unwrap()
        .map(|value| value.to_vec())
}

/// Deletes every local `*.sst`, which is the acceptance scenario: the store lost its SST
/// directory and has only the bucket and its manifest.
fn delete_local_ssts(dir: &std::path::Path) -> usize {
    let fs = LocalFileSystem::new();
    let mut deleted = 0;
    for path in fs.list(dir).unwrap() {
        if matches!(
            esker_engine::filename::classify_path(&path),
            Some(esker_engine::filename::FileKind::Sst(_))
        ) {
            fs.delete(&path).unwrap();
            deleted += 1;
        }
    }
    deleted
}

fn docker(action: &str, container: &str) -> bool {
    std::process::Command::new("docker")
        .args([action, container])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// **The acceptance shape, against a real bucket.** A database uploads its SSTs, loses its
/// local SST directory entirely, reopens, and reads every key back over `SigV4` and ranged
/// `GET`s.
#[test]
#[ignore = "needs a MinIO container; see the module docs"]
fn a_database_that_lost_its_ssts_rebuilds_from_the_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = "acceptance";

    let db = open(dir.path(), prefix, None);
    for batch in 0..3 {
        write_and_flush(&db, &format!("b{batch}-"), 300);
        assert_eq!(db.tier_maintenance().unwrap(), 1, "batch {batch}");
    }
    drop(db);

    assert_eq!(delete_local_ssts(dir.path()), 3, "the test deleted nothing");

    let db = open(dir.path(), prefix, None);
    for batch in 0..3 {
        for index in [0u32, 42, 299] {
            let key = format!("b{batch}-{index:06}");
            assert_eq!(
                get(&db, &key).unwrap_or_else(|| panic!("{key} was lost")),
                format!("value-{index}").as_bytes(),
                "{key}"
            );
        }
    }
    let stats = db.tier_stats().unwrap();
    assert!(stats.cache_misses > 0, "the reads did not go to the bucket");
    assert!(stats.ranged_reads > 0, "and they were not ranged");

    // A scan reads far more of each file than a point read, and reading a whole SST over
    // ranged GETs is where an off-by-one in the range arithmetic would surface.
    let mut iter = db.iter("default", &ReadOptions::default()).unwrap();
    iter.seek_to_first();
    let mut seen = 0;
    while iter.valid() {
        seen += 1;
        iter.next();
    }
    assert_eq!(seen, 900, "a full scan over tiered SSTs lost rows");
}

/// **The bucket goes away mid-flight.** The upload fails, the SST stays local, reads keep
/// working, and the retry lands when the endpoint comes back
/// ([ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 2).
///
/// Stops and restarts the shared container, hence `--test-threads=1`. Skipped rather than
/// failed when `docker` is not usable, because "the container could not be stopped" is not a
/// statement about the engine.
#[test]
#[ignore = "needs a MinIO container it stops and restarts; see the module docs"]
fn an_outage_leaves_the_sst_local_and_the_retry_lands() {
    let container = env_or("ESKER_S3_CONTAINER", "esker-minio");
    let dir = tempfile::tempdir().unwrap();
    let prefix = "outage";
    let db = open(dir.path(), prefix, None);

    write_and_flush(&db, "before-", 200);
    assert_eq!(db.tier_maintenance().unwrap(), 1, "the first upload");

    if !docker("stop", &container) {
        eprintln!("skipping: could not stop the container {container}");
        return;
    }

    write_and_flush(&db, "during-", 200);
    assert_eq!(
        db.tier_maintenance().unwrap(),
        0,
        "an upload landed while the endpoint was down"
    );
    // The whole point: nothing else broke.
    assert_eq!(get(&db, "during-000100").unwrap(), b"value-100");
    assert_eq!(get(&db, "before-000100").unwrap(), b"value-100");
    db.put("default", b"still-writable", b"yes").unwrap();
    assert_eq!(
        db.file_locations()
            .values()
            .filter(|location| **location == esker_engine::version::FileLocation::Tiered)
            .count(),
        1,
        "the failed upload must not have been recorded as tiered"
    );

    assert!(docker("start", &container), "restarting {container}");
    // The endpoint takes a moment to accept connections again; the retry loop is what waits.
    let mut landed = 0;
    for _ in 0..60 {
        landed += db.tier_maintenance().unwrap();
        if landed > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    assert_eq!(
        landed, 1,
        "the retry never landed after the endpoint returned"
    );
    assert!(db.tier_stats().unwrap().upload_failures >= 1);
}

/// A cold read of a tiered SST costs a handful of ranged `GET`s, not the whole file. The
/// in-memory store cannot show this, because its "range" is a slice of a `Vec`.
#[test]
#[ignore = "needs a MinIO container; see the module docs"]
fn a_cold_point_read_over_http_is_a_handful_of_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = "ranges";

    let db = open(dir.path(), prefix, None);
    write_and_flush(&db, "k", 5_000);
    db.tier_maintenance().unwrap();
    drop(db);
    delete_local_ssts(dir.path());

    // `batch: 0` so nothing is fetched back: the point is what one cold read costs.
    let dir_path = dir.path().to_path_buf();
    let fs = {
        let endpoint =
            esker_s3::Endpoint::parse(&env_or("ESKER_S3_ENDPOINT", "http://localhost:19000"))
                .unwrap();
        let config = esker_s3::Config::from_store_url(
            &format!(
                "s3://{}/engine/{prefix}",
                env_or("ESKER_S3_BUCKET", "esker")
            ),
            endpoint,
            "us-east-1",
            esker_s3::Credentials::new(
                env_or("ESKER_S3_KEY", "eskertest"),
                env_or("ESKER_S3_SECRET", "eskertest123"),
            ),
        )
        .unwrap();
        let key_prefix = config.prefix.clone();
        TieredFileSystem::new(
            Arc::new(LocalFileSystem::new()),
            Arc::new(esker_s3::S3Client::new(config)),
            &dir_path,
            TierOptions {
                key_prefix,
                background: false,
                batch: 0,
                ..TierOptions::default()
            },
        )
        .unwrap() as Arc<dyn FileSystem>
    };
    let db = Db::open_with(
        &dir_path,
        Options {
            create_if_missing: true,
            compaction_threads: 1,
            ..Options::default()
        },
        fs,
        &["default"],
    )
    .unwrap();

    assert_eq!(get(&db, "k002500").unwrap(), b"value-2500");
    let stats = db.tier_stats().unwrap();
    assert!(
        stats.ranged_reads > 0 && stats.ranged_reads < 32,
        "a cold point read took {} ranged reads",
        stats.ranged_reads
    );
    assert_eq!(stats.fetches, 0, "nothing should have been fetched whole");
}

/// **The claim marker against a real object store.** `tests/tier_claim.rs` decides all of the
/// behaviour against `MemoryStore`; what only a real endpoint can tell us is that the marker
/// survives a genuine `PutObject`/`GetObject` round trip — that a 37-byte object comes back as
/// 37 bytes with its CRC intact, and that a missing marker arrives as a 404 the claim path
/// recognises rather than as an error it reports.
#[test]
#[ignore = "needs the MinIO container; see this file's header"]
fn a_prefix_claimed_over_http_refuses_the_second_database() {
    let prefix = unique_prefix("claim");
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let local = LocalFileSystem::new();
    let first = Identity::new(id_for_directory(&local, first_dir.path()).unwrap()).of(1, 1);
    let second = Identity::new(id_for_directory(&local, second_dir.path()).unwrap()).of(1, 2);

    // A 404 for the marker is the ordinary first-open case, not a failure.
    claiming_fs(first_dir.path(), &prefix, first, false).expect("the first database claims it");

    // And it round-trips: the same database reopens over HTTP, reading back what it wrote.
    claiming_fs(first_dir.path(), &prefix, first, false).expect("the claimant reopens");

    let error = claiming_fs(second_dir.path(), &prefix, second, false)
        .expect_err("a second database must be refused over HTTP too");
    let text = error.to_string();
    assert!(text.contains(&first.claim.to_string()), "{text}");
    assert!(text.contains(&second.claim.to_string()), "{text}");
    assert!(
        text.contains("store 1") && text.contains("store 2"),
        "{text}"
    );
}
