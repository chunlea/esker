//! The S3 client against a real S3-compatible server.
//!
//! Every other test in this crate checks the client against its own idea of what a server does.
//! That is worth something for the parser and worth **nothing** for `SigV4`: a signing bug
//! produces a consistent, plausible, wrong signature, and only a server that computes the same
//! HMAC can say so. The published vectors in `src/sigv4.rs` are the first oracle; this is the
//! second, and it is the one that covers the parts a vector does not — the exact header set,
//! the payload hash, the request target, and whether the two encodings of the path agree.
//!
//! `#[ignore]` because it needs a container. Run it with:
//!
//! ```sh
//! docker run -d --name esker-minio -p 19000:9000 \
//!     -e MINIO_ROOT_USER=eskertest -e MINIO_ROOT_PASSWORD=eskertest123 \
//!     minio/minio:latest server /data
//! docker exec esker-minio mc alias set local http://127.0.0.1:9000 eskertest eskertest123
//! docker exec esker-minio mc mb --ignore-existing local/esker
//!
//! cargo test -p esker-s3 --test minio -- --ignored --test-threads=1
//! ```
//!
//! Port 19000 rather than 9000 so it cannot collide with a MinIO somebody is already running.
//! `ESKER_S3_ENDPOINT`, `ESKER_S3_BUCKET`, `ESKER_S3_KEY` and `ESKER_S3_SECRET` override the
//! defaults, so the same tests run against any S3-compatible endpoint reachable over HTTP.
//!
//! Verified against `minio/minio` at digest
//! `sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e`
//! (`RELEASE.2025-08-13`-era `mc`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_s3::{Config, Credentials, Endpoint, ObjectStore, S3Client};

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

/// A client pointed at the container, under a prefix unique to this run so that two runs — or
/// two of these tests in parallel — cannot collide on a key.
fn client(prefix: &str) -> S3Client {
    let endpoint = Endpoint::parse(&env_or("ESKER_S3_ENDPOINT", "http://localhost:19000"))
        .expect("the endpoint must parse");
    let bucket = env_or("ESKER_S3_BUCKET", "esker");
    let credentials = Credentials::new(
        env_or("ESKER_S3_KEY", "eskertest"),
        env_or("ESKER_S3_SECRET", "eskertest123"),
    );
    let config = Config::from_store_url(
        &format!("s3://{bucket}/tests/{prefix}"),
        endpoint,
        "us-east-1",
        credentials,
    )
    .expect("the store URL must parse");
    S3Client::new(config)
}

/// Bytes that compress badly and differ at every offset, so a ranged read that returns the
/// wrong window is caught by value and not only by length.
fn payload(len: usize) -> Vec<u8> {
    let mut rng = esker_base::rng::Pcg32::new(0x5153_5f74_6965_72, 1);
    let mut bytes = vec![0u8; len];
    rng.fill_bytes(&mut bytes);
    bytes
}

/// The whole surface, in the order tiering uses it: upload, read back, read a window, list,
/// delete. If `SigV4` were wrong, the first call would fail with `403`.
#[test]
#[ignore = "needs a MinIO container; see the module docs"]
fn the_four_calls_against_minio() {
    let client = client("four-calls");
    let key = client.config().key_for("000007.sst");
    let body = payload(64 * 1024);

    let etag = client.put(&key, &body).expect("PutObject");
    assert!(etag.is_some(), "MinIO always returns an ETag");

    let whole = client.get(&key).expect("GetObject");
    assert_eq!(whole.body, body, "what came back is not what went up");
    assert_eq!(whole.total_size, Some(body.len() as u64));
    assert_eq!(whole.etag, etag, "the ETag must survive the round trip");

    // A window from the middle, checked by value.
    let window = client
        .get_range(&key, 4096, 1024, etag.as_deref())
        .expect("ranged GetObject");
    assert_eq!(window.body, &body[4096..5120]);
    assert_eq!(window.total_size, Some(body.len() as u64));

    // A window that runs off the end returns what remains, like a short read at EOF.
    let tail = client
        .get_range(&key, body.len() as u64 - 10, 1000, None)
        .expect("a range past the end");
    assert_eq!(tail.body, &body[body.len() - 10..]);

    let listed = client.list(&client.config().prefix).expect("ListObjectsV2");
    let found = listed
        .iter()
        .find(|object| object.key == key)
        .expect("the object we just uploaded must be in the listing");
    assert_eq!(found.size, body.len() as u64);
    assert_eq!(Some(found.etag.clone()), etag, "listing and header agree");

    client.delete(&key).expect("DeleteObject");
    assert!(client.get(&key).unwrap_err().is_not_found());
    assert!(
        client.delete(&key).is_ok(),
        "deleting an absent key must succeed"
    );
}

/// A wrong secret must produce a `403` that is *not* retryable — otherwise the uploader
/// hammers the endpoint forever over a typo
/// ([ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 2).
#[test]
#[ignore = "needs a MinIO container; see the module docs"]
fn a_wrong_secret_is_a_403_and_not_retryable() {
    let mut client = client("wrong-secret");
    let mut config = client.config().clone();
    config.credentials = Credentials::new(
        env_or("ESKER_S3_KEY", "eskertest"),
        "definitely-not-the-secret",
    );
    client = S3Client::new(config);

    let err = client
        .put(&client.config().key_for("000001.sst"), b"x")
        .expect_err("a wrong secret must not be accepted");
    assert!(
        err.to_string().contains("403"),
        "expected a 403, got: {err}"
    );
    assert!(!err.is_retryable(), "a bad signature is not worth retrying");
}

/// The keys tiering actually uses are boring, but the encoding has to be right for the ones
/// that are not — and a path encoded differently in the request than in the signature is a
/// `403`, which is exactly what this asserts does *not* happen.
#[test]
#[ignore = "needs a MinIO container; see the module docs"]
fn awkward_keys_survive_the_round_trip() {
    let client = client("awkward");
    for name in [
        "plain.sst",
        "with space.sst",
        "with+plus.sst",
        "with=equals.sst",
        "nested/deeper/000042.sst",
        "tilde~and.dots..sst",
    ] {
        let key = client.config().key_for(name);
        client.put(&key, name.as_bytes()).unwrap_or_else(|err| {
            panic!("PutObject {name}: {err}");
        });
        let got = client
            .get(&key)
            .unwrap_or_else(|err| panic!("GetObject {name}: {err}"));
        assert_eq!(got.body, name.as_bytes(), "{name}");
        client.delete(&key).unwrap();
    }
}

/// A listing longer than one page. `ListObjectsV2` caps a page at 1,000 keys, so this uses the
/// `max-keys` MinIO honours by uploading enough objects to force a continuation — the loop that
/// follows the token is otherwise never executed by any test.
#[test]
#[ignore = "needs a MinIO container and uploads 1,050 objects; see the module docs"]
fn a_listing_longer_than_one_page_paginates() {
    let client = client("paginated");
    let prefix = client.config().prefix.clone();
    for index in 0..1_050u32 {
        let key = client.config().key_for(&format!("{index:06}.sst"));
        client.put(&key, b"x").expect("PutObject");
    }

    let listed = client.list(&prefix).expect("ListObjectsV2");
    assert_eq!(
        listed.len(),
        1_050,
        "the second page was dropped: {} objects listed",
        listed.len()
    );

    for object in &listed {
        client.delete(&object.key).expect("DeleteObject");
    }
    assert!(client.list(&prefix).expect("ListObjectsV2").is_empty());
}
