//! `tsvector` — a sorted, deduplicated set of lexemes, and the text it prints as.
//!
//! [ADR 0066](../../../docs/adr/0066-a-tsvector-is-its-canonical-text.md): the stored bytes are the
//! printed form, canonicalised on the way in, so equality, ordering, grouping and an index over the
//! column are the text machinery's. The road `hstore` takes, and the whole of the risk is here —
//! a node that stored the user's characters unchanged would round-trip `full_text_test.rb` and
//! disagree with a real server the first time a value arrived unsorted.
//!
//! **A literal is not what `to_tsvector` produces.** Measured in `captures/pg19_tsvector.txt`:
//!
//! ```text
//! 'a fat cat'::tsvector                          -> 'a' 'cat' 'fat'
//! to_tsvector('english', 'The Fat Cats ate a rat') -> 'ate':4 'cat':3 'fat':2 'rat':6
//! ```
//!
//! The literal is sorted and quoted and **nothing else** — no stemming, no stop-word removal, no
//! positions. Only the function normalises, and that is a different entry point.
//!
//! Lexemes sort by **bytes, not by length first**, which the capture settles rather than assumes:
//! `to_tsvector('simple', 'The Fat Cats ate a rat')` is `'a':5 'ate':4 'cats':3 'fat':2 'rat':6
//! 'the':1`, and `cats` before `fat` is only true of plain lexicographic order.

use crate::error::{Result, SqlError};

/// A position's weight. `D` is the default and is **not printed**, which is why the capture shows
/// `'cat':3` and `'cat':2A` but never `'cat':3D`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Weight {
    /// Printed `A`.
    A,
    /// Printed `B`.
    B,
    /// Printed `C`.
    C,
    /// The default, printed as nothing.
    D,
}

impl Weight {
    fn suffix(self) -> &'static str {
        match self {
            Weight::A => "A",
            Weight::B => "B",
            Weight::C => "C",
            Weight::D => "",
        }
    }

    fn of(letter: char) -> Option<Self> {
        match letter {
            'A' | 'a' => Some(Weight::A),
            'B' | 'b' => Some(Weight::B),
            'C' | 'c' => Some(Weight::C),
            'D' | 'd' => Some(Weight::D),
            _ => None,
        }
    }
}

/// One lexeme and the positions it was seen at, in ascending order with duplicates merged.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Lexeme {
    /// The word itself, exactly as written — a literal is not folded or stemmed.
    pub word: String,
    /// `(position, weight)`, ascending and deduplicated. Empty when the value carries no positions.
    pub positions: Vec<(u16, Weight)>,
}

/// Parses a `tsvector` literal into its canonical lexeme set.
///
/// **The whitespace-separated words are the lexemes**; a word may be bare (`cat`) or single-quoted
/// (`'cat'`, with `''` for an interior quote), and may carry `:1,2A` positions. Sorting,
/// deduplicating and merging positions is what makes this canonical, and it is why two values that
/// print alike are one value.
pub fn from_text(text: &str) -> Result<Vec<Lexeme>> {
    let chars: Vec<char> = text.chars().collect();
    let mut at = 0usize;
    let mut out: Vec<Lexeme> = Vec::new();
    loop {
        while at < chars.len() && chars[at].is_whitespace() {
            at += 1;
        }
        if at >= chars.len() {
            break;
        }
        let word = word_at(&chars, &mut at)?;
        let positions = positions_at(&chars, &mut at)?;
        out.push(Lexeme { word, positions });
    }
    Ok(canonical(out))
}

/// Sorts by lexeme, merges duplicates, and merges each lexeme's positions.
///
/// **Two lexemes that are equal are one lexeme**, and their positions are the union: PG answers
/// `'run':1,2` for `to_tsvector('english', 'running runs ran')`, where `running` and `runs` stem to
/// the same word at two positions.
fn canonical(mut lexemes: Vec<Lexeme>) -> Vec<Lexeme> {
    lexemes.sort_by(|a, b| a.word.cmp(&b.word));
    let mut out: Vec<Lexeme> = Vec::with_capacity(lexemes.len());
    for lexeme in lexemes {
        match out.last_mut() {
            Some(last) if last.word == lexeme.word => last.positions.extend(lexeme.positions),
            _ => out.push(lexeme),
        }
    }
    for lexeme in &mut out {
        lexeme.positions.sort_unstable();
        lexeme.positions.dedup_by_key(|(at, _)| *at);
    }
    out
}

/// The canonical text, which is what is stored and what a client reads back.
pub fn to_text(lexemes: &[Lexeme]) -> String {
    let mut out = String::new();
    for lexeme in lexemes {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&quote(&lexeme.word));
        for (n, (at, weight)) in lexeme.positions.iter().enumerate() {
            out.push(if n == 0 { ':' } else { ',' });
            out.push_str(&at.to_string());
            out.push_str(weight.suffix());
        }
    }
    out
}

/// **Every lexeme is quoted on the way out**, which the capture shows for the plainest possible
/// input: `'a b'::tsvector` prints `'a' 'b'`, not `a b`.
fn quote(word: &str) -> String {
    let mut out = String::with_capacity(word.len() + 2);
    out.push('\'');
    for c in word.chars() {
        if c == '\'' {
            out.push('\'');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

fn word_at(chars: &[char], at: &mut usize) -> Result<String> {
    let mut word = String::new();
    if chars[*at] == '\'' {
        *at += 1;
        loop {
            let Some(c) = chars.get(*at).copied() else {
                return Err(syntax(chars));
            };
            *at += 1;
            if c == '\'' {
                // A doubled quote is one quote and the lexeme goes on.
                if chars.get(*at) == Some(&'\'') {
                    word.push('\'');
                    *at += 1;
                    continue;
                }
                break;
            }
            word.push(c);
        }
    } else {
        while let Some(&c) = chars.get(*at) {
            if c.is_whitespace() || c == ':' {
                break;
            }
            word.push(c);
            *at += 1;
        }
    }
    if word.is_empty() {
        return Err(syntax(chars));
    }
    Ok(word)
}

fn positions_at(chars: &[char], at: &mut usize) -> Result<Vec<(u16, Weight)>> {
    if chars.get(*at) != Some(&':') {
        return Ok(Vec::new());
    }
    *at += 1;
    let mut out = Vec::new();
    loop {
        let mut digits = String::new();
        while let Some(&c) = chars.get(*at) {
            if !c.is_ascii_digit() {
                break;
            }
            digits.push(c);
            *at += 1;
        }
        let position: u16 = digits.parse().map_err(|_| syntax(chars))?;
        let weight = match chars.get(*at).copied().and_then(Weight::of) {
            Some(weight) => {
                *at += 1;
                weight
            }
            None => Weight::D,
        };
        out.push((position, weight));
        if chars.get(*at) == Some(&',') {
            *at += 1;
            continue;
        }
        break;
    }
    Ok(out)
}

fn syntax(chars: &[char]) -> SqlError {
    SqlError::InvalidTextRepresentation {
        ty: "tsvector",
        value: chars.iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation here is a row of `captures/pg19_tsvector.txt`, not a rule reasoned about.
    fn canon(text: &str) -> String {
        to_text(&from_text(text).expect("a valid tsvector"))
    }

    #[test]
    fn a_literal_is_sorted_and_quoted_and_nothing_else() {
        // `'a fat cat'::tsvector` -> `'a' 'cat' 'fat'`: sorted, quoted, and **not** stemmed, with
        // the stop word kept. That last part is what separates a literal from `to_tsvector`.
        assert_eq!(canon("a fat cat"), "'a' 'cat' 'fat'");
        assert_eq!(canon("'text' 'vector'"), "'text' 'vector'");
        assert_eq!(canon("text vector"), "'text' 'vector'");
        assert_eq!(canon("a b"), "'a' 'b'");
        assert_eq!(canon(""), "");
    }

    /// **Bytes, not length first.** `to_tsvector('simple', 'The Fat Cats ate a rat')` puts `cats`
    /// before `fat`, which is only true of plain lexicographic order — and `hstore`, the module
    /// this one is modelled on, sorts length-first, so the comparator could not be copied.
    #[test]
    fn lexemes_sort_by_bytes_and_not_by_length() {
        assert_eq!(
            canon("the fat cats ate a rat"),
            "'a' 'ate' 'cats' 'fat' 'rat' 'the'"
        );
    }

    /// Two occurrences of one lexeme are one lexeme with two positions, which is how
    /// `to_tsvector('english', 'running runs ran')` reaches `'run':1,2`.
    #[test]
    fn duplicate_lexemes_merge_and_keep_both_positions() {
        assert_eq!(canon("'run':1 'run':2"), "'run':1,2");
        assert_eq!(canon("'run':2 'run':1"), "'run':1,2");
        // The same position twice is one position.
        assert_eq!(canon("'run':1 'run':1"), "'run':1");
    }

    /// `D` is the default weight and is never printed; `A` is.
    #[test]
    fn a_weight_prints_only_when_it_is_not_the_default() {
        // Sorted by lexeme, so a weight travels with its own word rather than with its position:
        // `setweight(to_tsvector('english', 'fat cat'), 'A')` is `'cat':2A 'fat':1A`.
        assert_eq!(canon("'fat':1A 'cat':2A"), "'cat':2A 'fat':1A");
        assert_eq!(canon("'cat':3D"), "'cat':3");
    }

    /// A quote inside a lexeme is doubled on the way in and on the way out.
    #[test]
    fn an_interior_quote_survives_the_round_trip() {
        assert_eq!(canon("'it''s'"), "'it''s'");
    }
}
