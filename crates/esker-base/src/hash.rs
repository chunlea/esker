//! A 64-bit hash for in-memory keys: block-cache lookups, bloom filters, shard selection.
//!
//! This is deliberately not a cryptographic hash and deliberately not a crate. It is FNV-1a
//! — one multiply and one xor per byte, published constants, easy to verify against known
//! vectors — followed by the `SplitMix64` finalizer, because FNV-1a on its own has weak
//! avalanche in the low bits and those are exactly the bits a power-of-two shard mask uses.
//!
//! Nothing here is written to disk. It may be replaced by something faster once a profile
//! says it matters, and no format has to change when it is.

/// FNV-1a 64-bit offset basis.
pub const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;

/// FNV-1a 64-bit prime.
pub const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// FNV-1a over `data`, exactly as published.
#[must_use]
pub fn fnv1a64(data: &[u8]) -> u64 {
    fnv1a64_from(FNV_OFFSET_BASIS, data)
}

/// FNV-1a continued from an existing state, so a hash can be built in pieces.
#[must_use]
pub fn fnv1a64_from(state: u64, data: &[u8]) -> u64 {
    let mut hash = state;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The `SplitMix64` finalizer: a bijection that spreads every input bit over all 64 output bits.
#[must_use]
pub fn mix64(value: u64) -> u64 {
    let mut x = value;
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    x
}

/// The project's general-purpose 64-bit hash.
#[must_use]
pub fn hash64(data: &[u8]) -> u64 {
    mix64(fnv1a64(data))
}

/// [`hash64`] with a seed, for the several independent hashes a bloom filter needs.
#[must_use]
pub fn hash64_with_seed(seed: u64, data: &[u8]) -> u64 {
    mix64(fnv1a64_from(FNV_OFFSET_BASIS ^ seed, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Pcg32;

    /// Published FNV-1a 64 vectors. The finalizer is applied on top of these, so pinning the
    /// FNV layer against outside values is what makes the whole function verifiable.
    #[test]
    fn fnv1a_matches_published_vectors() {
        assert_eq!(fnv1a64(b""), 0xCBF2_9CE4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xAF63_DC4C_8601_EC8C);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_F739_67E8);
    }

    /// Frozen so a silent change to the hash shows up as a failing test rather than as a
    /// cache that quietly stops hitting.
    #[test]
    fn hash64_is_frozen() {
        assert_eq!(hash64(b""), 0xF52A_15E9_A9B5_E89B);
        assert_eq!(hash64(b"a"), 0x02C0_BDBF_4814_20F8);
        assert_eq!(hash64(b"foobar"), 0x404D_A9E3_B740_78C2);
        assert_eq!(hash64(b"esker"), 0xF342_17EB_D726_542B);
    }

    #[test]
    fn incremental_equals_one_shot() {
        let data = b"column families share one write-ahead log";
        for split in 0..=data.len() {
            let (head, tail) = data.split_at(split);
            assert_eq!(
                fnv1a64_from(fnv1a64(head), tail),
                fnv1a64(data),
                "split {split}"
            );
        }
    }

    #[test]
    fn seeds_produce_independent_hashes() {
        let key = b"region-1";
        let hashes: Vec<u64> = (0..8).map(|seed| hash64_with_seed(seed, key)).collect();
        for (i, a) in hashes.iter().enumerate() {
            for b in &hashes[i + 1..] {
                assert_ne!(a, b, "two seeds collided on the same key");
            }
        }
    }

    /// The low bits are what a shard mask reads, so they must not be degenerate. Without the
    /// finalizer, FNV-1a's low bits for sequential keys are heavily skewed.
    #[test]
    fn low_bits_are_well_distributed() {
        const SHARDS: usize = 8;
        const KEYS: usize = 8192;

        let mut counts = [0usize; SHARDS];
        for i in 0..KEYS {
            let key = format!("key-{i}");
            let shard = usize::try_from(hash64(key.as_bytes()) % SHARDS as u64).unwrap_or(0);
            counts[shard] += 1;
        }
        let expected = KEYS / SHARDS;
        for (shard, &count) in counts.iter().enumerate() {
            let deviation = count.abs_diff(expected);
            assert!(
                deviation * 10 < expected,
                "shard {shard} got {count} of {KEYS} keys, expected about {expected}"
            );
        }
    }

    #[test]
    fn mix64_is_a_bijection_on_sampled_inputs() {
        let mut rng = Pcg32::new(99, 3);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..4096 {
            assert!(seen.insert(mix64(rng.next_u64())), "mix64 collided");
        }
    }
}
