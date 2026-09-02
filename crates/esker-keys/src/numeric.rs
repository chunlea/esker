//! The representation of a `numeric`, and nothing about what it means.
//!
//! An arbitrary-precision decimal, held the way PostgreSQL holds one: a sign, a string of digits,
//! and a **scale** saying where the point goes. The scale is part of the value and not a display
//! choice — `1.0`, `1.00` and `1.000` are three different values of this type that all compare
//! equal, which is the property everything else about `numeric` follows from.
//!
//! **Printing lives here and reading does not**, which is a seam rather than an oversight:
//! rendering digits at a scale is a property of the representation and has one answer, where
//! *reading* a `numeric` is PostgreSQL's `numeric_in` with its exponents, its three words and its
//! errors — and comparison, rounding and the typmod are its rules too. Those are
//! `esker_sql::value::numeric`'s. This half is here because `esker-store` needs it and cannot see
//! that crate: a columnar fragment carries a `numeric` as its text.

/// An arbitrary-precision decimal.
///
/// `Finite` is `sign × digits × 10^-scale`, so the *same number* has many representations and the
/// one that was written is the one that is kept. A negative scale is legal and is how
/// `numeric(10,-2)` stores `12300` as three digits and a scale of `-2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Numeric {
    /// `'NaN'`. **A value, not an absence**, and unlike IEEE's it equals itself and sorts above
    /// every finite number — which is why it is a variant here rather than a bit pattern.
    NaN,
    /// `'Infinity'`.
    PosInfinity,
    /// `'-Infinity'`.
    NegInfinity,
    /// Everything else.
    Finite(Decimal),
}

/// A finite decimal: `sign × digits × 10^-scale`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decimal {
    /// The sign. **Zero is never negative**: `-0.0::numeric` is `0.0` on a real server, so nothing
    /// constructs a negative zero and the comparison needs no case for one.
    pub negative: bool,
    /// The unscaled value's digits, most significant first, each `0..=9`. At least one, and with
    /// no leading zero unless the whole number is a single `0`.
    pub digits: Vec<u8>,
    /// How many of those digits are after the point. May be **negative**, which multiplies:
    /// `numeric(10,-2)` holding `12300` is digits `123` and scale `-2`.
    pub scale: i32,
}

impl Decimal {
    /// Zero at scale zero — the value a `0` literal has before a typmod touches it.
    #[must_use]
    pub fn zero() -> Self {
        Decimal {
            negative: false,
            digits: vec![0],
            scale: 0,
        }
    }

    /// Whether every digit is zero, which is the one value whose sign is not recorded.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.digits.iter().all(|digit| *digit == 0)
    }

    /// The value with trailing zeros removed and the scale reduced to match — **past zero if the
    /// zeros are integral**, because a negative scale is how this type spells `1230` as `123`.
    ///
    /// **Not** what is stored — the trailing zeros are the declared scale and printing them is the
    /// whole point of the type. This is for the two places where two spellings of one number have
    /// to become one thing: an index key, where `1.0` and `1.00` must encode identically because
    /// they compare equal, and a comparison, which is defined on the number rather than the text.
    ///
    /// The stripping loop stopped at scale zero once, which met that contract for `1.0` and broke
    /// it for `1230`: digits `1230` at scale 0 and digits `123` at scale -1 are one value and got
    /// two index keys, so a unique index would admit both and a lookup by one spelling would miss
    /// a row stored under the other. `row::tests::a_numeric_key_sorts_the_way_the_number_does`
    /// is that pair.
    #[must_use]
    pub fn normalised(&self) -> Decimal {
        let mut digits = self.digits.clone();
        let mut scale = self.scale;
        while digits.last() == Some(&0) && digits.len() > 1 {
            digits.pop();
            scale -= 1;
        }
        // A zero of any scale is one value, and leading zeros are not digits.
        while digits.len() > 1 && digits[0] == 0 {
            digits.remove(0);
        }
        if digits == [0] {
            return Decimal::zero();
        }
        Decimal {
            negative: self.negative,
            digits,
            scale,
        }
    }

    /// The power of ten the leading digit stands for, for a value that is not zero.
    ///
    /// `123.45` normalises to digits `12345` at scale 2, so its leading `1` is `10^2` — which is
    /// what orders two decimals before their digits are compared at all.
    #[must_use]
    pub fn exponent(&self) -> i64 {
        i64::try_from(self.digits.len()).unwrap_or(i64::MAX) - 1 - i64::from(self.scale)
    }
}

/// What a decimal reads as: the digits with the point put back where the scale says.
///
/// A **negative** scale prints the trailing zeros it stands for — `numeric(10,-2)` holding three
/// digits and a scale of `-2` prints `12300`, not `123` — and a value that is all zeros prints
/// without a sign, because zero is never negative here.
#[must_use]
pub fn to_text(value: &Numeric) -> String {
    let decimal = match value {
        Numeric::NaN => return "NaN".to_owned(),
        Numeric::PosInfinity => return "Infinity".to_owned(),
        Numeric::NegInfinity => return "-Infinity".to_owned(),
        Numeric::Finite(decimal) => decimal,
    };
    let digits: String = decimal
        .digits
        .iter()
        .map(|digit| char::from(b'0' + digit))
        .collect();
    let sign = if decimal.negative && !decimal.is_zero() {
        "-"
    } else {
        ""
    };
    match decimal.scale {
        scale if scale <= 0 => {
            let zeros = "0".repeat(usize::try_from(-scale).unwrap_or(0));
            format!("{sign}{digits}{zeros}")
        }
        scale => {
            let scale = usize::try_from(scale).unwrap_or(0);
            if digits.len() > scale {
                let at = digits.len() - scale;
                format!("{sign}{}.{}", &digits[..at], &digits[at..])
            } else {
                let pad = "0".repeat(scale - digits.len());
                format!("{sign}0.{pad}{digits}")
            }
        }
    }
}
