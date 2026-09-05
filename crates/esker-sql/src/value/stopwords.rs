//! PostgreSQL's English stop-word list.
//!
//! # Source
//!
//! Copied verbatim from **`/usr/share/postgresql/19/tsearch_data/english.stop`** of
//! **PostgreSQL 19beta1 (Debian `19~beta1-1.pgdg13+1`)**, the same server
//! `captures/pg19_tsvector.txt` was taken against. **127 words**, under the
//! [PostgreSQL Licence](https://www.postgresql.org/about/licence/) — PostgreSQL ships it as the
//! Snowball project's English stop list.
//!
//! It is copied and not recalled, and that distinction is the whole reason this file has a source
//! header: a list written from memory is right about the words anyone would think of and wrong
//! about the rest, and every word it gets wrong is a lexeme that should have been dropped and was
//! not — a wrong answer no test in the suite would reach, because the suite never writes those
//! words.
//!
//! The capture is what proves the list is used at all:
//!
//! ```text
//! to_tsvector('english', 'The Fat Cats ate a rat') -> 'ate':4 'cat':3 'fat':2 'rat':6
//! to_tsvector('simple',  'The Fat Cats ate a rat') -> 'a':5 'ate':4 'cats':3 'fat':2 'rat':6 'the':1
//! ```
//!
//! `the` and `a` are gone under `english` and present under `simple`, **and the positions do not
//! shift**: the numbering is over the tokens, and a dropped stop word takes its number with it.
//!
//! # One thing this file does not settle
//!
//! Whether PostgreSQL checks the list **before** or **after** stemming. Every word in the capture
//! answers the same either way — `the` and `a` stem to themselves — so the capture cannot tell
//! them apart. The discriminating probe is **`to_tsvector('english', 'only')`**: `only` is in the
//! list *and* in the stemmer's exception table, which maps it to `onli`, which is **not** in the
//! list. Checked before stemming the answer is empty; checked after, it is `'onli':1`. Queued for
//! the oracle, and named here so the next reader does not have to find it again.

/// The 127 words, **sorted** so that [`is_stop_word`] can binary-search them.
///
/// The sorting is this file's; PostgreSQL ships them in a different order, and nothing depends on
/// which order they are read in.
pub const ENGLISH: &[&str] = &[
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "am",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "did",
    "do",
    "does",
    "doing",
    "don",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "had",
    "has",
    "have",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "itself",
    "just",
    "me",
    "more",
    "most",
    "my",
    "myself",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "s",
    "same",
    "she",
    "should",
    "so",
    "some",
    "such",
    "t",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "very",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "why",
    "will",
    "with",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

/// Whether a token is an English stop word.
///
/// The token is expected already lowercased, which is the order PostgreSQL's snowball dictionary
/// works in: fold, then consult the list.
#[must_use]
pub fn is_stop_word(word: &str) -> bool {
    ENGLISH.binary_search(&word).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The count is part of the citation: 127 is what `wc -l` gives on the file named above, and a
    /// list that quietly grew or shrank is no longer the one this file claims to be.
    #[test]
    fn the_list_is_the_one_the_header_names() {
        assert_eq!(ENGLISH.len(), 127, "the header cites 127 words");
        let mut sorted = ENGLISH.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ENGLISH.len(),
            "a duplicate would break the citation and the search"
        );
        assert_eq!(
            sorted, ENGLISH,
            "the table must stay sorted for `is_stop_word`"
        );
        assert!(
            ENGLISH
                .iter()
                .all(|word| word.is_ascii() && !word.is_empty())
        );
    }

    /// **What the capture actually proves**, and nothing beyond it: `the` and `a` are dropped under
    /// `english`, and words around them are not.
    #[test]
    fn the_words_the_capture_drops_are_stop_words() {
        for word in ["the", "a"] {
            assert!(
                is_stop_word(word),
                "{word} is dropped by to_tsvector('english', ...)"
            );
        }
        for word in ["fat", "cats", "ate", "rat", "thin", "dog", "run"] {
            assert!(
                !is_stop_word(word),
                "{word} survives into a lexeme in the capture"
            );
        }
    }

    /// Case is the caller's job, and this asserts the contract rather than hiding it: an unfolded
    /// token is not found, which is why the docs say the token arrives lowercased.
    #[test]
    fn the_list_is_consulted_in_lower_case() {
        assert!(is_stop_word("the"));
        assert!(!is_stop_word("The"));
    }
}
