//! The per-type encodings, and the tag byte that says which one a chunk used.
//!
//! Every encoding here is a pair of pure functions over a slice of values and a `Vec<u8>` — no
//! file, no chunk framing, no compression. That is what lets each one carry its own round-trip
//! proptest next to it, which `CLAUDE.md` requires of a format and this crate has one of per
//! encoding.
//!
//! Which encoding a chunk gets is decided by *encoding it both ways and keeping the smaller*.
//! That sounds wasteful and is not: the candidates are a handful of passes over data already in
//! cache, and the alternative is a heuristic — "dictionary when cardinality is below a third" —
//! that is a magic number somebody tunes once against one workload and nobody revisits. The
//! decoder does not care how the choice was made; it reads the tag.

/// Which encoding a column chunk's values are stored under.
///
/// The tag is on disk, so these numbers are frozen. A tag no version has written is corruption,
/// never a value to skip past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Values as they are: fixed width for numbers, bit-packed lengths plus bytes for strings.
    Plain,
    /// Frame of reference: one minimum, then bit-packed offsets from it.
    FrameOfReference,
    /// Zigzag deltas between neighbours, themselves frame-of-reference packed.
    Delta,
    /// A dictionary of distinct values, then bit-packed codes into it.
    Dictionary,
    /// One bit per boolean.
    Bitpacked,
    /// Alternating runs of equal booleans.
    Rle,
}

impl Encoding {
    /// The byte written into the footer's chunk entry. Frozen.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            Encoding::Plain => 1,
            Encoding::FrameOfReference => 2,
            Encoding::Delta => 3,
            Encoding::Dictionary => 4,
            Encoding::Bitpacked => 5,
            Encoding::Rle => 6,
        }
    }

    /// The encoding a tag byte names, or `None` for one no version has written.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Encoding::Plain),
            2 => Some(Encoding::FrameOfReference),
            3 => Some(Encoding::Delta),
            4 => Some(Encoding::Dictionary),
            5 => Some(Encoding::Bitpacked),
            6 => Some(Encoding::Rle),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Encoding;

    const ALL: [Encoding; 6] = [
        Encoding::Plain,
        Encoding::FrameOfReference,
        Encoding::Delta,
        Encoding::Dictionary,
        Encoding::Bitpacked,
        Encoding::Rle,
    ];

    /// The tags are in every footer this format has ever written.
    #[test]
    fn encoding_tags_are_frozen() {
        assert_eq!(Encoding::Plain.as_u8(), 1);
        assert_eq!(Encoding::FrameOfReference.as_u8(), 2);
        assert_eq!(Encoding::Delta.as_u8(), 3);
        assert_eq!(Encoding::Dictionary.as_u8(), 4);
        assert_eq!(Encoding::Bitpacked.as_u8(), 5);
        assert_eq!(Encoding::Rle.as_u8(), 6);

        for encoding in ALL {
            assert_eq!(Encoding::from_u8(encoding.as_u8()), Some(encoding));
        }
        assert_eq!(Encoding::from_u8(0), None);
        assert_eq!(Encoding::from_u8(7), None);
    }
}
