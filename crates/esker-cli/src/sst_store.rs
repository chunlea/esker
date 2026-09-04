//! Turning `--sst-store s3://bucket/prefix` into a filesystem.
//!
//! Both `esker server` and `esker bench` take the flag and both need the same thing built from
//! it, so it is built once, here. The endpoint and the credentials come from the **environment**
//! and never from a flag: a secret on a command line is a secret in everybody's `ps` output, and
//! in the shell history of whoever typed it.
//!
//! | Variable | Default |
//! |---|---|
//! | `ESKER_S3_ENDPOINT` | `http://localhost:19000` — the `MinIO` the tests use |
//! | `ESKER_S3_KEY` | `eskertest` |
//! | `ESKER_S3_SECRET` | `eskertest123` |
//! | `ESKER_S3_REGION` | `us-east-1` |
//! | `ESKER_S3_CA_CERT` | unset — the host's own CA bundle |
//!
//! `ESKER_S3_CA_CERT` names a PEM file of roots for an `https://` endpoint, which is what a
//! self-signed `MinIO` needs; unset, the host's bundle is used. It is a variable rather than a
//! flag for consistency with the four above, not for secrecy — a CA certificate is public, and
//! the reason those four are variables is that a secret on a command line is in everybody's `ps`
//! output. An `https://` endpoint on a build without the `tls` feature is refused by
//! `Endpoint::parse` with a message naming the feature, never downgraded to plaintext.
//!
//! The defaults are the local container's, which makes the flag usable in a test without four
//! more environment variables and useless against anything real without them — which is the
//! right way round.
//!
//! # One prefix per database
//!
//! Two databases sharing an `--sst-store` prefix would overwrite each other's `000007.sst`,
//! silently, because the object key is derived from a file number and file numbers restart at
//! one in every database. Two things stop that now:
//!
//! * [`for_node`] derives a per-store prefix, so an operator who gets the cluster right cannot
//!   get this wrong by hand;
//! * and the prefix itself is **claimed**. The first database to open one writes a marker naming
//!   itself; a database that is not the claimant is refused at startup with both identities in
//!   the message ([`esker_engine::fs::claim`], `docs/adr/0029-the-sst-store-claim.md`). The
//!   derivation is the convenience; the claim is the guarantee.

use std::path::Path;
use std::sync::Arc;

use esker_engine::fs::tier::{TierOptions, TieredFileSystem};
use esker_engine::fs::{FileSystem, LocalFileSystem, claim};

/// What the CLI knows about the database asking for a tier.
///
/// The ids are informational — they are in the claim marker so that a refusal names something an
/// operator recognises rather than only a random number — and `adopt` is the one decision that
/// must be made out loud.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Claim {
    /// The cluster, or `0` when there is none (a benchmark, a standalone store).
    pub(crate) cluster_id: u64,
    /// The store, or `0`.
    pub(crate) store_id: u64,
    /// Claim a prefix that holds objects but no marker. `false` refuses, which is the default:
    /// those objects may belong to a live database written before markers existed.
    pub(crate) adopt: bool,
}

/// Reads `name` from the environment, or returns `fallback`.
fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.to_string())
}

/// Which roots verify an `https://` endpoint: the one `ESKER_S3_CA_CERT` names, or the host's.
fn tls_roots() -> esker_s3::TlsRoots {
    std::env::var_os("ESKER_S3_CA_CERT").map_or(esker_s3::TlsRoots::Platform, |path| {
        esker_s3::TlsRoots::File(path.into())
    })
}

/// The store URL for one node of a cluster: `s3://bucket/prefix` becomes
/// `s3://bucket/prefix/node-N`.
///
/// A cluster is several databases, one per store, and they must not share a prefix. Deriving it
/// from the node id means an operator who gets the cluster right cannot get this wrong.
#[must_use]
pub(crate) fn for_node(store_url: &str, node: u64) -> String {
    format!("{}/node-{node}", store_url.trim_end_matches('/'))
}

/// The client for one store URL, and the key prefix it resolves to.
///
/// The half of [`filesystem`] that is only about the object store, for a caller that wants the
/// bucket and not a database on top of it — `esker sst-store reconcile`, which compares what the
/// prefix holds against what a manifest names. Same environment, same parser, so the tool and the
/// server can never disagree about which prefix a URL means.
pub(crate) fn object_store(
    store_url: &str,
) -> Result<(Arc<dyn esker_s3::ObjectStore>, String), String> {
    let endpoint_url = env_or("ESKER_S3_ENDPOINT", "http://localhost:19000");
    let endpoint = esker_s3::Endpoint::parse(&endpoint_url).map_err(|err| err.to_string())?;
    let credentials = esker_s3::Credentials::new(
        env_or("ESKER_S3_KEY", "eskertest"),
        env_or("ESKER_S3_SECRET", "eskertest123"),
    );
    let region = env_or("ESKER_S3_REGION", "us-east-1");
    let mut config = esker_s3::Config::from_store_url(store_url, endpoint, region, credentials)
        .map_err(|err| err.to_string())?;
    config.tls_roots = tls_roots();
    let prefix = config.prefix.clone();
    let client = esker_s3::S3Client::open(config).map_err(|err| err.to_string())?;
    Ok((Arc::new(client), prefix))
}

/// The filesystem the engine should run on.
///
/// `None` for `store_url` is the ordinary local filesystem, which is every store before phase 6b
/// and every store that does not ask for a tier.
///
/// `background` decides whether the uploader gets a thread. A server wants `true`, because
/// [ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 1 requires that no
/// flush ever wait on an upload. A benchmark wants `false`, because it wants a steady state
/// rather than a measurement of the uploader interfering with the measurement.
pub(crate) fn filesystem(
    store_url: Option<&str>,
    dir: &Path,
    local_budget: Option<u64>,
    background: bool,
    who: Claim,
) -> Result<Arc<dyn FileSystem>, String> {
    let local = Arc::new(LocalFileSystem::new());
    let Some(store_url) = store_url else {
        return Ok(local);
    };

    let endpoint_url = env_or("ESKER_S3_ENDPOINT", "http://localhost:19000");
    let endpoint = esker_s3::Endpoint::parse(&endpoint_url).map_err(|err| err.to_string())?;
    let credentials = esker_s3::Credentials::new(
        env_or("ESKER_S3_KEY", "eskertest"),
        env_or("ESKER_S3_SECRET", "eskertest123"),
    );
    let region = env_or("ESKER_S3_REGION", "us-east-1");

    let mut config = esker_s3::Config::from_store_url(store_url, endpoint, region, credentials)
        .map_err(|err| err.to_string())?;
    config.tls_roots = tls_roots();
    let key_prefix = config.prefix.clone();
    // `open` rather than `new`: a trust store that cannot be read is a startup error here, not a
    // failure on the first upload an hour later.
    let client = Arc::new(esker_s3::S3Client::open(config).map_err(|err| err.to_string())?);

    // The directory has to exist before the tier lists it for half-written fetches — and before
    // the claim id is written into it.
    std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;

    // Who this database is. Drawn once and kept in its own directory, so it is the *database*
    // that owns a prefix and not the flags it was started with: two `esker bench` runs share
    // every flag they have and must still not share a prefix.
    let claim = claim::id_for_directory(local.as_ref(), dir)
        .map_err(|err| format!("{}/{}: {err}", dir.display(), claim::CLAIM_ID_FILE))?;
    let identity = claim::Identity::new(claim).of(who.cluster_id, who.store_id);

    TieredFileSystem::new(
        local,
        client,
        dir,
        TierOptions {
            key_prefix,
            local_budget,
            background,
            identity: Some(identity),
            adopt_unclaimed: who.adopt,
            ..TierOptions::default()
        },
    )
    .map(|tier| tier as Arc<dyn FileSystem>)
    .map_err(|err| format!("opening the SST tier at {store_url}: {err}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Claim, filesystem, for_node};

    #[test]
    fn a_node_gets_its_own_prefix() {
        assert_eq!(for_node("s3://esker/c1", 1), "s3://esker/c1/node-1");
        assert_eq!(for_node("s3://esker/c1/", 3), "s3://esker/c1/node-3");
        assert_eq!(for_node("s3://esker", 2), "s3://esker/node-2");
        // Distinct per node is the whole point, so it gets its own assertion.
        assert_ne!(for_node("s3://esker/c1", 1), for_node("s3://esker/c1", 2));
    }

    #[test]
    fn no_store_url_is_the_local_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let fs = filesystem(None, dir.path(), None, true, Claim::default()).unwrap();
        assert!(
            esker_engine::fs::FileSystem::tier(fs.as_ref()).is_none(),
            "a store that asked for no tier must not have one"
        );
    }

    /// A bad URL fails when the store is opened, not on the first flush.
    ///
    /// The `https://` refusal (ADR 0025) is not asserted here: it depends on an environment
    /// variable, and `std::env::set_var` is `unsafe` in edition 2024 while `CLAUDE.md` denies
    /// `unsafe_code` workspace-wide. `esker-s3`'s `an_https_endpoint_is_refused_with_a_reason`
    /// covers it, at the layer that actually decides.
    #[test]
    fn a_bad_store_url_fails_early_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            filesystem(Some("esker/tier"), dir.path(), None, true, Claim::default()).unwrap_err();
        assert!(err.contains("s3://"), "{err}");
        let err =
            filesystem(Some("s3:///tier"), dir.path(), None, true, Claim::default()).unwrap_err();
        assert!(err.contains("no bucket"), "{err}");
    }
}
