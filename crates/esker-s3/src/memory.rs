//! An [`ObjectStore`] in a `BTreeMap`, for tests that are not about S3.
//!
//! The tiered filesystem has a lot of behaviour that has nothing to do with HTTP: what gets
//! uploaded and when, what the governor evicts, which objects the sweep may delete. Testing
//! that against a container would make those tests slow, flaky and conditional on Docker,
//! which is how a test suite ends up with the interesting cases behind `#[ignore]`.
//!
//! So the engine's tests run against this, and the S3 client's own tests run against `MinIO`. The
//! seam between them is [`ObjectStore`], which is why it is deliberately
//! smaller than S3.
//!
//! It also fails on demand: [`MemoryStore::fail_next_puts`] and friends are how the
//! failure-semantics tests in [ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md)
//! get a `PutObject` to fail at exactly the moment they want, the same way `FaultFs` does for
//! the filesystem.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::{Error, Result};
use crate::{GetResponse, ObjectStore, ObjectSummary};

/// An in-memory object store.
#[derive(Debug, Default)]
pub struct MemoryStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// How many more `put` calls will fail before any succeed.
    failing_puts: AtomicUsize,
    /// How many more `get` calls will fail.
    failing_gets: AtomicUsize,
    /// Counters the tests assert on, which is how "the cache was hit" becomes checkable.
    puts: AtomicUsize,
    gets: AtomicUsize,
    deletes: AtomicUsize,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes the next `count` uploads fail with a retryable error.
    pub fn fail_next_puts(&self, count: usize) {
        self.failing_puts.store(count, Ordering::SeqCst);
    }

    /// Makes the next `count` reads fail with a retryable error.
    pub fn fail_next_gets(&self, count: usize) {
        self.failing_gets.store(count, Ordering::SeqCst);
    }

    /// How many `put`, `get` and `delete` calls have been attempted.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.puts.load(Ordering::SeqCst),
            self.gets.load(Ordering::SeqCst),
            self.deletes.load(Ordering::SeqCst),
        )
    }

    /// Every key currently stored, in order.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.objects
            .lock()
            .map(|objects| objects.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether `key` is stored.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.objects
            .lock()
            .is_ok_and(|objects| objects.contains_key(key))
    }

    /// Replaces an object's bytes without going through `put`, so a test can simulate the
    /// object having been changed underneath us — which is what the `ETag` check exists for.
    pub fn corrupt(&self, key: &str, body: Vec<u8>) {
        if let Ok(mut objects) = self.objects.lock() {
            objects.insert(key.to_string(), body);
        }
    }

    /// A deterministic stand-in for an entity tag: the length and the CRC of the bytes.
    ///
    /// Not an MD5, and deliberately not: nothing may depend on an `ETag` being any particular
    /// function, only on it changing when the object does.
    fn etag(body: &[u8]) -> String {
        format!("{:08x}-{}", esker_base::crc32c::checksum(body), body.len())
    }

    fn take_failure(counter: &AtomicUsize) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }
}

impl ObjectStore for MemoryStore {
    fn put(&self, key: &str, body: &[u8]) -> Result<Option<String>> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if Self::take_failure(&self.failing_puts) {
            return Err(Error::io(
                "PutObject",
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "injected"),
            ));
        }
        let etag = Self::etag(body);
        let mut objects = self
            .objects
            .lock()
            .map_err(|_| Error::Config("the memory store was poisoned".into()))?;
        objects.insert(key.to_string(), body.to_vec());
        Ok(Some(etag))
    }

    fn get_range(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        expected_etag: Option<&str>,
    ) -> Result<GetResponse> {
        let whole = self.get(key)?;
        if let (Some(expected), Some(actual)) = (expected_etag, whole.etag.as_deref())
            && expected != actual
        {
            return Err(Error::RangeMismatch {
                key: key.to_string(),
                detail: format!("etag {actual} is not the expected {expected}"),
            });
        }
        let total = whole.body.len() as u64;
        let start = offset.min(total);
        let end = start.saturating_add(len).min(total);
        // The casts are bounded by `total`, which came from a `Vec`'s length.
        #[allow(clippy::cast_possible_truncation)]
        let slice = whole.body[start as usize..end as usize].to_vec();
        Ok(GetResponse {
            body: slice,
            etag: whole.etag,
            total_size: Some(total),
        })
    }

    fn get(&self, key: &str) -> Result<GetResponse> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        if Self::take_failure(&self.failing_gets) {
            return Err(Error::io(
                "GetObject",
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "injected"),
            ));
        }
        let objects = self
            .objects
            .lock()
            .map_err(|_| Error::Config("the memory store was poisoned".into()))?;
        let body = objects.get(key).ok_or_else(|| Error::Status {
            operation: "GetObject",
            key: key.to_string(),
            status: 404,
            body: String::new(),
        })?;
        Ok(GetResponse {
            etag: Some(Self::etag(body)),
            total_size: Some(body.len() as u64),
            body: body.clone(),
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectSummary>> {
        let objects = self
            .objects
            .lock()
            .map_err(|_| Error::Config("the memory store was poisoned".into()))?;
        Ok(objects
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, body)| ObjectSummary {
                key: key.clone(),
                size: body.len() as u64,
                etag: Self::etag(body),
            })
            .collect())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        let mut objects = self
            .objects
            .lock()
            .map_err(|_| Error::Config("the memory store was poisoned".into()))?;
        objects.remove(key);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::MemoryStore;
    use crate::ObjectStore;

    #[test]
    fn a_round_trip_and_a_range() {
        let store = MemoryStore::new();
        let etag = store.put("tier/000007.sst", b"0123456789").unwrap();
        assert!(store.contains("tier/000007.sst"));

        let ranged = store
            .get_range("tier/000007.sst", 3, 4, etag.as_deref())
            .unwrap();
        assert_eq!(ranged.body, b"3456");
        assert_eq!(ranged.total_size, Some(10));

        // Past the end returns what remains, like a short read at EOF.
        let tail = store.get_range("tier/000007.sst", 8, 100, None).unwrap();
        assert_eq!(tail.body, b"89");
        let past = store.get_range("tier/000007.sst", 100, 4, None).unwrap();
        assert!(past.body.is_empty());
    }

    #[test]
    fn an_etag_that_no_longer_matches_is_a_range_mismatch() {
        let store = MemoryStore::new();
        let etag = store.put("k", b"original").unwrap().unwrap();
        store.corrupt("k", b"different".to_vec());
        let err = store.get_range("k", 0, 4, Some(&etag)).unwrap_err();
        assert!(err.to_string().contains("etag"), "{err}");
    }

    #[test]
    fn injected_failures_are_retryable_and_run_out() {
        let store = MemoryStore::new();
        store.fail_next_puts(2);
        for _ in 0..2 {
            let err = store.put("k", b"v").unwrap_err();
            assert!(err.is_retryable(), "{err}");
        }
        assert!(store.put("k", b"v").is_ok());
        assert_eq!(store.counts().0, 3);
    }

    #[test]
    fn a_missing_object_is_not_found_and_listing_respects_the_prefix() {
        let store = MemoryStore::new();
        store.put("tier/a", b"1").unwrap();
        store.put("tier/b", b"22").unwrap();
        store.put("other/c", b"333").unwrap();

        assert!(store.get("nothing").unwrap_err().is_not_found());
        let listed = store.list("tier/").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].key, "tier/a");
        assert_eq!(listed[1].size, 2);

        store.delete("tier/a").unwrap();
        assert!(store.delete("tier/a").is_ok(), "deleting twice is fine");
        assert_eq!(store.list("tier/").unwrap().len(), 1);
    }
}
