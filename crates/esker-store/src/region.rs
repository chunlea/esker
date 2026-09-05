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
use esker_proto::{Epoch, Peer, ProtoError, Region, RequestHeader};

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

    /// A region covering the whole key space, replicated by every peer in `peers`.
    ///
    /// The peer list is what makes a redirect usable: a client that receives
    /// `NotLeader { leader_hint }` has a *peer* id, and only this list turns it into the store
    /// to send to (`docs/DESIGN.md` §10). A single-peer region can never redirect anywhere.
    #[must_use]
    pub fn replicated(region_id: u64, peers: Vec<Peer>) -> Self {
        Self {
            region: Region {
                id: region_id,
                start_key: Bytes::new(),
                end_key: Bytes::new(),
                peers,
                epoch: Epoch::INITIAL,
            },
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
        self.check_peer(header.peer)
    }

    /// Checks that a header names a peer this region actually has.
    ///
    /// `peer == 0` means "no opinion about the leader", which is the truth for a client that has
    /// just connected and has not been told one. It is accepted. A header naming a peer this
    /// region does not have is [`ProtoError::NotLeader`] with a peer that exists as the hint —
    /// including when that peer belongs to a *different* region on this same store, which is the
    /// mistake a store-wide peer list would make.
    pub fn check_peer(&self, peer: u64) -> Result<(), ProtoError> {
        if peer != 0 && !self.region.peers.iter().any(|have| have.peer_id == peer) {
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

    /// Checks every key a request touches against this region's range.
    ///
    /// The direct path checks these inside each handler. The replicated path has to check them
    /// *before* proposing, because an entry that reaches the log is applied on every peer — a
    /// key-range mistake caught at apply time would be caught three times and fix nothing.
    pub fn check_scope(&self, request: &esker_proto::RawKvReq) -> Result<(), ProtoError> {
        use esker_proto::RawKvReq;
        match request {
            RawKvReq::Get { key }
            | RawKvReq::Delete { key, .. }
            | RawKvReq::Put { key, .. }
            | RawKvReq::CompareAndSwap { key, .. } => self.check_key(key),
            RawKvReq::BatchGet { keys } => self.check_keys(keys.iter().map(|key| &key[..])),
            RawKvReq::BatchPut { pairs, .. } => {
                self.check_keys(pairs.iter().map(|(key, _)| &key[..]))
            }
            RawKvReq::DeleteRange { start, end, .. } => self.check_range(start, end),
            // A scan's bounds are clamped to the region rather than refused, which is what
            // `rawkv::scan_bounds` does on the direct path.
            RawKvReq::Scan { .. } => Ok(()),
        }
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

/// The key range one `TxnKv` request touches, so a stale epoch can be answered with the regions
/// that now cover it rather than with the one that was asked for.
///
/// A `Prewrite`'s range is over the keys it **writes**, not its primary: the primary may be in
/// another region entirely, and a batch of secondaries has to be routed by what it touches. A
/// request with no keys at all takes the empty range, which matches no region — the honest
/// answer, rather than the whole key space.
#[must_use]
pub fn txn_request_range(request: &esker_proto::TxnKvReq) -> (Bytes, Bytes) {
    use esker_proto::TxnKvReq;

    fn successor(key: &[u8]) -> Bytes {
        let mut out = Vec::with_capacity(key.len() + 1);
        out.extend_from_slice(key);
        out.push(0);
        Bytes::from(out)
    }

    fn span<'a>(keys: impl Iterator<Item = &'a [u8]>) -> (Bytes, Bytes) {
        let mut low: Option<&[u8]> = None;
        let mut high: Option<&[u8]> = None;
        for key in keys {
            if low.is_none_or(|current| key < current) {
                low = Some(key);
            }
            if high.is_none_or(|current| key > current) {
                high = Some(key);
            }
        }
        match (low, high) {
            (Some(low), Some(high)) => (Bytes::copy_from_slice(low), successor(high)),
            // A batch with no keys touches nothing, and an empty range matches no region — which
            // is the honest answer rather than the whole key space.
            _ => (Bytes::new(), Bytes::new()),
        }
    }

    match request {
        // One key each: a read of its value, and a read of its newest commit.
        TxnKvReq::Get { key, .. } | TxnKvReq::LatestCommit { key } => {
            (Bytes::copy_from_slice(key), successor(key))
        }
        // The span each names. A scan reads it; a reclaim clears it. Either way the epoch is
        // checked against this range and the store serves only its own share of it
        // (`CLAUDE.md` invariant 5, ADR 0069).
        TxnKvReq::Scan { start, end, .. } | TxnKvReq::ReclaimRange { start, end, .. } => {
            (start.clone(), end.clone())
        }
        TxnKvReq::Prewrite { mutations, .. } => {
            span(mutations.iter().map(|mutation| &mutation.key()[..]))
        }
        TxnKvReq::Commit { keys, .. }
        | TxnKvReq::Rollback { keys, .. }
        | TxnKvReq::ResolveLock { keys, .. } => span(keys.iter().map(|key| &key[..])),
        TxnKvReq::Heartbeat { primary, .. } => {
            (Bytes::copy_from_slice(primary), successor(primary))
        }

        // Store-local: it is addressed to a store rather than to a range.
        TxnKvReq::GcSafepoint { .. } => (Bytes::new(), Bytes::new()),
    }
}

/// The key range a request touches, as `[start, end)` with an **empty `end` meaning the end of
/// the key space** — the convention every region comparison uses.
///
/// This is what turns a refusal into a useful one: an `EpochNotMatch` answers with every local
/// region overlapping *this* range, so a client whose cached region has split learns about both
/// halves from one round trip ([`crate::regions::RegionMap::route`]).
///
/// A single key becomes `[key, key ++ 0x00)` rather than `[key, key]`, because an empty upper
/// bound is `+∞` here: `[b"", b"")` would be the whole key space, which is the opposite of what a
/// `Get` of the empty key asks for. An empty batch has no keys to bound, so it takes the start of
/// the key space, which is where the client's own `routing_key` sends it too.
#[must_use]
pub fn request_range(request: &esker_proto::RawKvReq) -> (Bytes, Bytes) {
    use esker_proto::RawKvReq;

    /// The smallest key strictly greater than `key`. Appending a zero byte always works and never
    /// overflows, which is why the exclusive end of a point range is written this way.
    fn successor(key: &[u8]) -> Bytes {
        let mut out = Vec::with_capacity(key.len() + 1);
        out.extend_from_slice(key);
        out.push(0);
        Bytes::from(out)
    }

    fn point(key: &[u8]) -> (Bytes, Bytes) {
        (Bytes::copy_from_slice(key), successor(key))
    }

    fn span<'a>(keys: impl Iterator<Item = &'a [u8]>) -> (Bytes, Bytes) {
        let mut low: Option<&[u8]> = None;
        let mut high: Option<&[u8]> = None;
        for key in keys {
            if low.is_none_or(|seen| key < seen) {
                low = Some(key);
            }
            if high.is_none_or(|seen| key > seen) {
                high = Some(key);
            }
        }
        match (low, high) {
            (Some(low), Some(high)) => (Bytes::copy_from_slice(low), successor(high)),
            _ => point(b""),
        }
    }

    match request {
        RawKvReq::Get { key }
        | RawKvReq::Put { key, .. }
        | RawKvReq::Delete { key, .. }
        | RawKvReq::CompareAndSwap { key, .. } => point(key),
        RawKvReq::BatchGet { keys } => span(keys.iter().map(|key| &key[..])),
        RawKvReq::BatchPut { pairs, .. } => span(pairs.iter().map(|(key, _)| &key[..])),
        RawKvReq::DeleteRange { start, end, .. } => (start.clone(), end.clone()),
        // A reverse scan names its *upper* bound first, so the range it touches is still
        // `[low, high)` once the two are put back in order.
        RawKvReq::Scan {
            start,
            end,
            reverse,
            ..
        } => {
            if *reverse {
                (end.clone(), start.clone())
            } else {
                (start.clone(), end.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RegionMeta, request_range};
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

    /// A point request must not become the whole key space. `[b"", b"")` is `+∞` in this
    /// convention, so the exclusive end of a single key is the key with a zero byte appended —
    /// and `Get(b"")` is the case that proves it, because it is the one where a naive
    /// `(key, key)` reads as "everything".
    #[test]
    fn a_point_request_covers_one_key_and_not_the_key_space() {
        assert_eq!(
            request_range(&esker_proto::RawKvReq::get(&b""[..])),
            (Bytes::new(), Bytes::from_static(b"\0"))
        );
        assert_eq!(
            request_range(&esker_proto::RawKvReq::put(&b"k"[..], &b"v"[..])),
            (Bytes::from_static(b"k"), Bytes::from_static(b"k\0"))
        );
        assert_eq!(
            request_range(&esker_proto::RawKvReq::delete(&b"\xff"[..])),
            (Bytes::from_static(b"\xff"), Bytes::from_static(b"\xff\0")),
            "the top of the key space has a successor too"
        );
    }

    /// A batch spans from its lowest key to just past its highest, whatever order it arrived in.
    #[test]
    fn a_batch_spans_its_keys() {
        let request = esker_proto::RawKvReq::BatchGet {
            keys: vec![
                Bytes::from_static(b"m"),
                Bytes::from_static(b"a"),
                Bytes::from_static(b"z"),
            ],
        };
        assert_eq!(
            request_range(&request),
            (Bytes::from_static(b"a"), Bytes::from_static(b"z\0"))
        );

        // An empty batch has no keys to bound and routes to the start of the key space, which is
        // where the client sends it too.
        assert_eq!(
            request_range(&esker_proto::RawKvReq::BatchGet { keys: vec![] }),
            (Bytes::new(), Bytes::from_static(b"\0"))
        );
    }

    /// A reverse scan names its upper bound first. Reporting the range in that order would make
    /// every overlap check on it read backwards.
    #[test]
    fn a_reverse_scan_reports_its_range_in_order() {
        let forward = esker_proto::RawKvReq::scan(&b"a"[..], &b"m"[..], 0);
        assert_eq!(
            request_range(&forward),
            (Bytes::from_static(b"a"), Bytes::from_static(b"m"))
        );

        let reverse = esker_proto::RawKvReq::Scan {
            start: Bytes::from_static(b"m"),
            end: Bytes::from_static(b"a"),
            limit: 0,
            reverse: true,
        };
        assert_eq!(
            request_range(&reverse),
            (Bytes::from_static(b"a"), Bytes::from_static(b"m"))
        );

        // An unbounded scan keeps its empty end, which is what makes it +infinity.
        assert_eq!(
            request_range(&esker_proto::RawKvReq::scan(&b""[..], &b""[..], 0)),
            (Bytes::new(), Bytes::new())
        );
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
