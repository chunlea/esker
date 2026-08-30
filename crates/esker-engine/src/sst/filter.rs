//! The bloom filter that lets `get` skip a table without touching a data block
//! (`docs/DESIGN.md` §4.5, 10 bits per key by default).
//!
//! # Block layout (format v1)
//!
//! ```text
//! +---------------------------+
//! | bit array, `bytes` bytes  |   bit i lives in byte i/8, at bit i%8 (LSB first)
//! +---------------------------+
//! | probes: u8                |   number of hash probes, k
//! +---------------------------+
//! ```
//!
//! `k = max(1, min(30, bits_per_key * 69 / 100))`, `LevelDB`'s approximation of the optimal
//! `ln 2 * bits_per_key`. The k probe positions come from double hashing:
//! `h = hash64(key)`, `delta = h.rotate_left(32)`, `h += delta` between probes. One hash of
//! the key, then k cheap steps.
//!
//! # The one unforgivable bug
//!
//! A bloom filter may say "maybe" about a key that is absent. It must **never** say "no"
//! about a key that is present, because `get` believes it. The way that happens in practice
//! is a mismatch between the bytes hashed at build time and at probe time: build over the
//! prefix, probe with the whole key, and every lookup of a present key misses. [`filter_key`]
//! is therefore the *only* place the choice is made, and both paths call it — see
//! [`whole_key_probe_against_a_prefix_filter_still_finds_keys`].
//!
//! [`whole_key_probe_against_a_prefix_filter_still_finds_keys`]:
//!     tests::whole_key_probe_against_a_prefix_filter_still_finds_keys

use std::sync::Arc;

use esker_base::hash::hash64;

use crate::error::{Error, Result};
use crate::options::PrefixExtractor;

/// Bits of filter per key, the `docs/DESIGN.md` §14 default. At this size the false-positive
/// rate is under 1%, and [`false_positive_rate_is_under_two_percent`] holds it there.
///
/// [`false_positive_rate_is_under_two_percent`]: tests::false_positive_rate_is_under_two_percent
pub const DEFAULT_BITS_PER_KEY: usize = 10;

/// Smallest bit array we will build. Below this a handful of keys collide so often that the
/// filter costs a read instead of saving one.
const MIN_BITS: usize = 64;

/// Probe counts outside `1..=MAX_PROBES` mean a filter this build cannot interpret. Rather
/// than reject the table, [`BloomFilter::may_contain`] answers "maybe" to every probe, which
/// is always sound: it costs a block read, never a lost key.
const MAX_PROBES: u8 = 30;

/// Chooses the bytes a filter hashes for `key`, or `None` when this key must not be filtered.
///
/// With no extractor the whole key is hashed. With one, the extracted prefix is hashed — but
/// only for keys the extractor claims (`in_domain`); a key outside the domain contributes
/// nothing to the filter, so probing for it has to return "maybe" rather than consult a
/// filter that never saw it. Build and probe both route through here so they cannot disagree.
#[must_use]
pub fn filter_key<'k>(extractor: Option<&dyn PrefixExtractor>, key: &'k [u8]) -> Option<&'k [u8]> {
    match extractor {
        None => Some(key),
        Some(extractor) if extractor.in_domain(key) => Some(extractor.prefix(key)),
        Some(_) => None,
    }
}

/// Accumulates one hash per key, then paints the bit array in [`BloomBuilder::finish`].
///
/// Only the 8-byte hashes are retained, not the keys, so a filter over a million keys costs
/// 8 MiB of scratch regardless of how long the keys are.
#[derive(Debug, Clone)]
pub struct BloomBuilder {
    bits_per_key: usize,
    hashes: Vec<u64>,
}

impl BloomBuilder {
    /// A builder sized at `bits_per_key` bits per distinct added key.
    #[must_use]
    pub fn new(bits_per_key: usize) -> Self {
        Self {
            bits_per_key,
            hashes: Vec::new(),
        }
    }

    /// Adds one already-[`filter_key`]-mapped key. Adding the same bytes twice is harmless
    /// beyond over-sizing the array slightly, which is what happens with a prefix extractor
    /// and many keys per prefix.
    pub fn add(&mut self, key: &[u8]) {
        self.hashes.push(hash64(key));
    }

    /// Keys added so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// True when no key has been added, so the caller can skip writing a filter block.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// The number of probes a filter of this density uses.
    #[must_use]
    pub fn probes(&self) -> u8 {
        probes_for(self.bits_per_key)
    }

    /// Paints the bit array and returns the filter block payload described in the module docs.
    #[must_use]
    pub fn finish(&self) -> Vec<u8> {
        let probes = self.probes();
        let bits = (self.hashes.len().saturating_mul(self.bits_per_key)).max(MIN_BITS);
        let bytes = bits.div_ceil(8);
        let bits = bytes * 8;

        let mut out = vec![0u8; bytes + 1];
        for &hash in &self.hashes {
            paint(&mut out[..bytes], hash, probes, bits);
        }
        out[bytes] = probes;
        out
    }
}

/// `k = ln 2 * bits_per_key`, rounded the way `LevelDB` rounds it and clamped to a range this
/// format can express.
fn probes_for(bits_per_key: usize) -> u8 {
    let k = (bits_per_key * 69) / 100;
    u8::try_from(k).unwrap_or(MAX_PROBES).clamp(1, MAX_PROBES)
}

/// Sets the `probes` bits that `hash` selects out of `bits`. Shared by build and probe so the
/// two can never walk different positions.
fn each_probe(hash: u64, probes: u8, bits: usize, mut visit: impl FnMut(usize) -> bool) -> bool {
    debug_assert!(bits > 0, "caller must reject a zero-length bit array");
    let delta = hash.rotate_left(32);
    let mut h = hash;
    let modulus = bits as u64;
    for _ in 0..probes {
        // `bits` came from a slice length, so the remainder is always in range.
        let position = usize::try_from(h % modulus).unwrap_or(0);
        if !visit(position) {
            return false;
        }
        h = h.wrapping_add(delta);
    }
    true
}

fn paint(array: &mut [u8], hash: u64, probes: u8, bits: usize) {
    each_probe(hash, probes, bits, |position| {
        array[position / 8] |= 1 << (position % 8);
        true
    });
}

/// A filter block, parsed once when the table opens and probed per `get`.
///
/// Holds the block bytes as an [`Arc`] so the filter can outlive the read that produced it
/// without copying.
#[derive(Debug, Clone)]
pub struct BloomFilter {
    /// The whole block: bit array followed by the probe count.
    data: Arc<[u8]>,
    probes: u8,
    /// `(data.len() - 1) * 8`, cached so a probe does no arithmetic on the hot path.
    bits: usize,
}

impl BloomFilter {
    /// Reads the trailing probe count and validates that a bit array is present.
    ///
    /// The block's checksum has already been verified by the reader; this catches the shapes
    /// a valid checksum cannot rule out, such as a zero-length block.
    pub fn parse(data: Arc<[u8]>) -> Result<Self> {
        let Some((&probes, array)) = data.split_last() else {
            return Err(Error::corruption(
                "sst filter block",
                "block is empty; expected at least a probe-count byte",
            ));
        };
        Ok(Self {
            bits: array.len() * 8,
            data,
            probes,
        })
    }

    /// Probes for a key that has already been through [`filter_key`].
    ///
    /// `false` is a promise the key is absent; `true` means "read the block and see". A
    /// filter this build cannot interpret — no bits, or a probe count outside the range it
    /// writes — degrades to answering `true`, never to a false negative.
    #[must_use]
    pub fn may_contain(&self, key: &[u8]) -> bool {
        if self.bits == 0 || self.probes == 0 || self.probes > MAX_PROBES {
            return true;
        }
        let array = &self.data[..self.data.len() - 1];
        each_probe(hash64(key), self.probes, self.bits, |position| {
            array[position / 8] & (1 << (position % 8)) != 0
        })
    }

    /// Probe count read from the block.
    #[must_use]
    pub fn probes(&self) -> u8 {
        self.probes
    }

    /// Bits in the array, excluding the trailing probe-count byte.
    #[must_use]
    pub fn bits(&self) -> usize {
        self.bits
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BloomBuilder, BloomFilter, DEFAULT_BITS_PER_KEY, MAX_PROBES, MIN_BITS, filter_key,
        probes_for,
    };
    use crate::options::PrefixExtractor;
    use esker_base::rng::Pcg32;
    use std::sync::Arc;

    /// The first `n` bytes of a key, in domain only when the key is at least that long —
    /// the shape `esker-txn` will use for `user_key ++ ts`.
    #[derive(Debug)]
    struct FixedPrefix(usize);

    impl PrefixExtractor for FixedPrefix {
        fn prefix<'a>(&self, key: &'a [u8]) -> &'a [u8] {
            &key[..self.0.min(key.len())]
        }
        fn in_domain(&self, key: &[u8]) -> bool {
            key.len() >= self.0
        }
        fn name(&self) -> &'static str {
            "test.FixedPrefix"
        }
    }

    fn build(bits_per_key: usize, keys: &[Vec<u8>]) -> BloomFilter {
        let mut builder = BloomBuilder::new(bits_per_key);
        for key in keys {
            builder.add(key);
        }
        BloomFilter::parse(Arc::from(builder.finish().into_boxed_slice()))
            .expect("a freshly built filter parses")
    }

    fn numbered_keys(count: usize, salt: u8) -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| {
                let mut key = format!("key-{i:08}").into_bytes();
                key.push(salt);
                key
            })
            .collect()
    }

    /// The probe count is part of the on-disk format; it must match `LevelDB`'s rounding.
    #[test]
    fn probe_count_follows_the_formula() {
        assert_eq!(probes_for(DEFAULT_BITS_PER_KEY), 6);
        assert_eq!(probes_for(0), 1, "never zero probes");
        assert_eq!(probes_for(1), 1);
        assert_eq!(probes_for(16), 11);
        assert_eq!(probes_for(100), MAX_PROBES, "clamped, not truncated");
        assert_eq!(probes_for(usize::MAX / 100), MAX_PROBES);
    }

    /// The property that makes the filter usable at all. A "no" from the filter about a key
    /// that was added would silently lose data.
    #[test]
    fn a_built_key_is_never_reported_absent() {
        for bits_per_key in [1usize, 2, 10, 16, 30] {
            let keys = numbered_keys(2_000, 0);
            let filter = build(bits_per_key, &keys);
            for key in &keys {
                assert!(
                    filter.may_contain(key),
                    "false negative at {bits_per_key} bits/key for {key:?}"
                );
            }
        }
    }

    /// The whole point of 10 bits/key: `docs/DESIGN.md` §4.5's budget, measured.
    #[test]
    fn false_positive_rate_is_under_two_percent() {
        let present = numbered_keys(10_000, b'a');
        let filter = build(DEFAULT_BITS_PER_KEY, &present);

        let absent = numbered_keys(10_000, b'z');
        let positives = absent.iter().filter(|k| filter.may_contain(k)).count();
        // Integer arithmetic rather than a ratio: `positives / total <= 0.02`.
        assert!(
            positives * 50 <= absent.len(),
            "false-positive rate is {positives} of {}, over the 2% budget",
            absent.len()
        );
    }

    /// A tiny filter must still be a filter: below the floor the array is padded, not shrunk
    /// to something that answers "maybe" to everything.
    #[test]
    fn a_small_filter_is_padded_to_the_floor() {
        let keys = numbered_keys(1, 0);
        let filter = build(DEFAULT_BITS_PER_KEY, &keys);
        assert_eq!(filter.bits(), MIN_BITS);
        assert!(filter.may_contain(&keys[0]));

        let absent = numbered_keys(64, b'q');
        let positives = absent.iter().filter(|k| filter.may_contain(k)).count();
        assert!(
            positives < 8,
            "{positives} of 64 absent keys passed a 1-key filter"
        );
    }

    /// An empty filter block still parses, and reports everything absent.
    #[test]
    fn an_empty_filter_reports_absence() {
        let filter = build(DEFAULT_BITS_PER_KEY, &[]);
        assert_eq!(filter.bits(), MIN_BITS);
        assert!(!filter.may_contain(b"anything"));
    }

    /// The filter block is on disk, so its bytes are frozen. This vector is the contract:
    /// changing the hash, the probe count or the bit order breaks it.
    #[test]
    fn golden_vector() {
        let keys: Vec<Vec<u8>> = [b"alpha".as_slice(), b"bravo", b"charlie"]
            .iter()
            .map(|k| k.to_vec())
            .collect();
        let bytes = {
            let mut builder = BloomBuilder::new(DEFAULT_BITS_PER_KEY);
            for key in &keys {
                builder.add(key);
            }
            builder.finish()
        };
        // 3 keys x 10 bits = 30 bits, padded up to the 64-bit floor: 8 bytes + probe count.
        assert_eq!(bytes.len(), 9);
        assert_eq!(bytes[8], 6, "probe count");
        assert_eq!(
            bytes,
            vec![0x05, 0xa4, 0xba, 0x50, 0x40, 0x01, 0x40, 0x01, 0x06],
            "the filter block layout changed; that is a format change (ADR + version bump)"
        );
    }

    /// Corrupt shapes a checksum cannot catch must degrade to "maybe", never to a panic and
    /// never to a false negative.
    #[test]
    fn undecodable_filters_answer_maybe() {
        assert!(BloomFilter::parse(Arc::from(Vec::new().into_boxed_slice())).is_err());

        // Probe count only, no bit array.
        let filter = BloomFilter::parse(Arc::from(vec![6u8].into_boxed_slice())).unwrap();
        assert_eq!(filter.bits(), 0);
        assert!(filter.may_contain(b"anything"));

        // A probe count this build never writes.
        for probes in [0u8, MAX_PROBES + 1, u8::MAX] {
            let filter = BloomFilter::parse(Arc::from(
                vec![0u8, 0, 0, 0, 0, 0, 0, 0, probes].into_boxed_slice(),
            ))
            .unwrap();
            assert!(
                filter.may_contain(b"anything"),
                "probes={probes} filtered against an uninterpretable array"
            );
        }
    }

    /// `filter_key` is the single decision point, and this is what it decides.
    #[test]
    fn filter_key_maps_build_and_probe_the_same_way() {
        let extractor = FixedPrefix(4);
        assert_eq!(filter_key(None, b"abcdefgh"), Some(b"abcdefgh".as_slice()));
        assert_eq!(
            filter_key(Some(&extractor), b"abcdefgh"),
            Some(b"abcd".as_slice())
        );
        assert_eq!(filter_key(Some(&extractor), b"ab"), None, "out of domain");
    }

    /// The unforgivable bug, pinned. A filter built over 4-byte prefixes, probed through the
    /// same `filter_key`, still finds every key that went into it — and a key too short for
    /// the extractor's domain is never filtered out, because it was never filtered in.
    #[test]
    fn whole_key_probe_against_a_prefix_filter_still_finds_keys() {
        let extractor = FixedPrefix(4);
        let mut builder = BloomBuilder::new(DEFAULT_BITS_PER_KEY);
        let keys: Vec<Vec<u8>> = (0..500u32)
            .map(|i| format!("k{i:03}-suffix-{i}").into_bytes())
            .collect();
        for key in &keys {
            if let Some(bytes) = filter_key(Some(&extractor), key) {
                builder.add(bytes);
            }
        }
        let filter = BloomFilter::parse(Arc::from(builder.finish().into_boxed_slice())).unwrap();

        // The correct probe finds every key. This is the guarantee.
        for key in &keys {
            let probe = filter_key(Some(&extractor), key);
            assert!(
                probe.is_some_and(|bytes| filter.may_contain(bytes)),
                "prefix-built filter lost {key:?}"
            );
        }

        // And the incorrect probe — the whole key against a prefix-built filter — loses
        // nearly all of them. That gap is the bug this test exists to keep out: it would
        // read as "key absent" for a key that is very much present.
        let found_by_whole_key = keys.iter().filter(|key| filter.may_contain(key)).count();
        assert!(
            found_by_whole_key * 20 < keys.len(),
            "{found_by_whole_key} of {} whole-key probes hit, so this test proves nothing \
             about the prefix path",
            keys.len()
        );

        // Out-of-domain keys were never added, so the filter must not be consulted for them.
        assert_eq!(filter_key(Some(&extractor), b"ab"), None);
    }

    /// Random keys, random lengths, arbitrary bytes: still no false negatives. This is the
    /// proptest of the module's one hard guarantee, run over a seeded generator so a failure
    /// replays exactly (`CLAUDE.md`: the only randomness is `Pcg32`).
    #[test]
    fn no_false_negatives_over_random_keys() {
        let mut rng = Pcg32::from_seed(0x5E5_5100D);
        for round in 0..64u32 {
            let count = usize::try_from(rng.below(300)).unwrap_or(0) + 1;
            let keys: Vec<Vec<u8>> = (0..count)
                .map(|_| {
                    let len = usize::try_from(rng.below(48)).unwrap_or(0);
                    let mut key = vec![0u8; len];
                    rng.fill_bytes(&mut key);
                    key
                })
                .collect();
            let bits_per_key = usize::try_from(rng.range_inclusive(1, 24)).unwrap_or(10);
            let filter = build(bits_per_key, &keys);
            for key in &keys {
                assert!(
                    filter.may_contain(key),
                    "round {round}: false negative for {key:?} at {bits_per_key} bits/key"
                );
            }
        }
    }
}
