//! The store's view of one region, and the checks every request runs against it.
//!
//! `CLAUDE.md` invariant 5: *every request carries a region epoch; a stale epoch is rejected
//! with a redirect hint, and a store never serves a request for a range it no longer owns.*
//!
//! In this phase there is exactly one region, it covers the whole key space, and its epoch
//! never moves — so every check here always passes in normal use. They run anyway, and they
//! are tested against epochs that are behind, ahead and sideways, because **an invariant that
//! is only enforced once it can fail is one that was never tested.** When phase 4 makes splits
//! real, this is the code that has to already be right.
//!
//! The key-range checks are in *user* key space, the space a client speaks. The `'r'` prefix
//! belongs to storage and is added afterwards ([`crate::rawkv`]).

use bytes::Bytes;
use esker_proto::{Epoch, ProtoError, Region, RequestHeader};

/// One region as this store holds it.
///
/// The epoch lives inside the [`Region`], as `docs/DESIGN.md` §6 defines it: it is the
/// region's own version, and a caller holding a `Region` holds everything needed both to
/// address it and to know whether what it holds is still true.
#[derive(Debug, Clone)]
pub struct RegionMeta {
    region: Region,
}

impl RegionMeta {
    /// The first region of a new store: covering everything, with this store's single peer
    /// (`docs/DESIGN.md` §7).
    #[must_use]
    pub fn bootstrap(region_id: u64, store_id: u64, peer_id: u64) -> Self {
        Self {
            region: Region::bootstrap(region_id, store_id, peer_id),
        }
    }

    /// Wraps a region this store already owns.
    #[must_use]
    pub fn new(region: Region) -> Self {
        Self { region }
    }

    /// The region itself, as it goes on the wire.
    #[must_use]
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// Its id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.region.id
    }

    /// Its epoch.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.region.epoch
    }

    /// Checks the header of an incoming request: the right region, at the right epoch, on a
    /// peer this store actually has.
    ///
    /// The order matters. A wrong region id is [`ProtoError::RegionNotFound`], because the
    /// client is at the wrong store entirely; a right region at the wrong epoch is
    /// [`ProtoError::EpochNotMatch`] **carrying the current region**, so one round trip
    /// refreshes the client's cache rather than starting a guessing game.
    ///
    /// `header.peer == 0` means "no opinion about the leader", which is the truth for a client
    /// that has just connected and has not been told one. It is accepted. A header naming a
    /// peer this region does not have is [`ProtoError::NotLeader`] with a peer that exists as
    /// the hint.
    pub fn check(&self, header: &RequestHeader) -> Result<(), ProtoError> {
        if header.region_id != self.region.id {
            return Err(ProtoError::RegionNotFound {
                region_id: header.region_id,
            });
        }
        if header.epoch != self.region.epoch {
            return Err(ProtoError::EpochNotMatch {
                current_regions: vec![self.region.clone()],
            });
        }
        if header.peer != 0
            && !self
                .region
                .peers
                .iter()
                .any(|peer| peer.peer_id == header.peer)
        {
            return Err(ProtoError::NotLeader {
                region_id: self.region.id,
                leader_hint: self.region.peers.first().map(|peer| peer.peer_id),
            });
        }
        Ok(())
    }

    /// Checks that a single key is inside this region.
    pub fn check_key(&self, key: &[u8]) -> Result<(), ProtoError> {
        if self.region.contains(key) {
            return Ok(());
        }
        Err(self.not_in_region(key))
    }

    /// Checks that every key of a batch is inside this region.
    ///
    /// All of them, before any of them is written: a batch is one atomic engine write, so
    /// finding the bad key half way through would mean either a partial application or an
    /// undo, and neither is something to build on.
    pub fn check_keys<'a>(
        &self,
        keys: impl IntoIterator<Item = &'a [u8]>,
    ) -> Result<(), ProtoError> {
        for key in keys {
            self.check_key(key)?;
        }
        Ok(())
    }

    /// Checks that `[start, end)` is inside this region, where an empty `end` means the end of
    /// the key space.
    ///
    /// An inverted range is a caller error rather than an empty answer, because every way of
    /// reading one silently — as empty, as reversed — is a way of not doing what was asked.
    pub fn check_range(&self, start: &[u8], end: &[u8]) -> Result<(), ProtoError> {
        if !end.is_empty() && start > end {
            return Err(ProtoError::invalid(format!(
                "range start {start:?} is after its end {end:?}"
            )));
        }
        if self.region.contains_range(start, end) {
            return Ok(());
        }
        Err(self.not_in_region(start))
    }

    fn not_in_region(&self, key: &[u8]) -> ProtoError {
        ProtoError::KeyNotInRegion {
            key: Bytes::copy_from_slice(key),
            region_id: self.region.id,
            start_key: self.region.start_key.clone(),
            end_key: self.region.end_key.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RegionMeta;
    use bytes::Bytes;
    use esker_proto::{Epoch, Peer, ProtoError, Region, RequestHeader};

    fn whole() -> RegionMeta {
        RegionMeta::bootstrap(1, 1, 1)
    }

    fn header(region_id: u64, epoch: Epoch, peer: u64) -> RequestHeader {
        RequestHeader::new(region_id, epoch, peer)
    }

    #[test]
    fn a_matching_header_passes() {
        let meta = whole();
        meta.check(&header(1, Epoch::INITIAL, 1)).unwrap();
    }

    /// The client sends peer 0 when it has not been told who the leader is, which is the truth
    /// for a freshly connected one. Rejecting it would make every first request fail.
    #[test]
    fn peer_zero_means_no_opinion_and_is_accepted() {
        whole().check(&header(1, Epoch::INITIAL, 0)).unwrap();
    }

    #[test]
    fn a_header_naming_a_peer_we_do_not_have_gets_a_hint() {
        let error = whole().check(&header(1, Epoch::INITIAL, 99)).unwrap_err();
        assert_eq!(
            error,
            ProtoError::NotLeader {
                region_id: 1,
                leader_hint: Some(1),
            }
        );
        assert!(error.is_retryable(), "a redirect must be retryable");
    }

    #[test]
    fn a_wrong_region_id_is_not_an_epoch_problem() {
        let error = whole().check(&header(7, Epoch::INITIAL, 1)).unwrap_err();
        assert_eq!(error, ProtoError::RegionNotFound { region_id: 7 });
    }

    /// A mismatch in *either* counter, in either direction, is a mismatch: the two move on
    /// different events, and a client that is ahead is as wrong as one that is behind.
    #[test]
    fn any_epoch_mismatch_returns_the_current_region() {
        let meta = RegionMeta::new(Region {
            id: 1,
            start_key: Bytes::new(),
            end_key: Bytes::new(),
            peers: vec![Peer::voter(1, 1)],
            epoch: Epoch::new(2, 3),
        });

        for wrong in [
            Epoch::new(1, 3),
            Epoch::new(2, 2),
            Epoch::new(3, 3),
            Epoch::new(2, 4),
        ] {
            let error = meta.check(&header(1, wrong, 1)).unwrap_err();
            match error {
                ProtoError::EpochNotMatch { current_regions } => {
                    assert_eq!(current_regions.len(), 1);
                    assert_eq!(
                        current_regions[0].epoch,
                        Epoch::new(2, 3),
                        "the error must carry the epoch the client should adopt"
                    );
                }
                other => panic!("{wrong:?} gave {other:?}"),
            }
        }
    }

    /// The first region covers everything, including the empty key and keys that look like
    /// namespace prefixes. This is the check that would break if `contains` treated an empty
    /// `end_key` as an empty range.
    #[test]
    fn the_bootstrap_region_contains_every_key() {
        let meta = whole();
        for key in [&b""[..], b"a", b"r", b"\xff\xff\xff\xff"] {
            meta.check_key(key).unwrap_or_else(|error| {
                panic!("the whole-key-space region rejected {key:?}: {error}")
            });
        }
        meta.check_range(b"", b"").unwrap();
        meta.check_range(b"a", b"z").unwrap();
    }

    #[test]
    fn a_key_outside_a_bounded_region_is_refused_with_its_range() {
        let meta = RegionMeta::new(Region {
            id: 1,
            start_key: Bytes::from_static(b"d"),
            end_key: Bytes::from_static(b"m"),
            peers: vec![Peer::voter(1, 1)],
            epoch: Epoch::INITIAL,
        });

        meta.check_key(b"d").unwrap();
        meta.check_key(b"l").unwrap();
        let error = meta.check_key(b"m").unwrap_err();
        match error {
            ProtoError::KeyNotInRegion {
                key,
                region_id,
                start_key,
                end_key,
            } => {
                assert_eq!(key, Bytes::from_static(b"m"));
                assert_eq!(region_id, 1);
                assert_eq!(start_key, Bytes::from_static(b"d"));
                assert_eq!(end_key, Bytes::from_static(b"m"));
            }
            other => panic!("{other:?}"),
        }

        // A scan that would run off the end of a bounded region is refused too, including the
        // unbounded one, which is the case a plain comparison gets wrong.
        assert!(meta.check_range(b"d", b"n").is_err());
        assert!(meta.check_range(b"d", b"").is_err());
        meta.check_range(b"d", b"m").unwrap();
    }

    /// Every key of a batch is checked before any of it is written, because the write is
    /// atomic and there is no half of it to keep.
    #[test]
    fn a_batch_is_refused_whole() {
        let meta = RegionMeta::new(Region {
            id: 1,
            start_key: Bytes::new(),
            end_key: Bytes::from_static(b"m"),
            peers: vec![Peer::voter(1, 1)],
            epoch: Epoch::INITIAL,
        });
        let keys: Vec<&[u8]> = vec![b"a", b"b", b"z"];
        assert!(meta.check_keys(keys).is_err());
        let good: Vec<&[u8]> = vec![b"a", b"b"];
        meta.check_keys(good).unwrap();
    }

    #[test]
    fn an_inverted_range_is_a_caller_error_not_an_empty_answer() {
        let error = whole().check_range(b"z", b"a").unwrap_err();
        assert!(
            matches!(error, ProtoError::InvalidRequest { .. }),
            "{error:?}"
        );
    }
}
