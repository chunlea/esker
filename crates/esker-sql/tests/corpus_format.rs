//! **The corpus format's own escape contract**, asserted through the node like any other corpus.
//!
//! `docs/plans/debts-v1.1.md` #20: a row is `statement TAB types TAB rows`, rows separated by
//! `" ; "` and cells by `"|"`, and nothing said what happens when a **value** holds one of those.
//! It silently became extra cells — `to_tsquery('fat | cat')` renders `'fat' | 'cat'` and parsed
//! as three columns where the node answered two, so a row that read identically was reported as a
//! disagreement. Three files declared exactly that, two with an `UNMEASURED` provenance, and it
//! cost this lane two rounds before anyone printed the raw `Answer`.
//!
//! Two halves, and only one of them is decidable without an escape:
//!
//! * **A type name with a comma** — `numeric(10,2)` — is decidable, because a comma that separates
//!   two types is never inside parentheses. `parity_harness::declared_types` splits at depth zero
//!   and six statements in `tests/numeric.rs` stopped needing a declaration for it.
//! * **The directive's position is part of the contract**: `#!escaped` must appear in the
//!   **header comment block**, before the first line that is neither blank nor a comment. Not
//!   merely somewhere in the file — this very corpus writes the directive as an example in its own
//!   header prose, and a file explaining the format would otherwise silently start escaping.
//! * **A `|`, a `;`, a newline or a tab inside a value** is not. It needs an escape, which needs
//!   the *writer* to emit one — so the contract is opt-in per file (`#!escaped`), measured that
//!   way: 19 rows across `tests/corpus/` already hold a `\\` or a `\n` inside a value, and
//!   un-escaping unconditionally would silently change what they assert.
//!
//! This file is the reader's half, end to end and through the node rather than as a unit test of
//! a private function. It is also **the specification the capture tool has to hit**: the corpus
//! beside it is what `sesscap.py` must produce for these four statements, and today it cannot —
//! it writes no escapes, and its psql-error-context filter drops every output line beginning with
//! a space, which is every continuation line of a multi-line value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const CORPUS_FIXTURE: &[&str] = &[];

/// Nothing: a value that needs the escape is a value this node already answers correctly.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT E'one\\ntwo', length(E'one\\ntwo')",
            "**An `E'...'` literal is refused by name, and the row is here for the *format* rather than for the literal.** `0A000 the literal E'…' is not supported` — the escape-string syntax is not implemented, and the suite sends it **0 times** in 508 captured statements, so building it would be building what nothing asks for (contract C2's own reasoning, the same as `md5`'s in `tests/index_expression_volatility.rs`). What the row does assert is the reader: the expected side carries `\\\\n` inside a value and is parsed back to a real newline before the comparison, which is the half of the escape contract no other row reaches. It is also what the capture tool has to be able to write — a value with a newline in it is exactly what `sesscap.py`'s line filter drops today.",
            "pg19_corpus_format.txt:43",
        ),
        (
            "SELECT E'tab\\tend', length(E'tab\\tend')",
            "**An `E'...'` literal is refused by name, and the row is here for the *format* rather than for the literal.** `0A000 the literal E'…' is not supported` — the escape-string syntax is not implemented, and the suite sends it **0 times** in 508 captured statements, so building it would be building what nothing asks for (contract C2's own reasoning, the same as `md5`'s in `tests/index_expression_volatility.rs`). What the row does assert is the reader: the expected side carries `\\\\t` inside a value and is parsed back to a real tab before the comparison, which is the half of the escape contract no other row reaches. It is also what the capture tool has to be able to write — a value with a tab in it is exactly what `sesscap.py`'s line filter drops today.",
            "pg19_corpus_format.txt:44",
        ),
    ],
};

#[test]
fn a_value_that_holds_a_separator_survives_the_corpus_format() {
    let checked = parity::replay(
        include_str!("corpus/pg19_corpus_format.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert_eq!(checked, 4, "the contract corpus is not being read");
}
