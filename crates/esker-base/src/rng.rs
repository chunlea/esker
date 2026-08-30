//! PCG32 — the only source of randomness in Esker.
//!
//! `docs/DESIGN.md` §5 and §11 make determinism a correctness property, not a convenience:
//! the Raft core, the simulator and the model checker are only useful if every run is a
//! function of its seed. So no component may call OS entropy or a thread-local generator;
//! it takes a [`Pcg32`] and is handed one by its caller. Every simulator failure prints its
//! seed, and that seed is enough to replay the failure exactly.
//!
//! The generator is O'Neill's PCG-XSH-RR 64/32: a 64-bit LCG whose output is an
//! xor-shift then a rotate driven by the top bits of the state. It is small, it has a
//! published reference stream to test against, and it is not `rand` (which the dependency
//! policy bans).

/// The LCG multiplier from the reference implementation. Part of the reproducible stream.
const MULTIPLIER: u64 = 6_364_136_223_846_793_005;

/// A seeded, reproducible random number generator.
///
/// Two `Pcg32`s built with the same `(seed, sequence)` produce the same stream forever, on
/// every platform. Different `sequence` values give streams that are independent, so
/// components in one simulation can each have their own without correlating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pcg32 {
    state: u64,
    /// Always odd — it is the LCG increment, and an even increment halves the period.
    increment: u64,
}

impl Pcg32 {
    /// Creates a generator from a seed and a stream selector, following the reference
    /// seeding procedure so the stream matches published test vectors.
    #[must_use]
    pub fn new(seed: u64, sequence: u64) -> Self {
        let mut rng = Self {
            state: 0,
            increment: (sequence << 1) | 1,
        };
        rng.step();
        rng.state = rng.state.wrapping_add(seed);
        rng.step();
        rng
    }

    /// Creates a generator on the default stream. Use this when there is only one generator.
    #[must_use]
    pub fn from_seed(seed: u64) -> Self {
        Self::new(seed, 0xDA3E_39CB_94B9_5BDB)
    }

    /// Advances the LCG and returns the previous state, which is what the output function
    /// is computed from.
    fn step(&mut self) -> u64 {
        let previous = self.state;
        self.state = previous
            .wrapping_mul(MULTIPLIER)
            .wrapping_add(self.increment);
        previous
    }

    /// The next 32 bits of the stream.
    pub fn next_u32(&mut self) -> u32 {
        let state = self.step();
        // XSH: fold the high bits down. RR: rotate by the top five bits of the state.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "both casts keep the low bits on purpose; that is the output function"
        )]
        {
            let xorshifted = (((state >> 18) ^ state) >> 27) as u32;
            let rotation = (state >> 59) as u32;
            xorshifted.rotate_right(rotation)
        }
    }

    /// The next 64 bits, as two draws. The high half is drawn first so the value is a
    /// function of the stream position only.
    pub fn next_u64(&mut self) -> u64 {
        let high = u64::from(self.next_u32());
        let low = u64::from(self.next_u32());
        (high << 32) | low
    }

    /// A uniform value in `0..bound`, without modulo bias. Returns `0` when `bound` is 0.
    ///
    /// The rejection threshold discards the short final block of the 32-bit range, which is
    /// what would otherwise make small values slightly more likely.
    pub fn below(&mut self, bound: u32) -> u32 {
        if bound <= 1 {
            return 0;
        }
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let draw = self.next_u32();
            if draw >= threshold {
                return draw % bound;
            }
        }
    }

    /// A uniform value in `low..=high`. Returns `low` when the range is empty or a point.
    pub fn range_inclusive(&mut self, low: u64, high: u64) -> u64 {
        if high <= low {
            return low;
        }
        let Some(count) = (high - low).checked_add(1) else {
            // `low == 0 && high == u64::MAX`: the whole range, so no reduction is needed.
            return self.next_u64();
        };
        if let Ok(narrow) = u32::try_from(count) {
            return low + u64::from(self.below(narrow));
        }
        // Wide ranges: reduce a 64-bit draw. The residual bias is below 2^-32 of the range.
        low + self.next_u64() % count
    }

    /// `true` with probability `probability`, clamped to `0.0..=1.0`.
    ///
    /// The comparison is done on integers so the result depends only on the stream, never on
    /// floating-point rounding at the decision point.
    pub fn chance(&mut self, probability: f64) -> bool {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "probability is clamped to 0.0..=1.0, so the product is in 0..=2^32-1"
        )]
        let threshold = (probability.clamp(0.0, 1.0) * f64::from(u32::MAX)) as u32;
        self.next_u32() < threshold
    }

    /// Fills `dst` with stream bytes.
    pub fn fill_bytes(&mut self, dst: &mut [u8]) {
        let mut chunks = dst.chunks_exact_mut(4);
        for chunk in &mut chunks {
            chunk.copy_from_slice(&self.next_u32().to_le_bytes());
        }
        let tail = chunks.into_remainder();
        if !tail.is_empty() {
            let bytes = self.next_u32().to_le_bytes();
            tail.copy_from_slice(&bytes[..tail.len()]);
        }
    }

    /// Shuffles `slice` in place (Fisher-Yates, drawing from the high end down).
    pub fn shuffle<T>(&mut self, slice: &mut [T]) {
        for i in (1..slice.len()).rev() {
            let Ok(bound) = u32::try_from(i + 1) else {
                // A slice longer than 4 billion elements is not something we shuffle.
                return;
            };
            slice.swap(i, self.below(bound) as usize);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference stream from the PCG demo program, seeded with `(42, 54)`. This is the
    /// test that proves the algorithm is PCG32 and not merely self-consistent.
    #[test]
    fn matches_the_reference_stream() {
        let mut rng = Pcg32::new(42, 54);
        let expected = [
            0xA15C_02B7,
            0x7B47_F409,
            0xBA1D_3330,
            0x83D2_F293,
            0xBFA4_784B,
            0xCBED_606E,
        ];
        for (index, want) in expected.into_iter().enumerate() {
            assert_eq!(rng.next_u32(), want, "draw {index}");
        }
    }

    #[test]
    fn same_seed_gives_the_same_stream() {
        let mut a = Pcg32::new(7, 1);
        let mut b = Pcg32::new(7, 1);
        for _ in 0..1000 {
            assert_eq!(a.next_u32(), b.next_u32());
        }
        assert_eq!(a, b);
    }

    #[test]
    fn different_streams_do_not_correlate() {
        let mut a = Pcg32::new(7, 1);
        let mut b = Pcg32::new(7, 2);
        let matches = (0..1000).filter(|_| a.next_u32() == b.next_u32()).count();
        assert!(
            matches < 5,
            "{matches} of 1000 draws matched across streams"
        );
    }

    #[test]
    fn increment_is_always_odd() {
        for sequence in [0u64, 1, 2, u64::MAX, u64::MAX / 2] {
            assert_eq!(Pcg32::new(0, sequence).increment & 1, 1);
        }
    }

    #[test]
    fn below_respects_its_bound_and_covers_it() {
        let mut rng = Pcg32::from_seed(3);
        assert_eq!(rng.below(0), 0);
        assert_eq!(rng.below(1), 0);

        let mut seen = [false; 6];
        for _ in 0..2000 {
            let value = rng.below(6);
            assert!(value < 6);
            seen[value as usize] = true;
        }
        assert!(
            seen.iter().all(|hit| *hit),
            "some values in 0..6 never came up"
        );
    }

    #[test]
    fn below_is_close_to_uniform() {
        const BUCKETS: u32 = 16;
        const DRAWS: usize = 64_000;

        let mut rng = Pcg32::from_seed(11);
        let mut counts = [0usize; BUCKETS as usize];
        for _ in 0..DRAWS {
            counts[rng.below(BUCKETS) as usize] += 1;
        }
        let expected = DRAWS / BUCKETS as usize;
        for (bucket, &count) in counts.iter().enumerate() {
            assert!(
                count.abs_diff(expected) * 10 < expected,
                "bucket {bucket} got {count}, expected about {expected}"
            );
        }
    }

    #[test]
    fn range_inclusive_stays_inside_its_bounds() {
        let mut rng = Pcg32::from_seed(5);
        for _ in 0..1000 {
            let value = rng.range_inclusive(10, 20);
            assert!((10..=20).contains(&value));
        }
        assert_eq!(rng.range_inclusive(4, 4), 4);
        assert_eq!(rng.range_inclusive(9, 4), 9);
        // The full 64-bit range is the case where `span + 1` would overflow.
        let mut distinct = std::collections::BTreeSet::new();
        for _ in 0..100 {
            distinct.insert(rng.range_inclusive(0, u64::MAX));
        }
        assert_eq!(
            distinct.len(),
            100,
            "the full range collapsed to a few values"
        );
        // A range wider than u32 but not the whole space takes the reducing path.
        for _ in 0..100 {
            let value = rng.range_inclusive(1, u64::MAX - 1);
            assert!((1..=u64::MAX - 1).contains(&value));
        }
    }

    #[test]
    fn chance_of_zero_and_one_are_absolute() {
        let mut rng = Pcg32::from_seed(13);
        for _ in 0..500 {
            assert!(!rng.chance(0.0));
            assert!(rng.chance(1.0));
            assert!(!rng.chance(-5.0));
            assert!(rng.chance(5.0));
        }
        let hits = (0..10_000).filter(|_| rng.chance(0.25)).count();
        assert!(
            (2200..2800).contains(&hits),
            "0.25 produced {hits} hits in 10000"
        );
    }

    #[test]
    fn fill_bytes_handles_every_tail_length() {
        for len in 0..17 {
            let mut a = vec![0u8; len];
            let mut b = vec![0u8; len];
            Pcg32::from_seed(1).fill_bytes(&mut a);
            Pcg32::from_seed(1).fill_bytes(&mut b);
            assert_eq!(a, b, "length {len} is not reproducible");
        }
        let mut buffer = [0u8; 64];
        Pcg32::from_seed(2).fill_bytes(&mut buffer);
        assert!(buffer.iter().any(|&byte| byte != 0));
    }

    #[test]
    fn shuffle_is_a_permutation_and_is_reproducible() {
        let original: Vec<u32> = (0..64).collect();

        let mut once = original.clone();
        Pcg32::from_seed(21).shuffle(&mut once);
        let mut twice = original.clone();
        Pcg32::from_seed(21).shuffle(&mut twice);

        assert_eq!(once, twice, "shuffle is not reproducible");
        assert_ne!(once, original, "shuffle left the slice untouched");

        let mut sorted = once.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, original, "shuffle lost or duplicated elements");

        // Degenerate inputs must not panic.
        Pcg32::from_seed(1).shuffle(&mut [] as &mut [u32]);
        Pcg32::from_seed(1).shuffle(&mut [1]);
    }
}
