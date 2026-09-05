//! `tsvector` and `tsquery` — run 79's largest row, 52 tests over two files.
//!
//! Measured before it was scoped, and the two files want different things.
//! `adapters/postgresql/full_text_test.rb` (**3 tests**) declares a `tsvector` column, writes the
//! literal `'text' 'vector'`, reads the same characters back, updates it, and dumps the schema.
//! `adapters/postgresql/schema_test.rb` (**48 tests, none of them about full text**) dies in a
//! shared setup that wants the column and two GIN indexes.
//!
//! [ADR 0066](../../../docs/adr/0066-a-tsvector-is-its-canonical-text.md) is what this implements:
//! a value is its canonical text, so equality, ordering and an index over the column are the text
//! machinery's.
//!
//! # This slice replays parts 1 and 2, and the corpus holds all of it
//!
//! `corpus/pg19_tsvector.txt` is `captures/pg19_tsvector.txt` **byte for byte** — the harness's
//! captures directory is not in git, so this copy is the capture's only backup and it is kept
//! whole. The replay takes the file in prefixes as the machinery underneath it lands: part 1 is
//! the declaration and round trip, part 2 the value and operator surface once there was a stemmer.
//!
//! **Part 3 is one statement away.** `schema_test.rb`'s two schemas, `tsvector` column and two GIN
//! indexes now all answer as a real server does except
//! `CREATE INDEX … USING gin (name_vector)` — a `tsvector` **column** key, which
//! [`esker_keys::row::is_index_key`] refuses because a `tsvector`'s byte order is not its printed
//! order ([ADR 0066](../../../docs/adr/0066-a-tsvector-is-its-canonical-text.md)). The *expression*
//! index over `to_tsvector('english', …)` — the same type — is accepted and writes, so the two
//! paths disagree with each other and reconciling them is an ADR decision rather than a fix.
//!
//! **Why it is a prefix and not a list of declared divergences**: a declared divergence over a
//! statement that *raises* here and *succeeds* on PostgreSQL still aborts the transaction, and
//! every later statement in the file then answers `25P02`. Fifty declarations would have hidden
//! the file behind the first of them — and part 3 demonstrated it twice over, since one refusal
//! swallowed fourteen statements and left a single one visible.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// Where the capture stops, which is now a single statement rather than a section.
///
/// Everything above this marker replays; inside part 3 the two schemas, the column, both
/// expression indexes and every readback agree, and `USING gin (name_vector)` does not. The
/// marker stays until that is decided, because the capture is one transaction and a refusal
/// inside it swallows every statement after.
const PART_3: &str = "# ---- part 3:";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `name` and `"char"` there, `text` here — the standing choice every catalog view in this
    // crate makes, for columns whose comparison is identical.
    types: &[
        "SELECT 'r', cfgname, nspname FROM pg_ts_config c JOIN pg_namespace n ON n.oid = c.cfgnamespace ORDER BY cfgname",
        "SELECT 'r', typname, typtype, typcategory, typdelim, typlen FROM pg_type WHERE typname IN ('tsvector','tsquery','_tsvector','regconfig') ORDER BY typname",
        "SELECT 'r', a.typname AS array_of_tsvector FROM pg_type b JOIN pg_type a ON a.oid = b.typarray WHERE b.typname = 'tsvector'",
        "SELECT 'r', c.relname, a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid WHERE c.relname = 'tsv' AND a.attnum > 0 ORDER BY a.attnum",
    ],
    // **The corpus format cannot express this row, and the answer is right.** A row's columns are
    // separated by `|` and a `tsquery`'s *or* operator **is** `|`, so the expected side parses
    // into three fields (`r`, `'fat' `, ` 'cat'`) where this node's answer is two. Both print
    // identically — the harness's own report shows the same characters on both lines — and only
    // the structure differs.
    //
    // The behaviour is covered where the separator cannot reach it:
    // `value::tsquery::tests::the_operators_print_the_way_postgresql_prints_them` asserts
    // `to_tsquery("fat | cat")` renders `'fat' | 'cat'`. Declared here rather than silently
    // dropped from the corpus, because the corpus is the capture and the capture is right.
    answers: &[
        // **Two configurations, not thirty-two.** `pg_am`'s own comment states the rule this
        // follows: a row for a configuration nothing can be tokenised with would be a claim
        // rather than a report. `simple` is here now, `english` arrives with the stemmer.
        (
            "SELECT 'r', cfgname, nspname FROM pg_ts_config c JOIN pg_namespace n ON n.oid = c.cfgnamespace ORDER BY cfgname",
            "this node has the configurations it can tokenise with, which is simple and, once the \
             stemmer lands, english",
        ),
        // One type short of the four: there is no `regconfig` here, because nothing takes a text
        // search configuration as a *value*. The three tsvector rows match exactly — `typtype`,
        // `typcategory`, `typdelim` and `typlen` all measured against the oracle.
        (
            "SELECT 'r', to_tsquery('english', 'fat | cat')",
            "not a disagreement: the corpus separates a row's columns with `|` and a tsquery's or \
             operator is `|`, so the expected side parses into three fields where the answer is \
             two. Both print the same characters. Covered by \
             `value::tsquery::tests::the_operators_print_the_way_postgresql_prints_them`",
        ),
        (
            "SELECT 'r', typname, typtype, typcategory, typdelim, typlen FROM pg_type WHERE typname IN ('tsvector','tsquery','_tsvector','regconfig') ORDER BY typname",
            "no regconfig type: a configuration is named by a string argument here, never held as \
             a value",
        ),
    ],
};

#[test]
fn every_tsvector_value_and_operator_answer_is_postgresql_19_s() {
    let whole = include_str!("corpus/pg19_tsvector.txt");
    let through_part_two = whole.split_once(PART_3).map_or(whole, |(before, _)| before);
    let checked = parity::replay(through_part_two, CORPUS_FIXTURE, &DIVERGENCES);
    assert!(
        checked > 45,
        "only {checked} statements ran; the corpus did not load"
    );
}
