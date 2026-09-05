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

    /// The weight a letter names, or `None` for anything that is not one — `setweight`'s second
    /// argument is a `"char"` and a real server refuses a letter outside `A`–`D`.
    #[must_use]
    pub fn of(letter: char) -> Option<Self> {
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

/// The text-search configurations this node has. `pg_ts_config` reports the same two.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Config {
    /// Lowercase, keep everything, stem nothing.
    Simple,
    /// Lowercase, drop the stop words, stem the rest.
    English,
}

impl Config {
    /// Resolves a configuration name, schema-qualified or not.
    ///
    /// **A name this node does not have is `42704`**, which is the sentence a real server gives an
    /// unknown one — measured as `to_tsvector('nosuchconfig', 'a')`. A name PostgreSQL *does* have
    /// and this node does not (`french`, say) takes the same answer, which is consistent with
    /// `pg_ts_config` reporting two rows rather than thirty-two, and is declared in the corpus.
    pub fn resolve(name: &str) -> Result<Self> {
        match name.strip_prefix("pg_catalog.").unwrap_or(name) {
            "simple" => Ok(Config::Simple),
            "english" => Ok(Config::English),
            other => Err(SqlError::UndefinedTextSearchConfig(other.to_owned())),
        }
    }
}

/// `to_tsvector(config, text)`.
///
/// **Positions are token indices, assigned before anything is dropped.** The capture is explicit:
/// `'The Fat Cats ate a rat'` is `'ate':4 'cat':3 'fat':2 'rat':6` under `english` — nothing at 1
/// or 5, because `the` and `a` were numbered and then removed. Numbering the survivors instead
/// would give `'ate':3 'cat':2 'fat':1 'rat':4`, which is a different value.
#[must_use]
pub fn to_tsvector(config: Config, text: &str) -> Vec<Lexeme> {
    canonical(to_lexemes(config, text))
}

/// The same pipeline **in token order**, which is what the query side needs.
///
/// `to_tsvector` sorts, because a tsvector is a set; a `tsquery` is an expression and its operands
/// keep the order they were written in — `plainto_tsquery('english', 'the fat cats')` is
/// `'fat' & 'cat'` and **not** `'cat' & 'fat'`. Sharing the sorted version between the two was a
/// real bug, caught by the captured answer.
#[must_use]
pub fn to_lexemes(config: Config, text: &str) -> Vec<Lexeme> {
    let mut lexemes = Vec::new();
    for (at, token) in tokens(text) {
        let folded = token.to_lowercase();
        let word = match config {
            Config::Simple => folded,
            // Measured: the list is consulted **before** the stemmer, so `only` is dropped rather
            // than stemmed to `onli`. `crate::value::stopwords` carries the probe that shows it.
            Config::English => {
                if crate::value::stopwords::is_stop_word(&folded) {
                    continue;
                }
                crate::value::stemmer::stem(&folded)
            }
        };
        if word.is_empty() {
            continue;
        }
        lexemes.push(Lexeme {
            word,
            positions: vec![(at, Weight::D)],
        });
    }
    lexemes
}

/// `a || b`: the two lexeme sets, with **`b`'s positions shifted by `a`'s maximum**.
///
/// Measured both ways round, and they differ — which is the whole reason a tsvector needed a
/// `Datum` of its own rather than riding on `Text`:
///
/// ```text
/// to_tsvector('english','fat cat') || to_tsvector('english','thin dog') -> 'cat':2 'dog':4 'fat':1 'thin':3
/// to_tsvector('english','thin dog') || to_tsvector('english','fat cat') -> 'cat':4 'dog':2 'fat':3 'thin':1
/// ```
pub fn concat(left: &str, right: &str) -> Result<String> {
    let left = from_text(left)?;
    let shift = left
        .iter()
        .flat_map(|lexeme| lexeme.positions.iter().map(|(at, _)| *at))
        .max()
        .unwrap_or(0);
    let mut merged = left;
    for mut lexeme in from_text(right)? {
        for position in &mut lexeme.positions {
            position.0 = position.0.saturating_add(shift);
        }
        merged.push(lexeme);
    }
    Ok(to_text(&canonical(merged)))
}

/// `ts_headline(config, text, query)`: the text with every matching token wrapped in `<b>`.
///
/// Measured on 19beta1:
///
/// ```text
/// ts_headline('english','The Fat Cats ate a rat', to_tsquery('english','cat'))
///   -> The Fat <b>Cats</b> ate a rat
/// ts_headline('english','The Fat Cats ate a rat', to_tsquery('english','fat & cat'))
///   -> The <b>Fat</b> <b>Cats</b> ate a rat
/// ts_headline('english','The Fat Cats ate a rat', to_tsquery('english','dog'))
///   -> The Fat Cats ate a rat
/// ```
///
/// Three rules fall out of those three lines. **A token is matched by its stem and printed as it
/// was written** — `cat` matches `Cats`, and the markup goes round `Cats`. **The separators
/// survive**, so the answer is the input with tags inserted rather than a rebuilt string. And **no
/// match is the text unchanged**, not an empty string.
///
/// What this does not do is PostgreSQL's *fragment* selection: a real server can return a window of
/// a long document with `MaxWords`/`MinWords`, and this returns the whole text every time. The two
/// agree for any text shorter than the default window, which is every text the suite has, and the
/// difference is declared rather than hidden.
#[must_use]
pub fn headline(config: Config, text: &str, query_lexemes: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if token.is_empty() {
            return;
        }
        let folded = token.to_lowercase();
        let stem = match config {
            Config::Simple => folded,
            Config::English => {
                if crate::value::stopwords::is_stop_word(&folded) {
                    String::new()
                } else {
                    crate::value::stemmer::stem(&folded)
                }
            }
        };
        if !stem.is_empty() && query_lexemes.contains(&stem) {
            out.push_str("<b>");
            out.push_str(token);
            out.push_str("</b>");
        } else {
            out.push_str(token);
        }
        token.clear();
    };
    for c in text.chars() {
        if c.is_alphanumeric() {
            token.push(c);
        } else {
            flush(&mut token, &mut out);
            out.push(c);
        }
    }
    flush(&mut token, &mut out);
    out
}

/// The word tokens of a text, numbered from one.
///
/// A token is a run of alphanumerics; everything else separates. That is narrower than a real
/// server's parser, which also recognises URLs, hosts, file paths and numbers as their own token
/// types — none of which the suite writes, and each of which would be a *different lexeme*, so the
/// gap is a gap and not a wrong answer waiting to happen.
fn tokens(text: &str) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() {
            current.push(c);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out.into_iter()
        .enumerate()
        // Positions are `u16` on a real server and saturate rather than wrap; a text with more
        // than 65,535 tokens is not something the suite writes, and stopping the count is the
        // conservative answer.
        .map(|(i, token)| (u16::try_from(i + 1).unwrap_or(u16::MAX), token))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation here is a row of `captures/pg19_tsvector.txt`, not a rule reasoned about.
    fn canon(text: &str) -> String {
        to_text(&from_text(text).expect("a valid tsvector"))
    }

    /// **The capture's own two rows, side by side**, which is what proves the position model:
    /// the same 1..6 numbering under both configurations, with `english` dropping two of them and
    /// **not** renumbering the rest.
    #[test]
    fn to_tsvector_is_what_postgresql_answers() {
        let text = "The Fat Cats ate a rat";
        assert_eq!(
            to_text(&to_tsvector(Config::English, text)),
            "'ate':4 'cat':3 'fat':2 'rat':6"
        );
        assert_eq!(
            to_text(&to_tsvector(Config::Simple, text)),
            "'a':5 'ate':4 'cats':3 'fat':2 'rat':6 'the':1"
        );
    }

    /// Two occurrences of one stem are one lexeme with both positions, and the irregular past
    /// tense is a lexeme of its own — `'ran':3 'run':1,2`.
    #[test]
    fn repeated_stems_merge_their_positions() {
        assert_eq!(
            to_text(&to_tsvector(Config::English, "running runs ran")),
            "'ran':3 'run':1,2"
        );
    }

    /// `to_tsvector('english', '')` is the empty tsvector, which prints as nothing.
    #[test]
    fn an_empty_text_is_an_empty_tsvector() {
        assert_eq!(to_text(&to_tsvector(Config::English, "")), "");
        // And a text that is nothing but stop words is empty too, which is the same value.
        assert_eq!(to_text(&to_tsvector(Config::English, "the a of")), "");
    }

    /// A configuration this node does not have is `42704`, with a real server's own sentence.
    #[test]
    fn an_unknown_configuration_is_undefined_rather_than_unsupported() {
        let error = Config::resolve("nosuchconfig").expect_err("no such configuration");
        assert_eq!(
            error.to_string(),
            "text search configuration \"nosuchconfig\" does not exist"
        );
        assert!(Config::resolve("english").is_ok());
        assert!(Config::resolve("pg_catalog.english").is_ok());
        assert!(Config::resolve("simple").is_ok());
    }

    /// The capture's three `ts_headline` rows, which between them fix all three rules.
    #[test]
    fn ts_headline_marks_what_the_query_names() {
        let query = |text: &str| {
            crate::value::tsquery::lexemes(
                &crate::value::tsquery::to_tsquery(Config::English, text)
                    .unwrap()
                    .unwrap(),
            )
        };
        let text = "The Fat Cats ate a rat";
        // Matched by **stem**, printed as **written**: `cat` marks up `Cats`.
        assert_eq!(
            headline(Config::English, text, &query("cat")),
            "The Fat <b>Cats</b> ate a rat"
        );
        assert_eq!(
            headline(Config::English, text, &query("fat & cat")),
            "The <b>Fat</b> <b>Cats</b> ate a rat"
        );
        // No match is the text unchanged, not an empty string.
        assert_eq!(headline(Config::English, text, &query("dog")), text);
    }

    /// **The separators survive**, which is what makes this an insertion into the input rather
    /// than a rebuild of it: two spaces stay two, and the punctuation keeps its place.
    #[test]
    fn headline_returns_the_input_with_tags_inserted() {
        let query = vec!["cat".to_owned()];
        assert_eq!(
            headline(Config::English, "a  cat, and", &query),
            "a  <b>cat</b>, and"
        );
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
