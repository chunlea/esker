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
//! # This slice replays part 1, and the corpus holds all of it
//!
//! `corpus/pg19_tsvector.txt` is `captures/pg19_tsvector.txt` **byte for byte** — the harness's
//! captures directory is not in git, so this copy is the capture's only backup and it is kept
//! whole. The replay below stops at the capture's own `part 2` marker, because the parts after it
//! need a stemmer (`to_tsvector('english', …)`) and the GIN indexes, and those are separate
//! landable slices.
//!
//! **A prefix rather than a list of declared divergences**, and the reason is mechanical: a
//! declared divergence over a statement that *raises* here and *succeeds* on PostgreSQL still
//! aborts the transaction, and every later statement in the file then answers `25P02`. Fifty
//! declarations would have hidden the file behind the first of them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// Where the capture stops being about the type and starts being about the functions.
const PART_2: &str = "# ---- part 2:";

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
            "SELECT 'r', typname, typtype, typcategory, typdelim, typlen FROM pg_type WHERE typname IN ('tsvector','tsquery','_tsvector','regconfig') ORDER BY typname",
            "no regconfig type: a configuration is named by a string argument here, never held as \
             a value",
        ),
    ],
};

#[test]
fn every_tsvector_declaration_and_round_trip_is_postgresql_19_s() {
    let whole = include_str!("corpus/pg19_tsvector.txt");
    let part_one = whole.split_once(PART_2).map_or(whole, |(before, _)| before);
    let checked = parity::replay(part_one, CORPUS_FIXTURE, &DIVERGENCES);
    assert!(
        checked > 15,
        "only {checked} statements ran; the corpus did not load"
    );
}
