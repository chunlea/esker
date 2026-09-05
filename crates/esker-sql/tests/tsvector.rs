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
//! # The whole capture replays
//!
//! `corpus/pg19_tsvector.txt` is `captures/pg19_tsvector.txt` (vendored, ADR 0075) **byte for byte** — the harness's
//! captures directory is not in git, so this copy is the capture's only backup and it is kept
//! whole. The replay took the file in prefixes as the machinery underneath it landed: part 1 the
//! declaration and round trip, part 2 the value and operator surface once there was a stemmer, and
//! part 3 `schema_test.rb`'s two schemas and two GIN indexes — which needed the operator-class
//! recording ([ADR 0070](../../../docs/adr/0070-an-operator-class-is-recorded-and-the-index-underneath-is-ordered.md)),
//! a catalog function's volatility read rather than assumed, and last a `tsvector` **column**
//! admitted as a `gin` key ([ADR 0066](../../../docs/adr/0066-a-tsvector-is-its-canonical-text.md),
//! amended: under `gin` the ordering is unused rather than absent). There is no prefix left.
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

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `name` and `"char"` there, `text` here — the standing choice every catalog view in this
    // crate makes, for columns whose comparison is identical.
    types: &[
        "SELECT 'r', cfgname, nspname FROM pg_ts_config c JOIN pg_namespace n ON n.oid = c.cfgnamespace ORDER BY cfgname",
        "SELECT 'r', typname, typtype, typcategory, typdelim, typlen FROM pg_type WHERE typname IN ('tsvector','tsquery','_tsvector','regconfig') ORDER BY typname",
        "SELECT 'r', a.typname AS array_of_tsvector FROM pg_type b JOIN pg_type a ON a.oid = b.typarray WHERE b.typname = 'tsvector'",
        "SELECT 'r', c.relname, a.attname, format_type(a.atttypid, a.atttypmod) FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid WHERE c.relname = 'tsv' AND a.attnum > 0 ORDER BY a.attnum",
        // Part 3's catalog readback. `nspname`, `relname` and `amname` are all three of them
        // `name` on a real server; the **rows** agree, which is what says both indexes were
        // recorded with the access method the statement asked for and that the expression one
        // reads `indkey` 0 with `indexprs` set.
        "SELECT 'r', n.nspname, c.relname AS index_name, am.amname, i.indnatts, i.indkey::text, i.indexprs IS NOT NULL AS is_expression FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid JOIN pg_namespace n ON n.oid = c.relnamespace JOIN pg_am am ON am.oid = c.relam WHERE c.relname IN ('c_index_full_text_search','e_index_things_on_name_vector') ORDER BY n.nspname, c.relname",
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
        (
            "SELECT 'r', c.relname, pg_get_indexdef(c.oid) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.relname IN ('c_index_full_text_search','e_index_things_on_name_vector') AND n.nspname = 'test_schema' ORDER BY c.relname",
            "**The expression index prints as written here and as a deparsed parse tree there**, \
             and the column index beside it agrees exactly — which is what says the difference is \
             the printing and not the index. PostgreSQL stores `pg_node_tree` and reconstructs the \
             text, so `to_tsvector('english', coalesce(things.name, ''))` comes back as \
             `to_tsvector('english'::regconfig, (COALESCE(name, ''::character varying))::text)`: \
             the configuration cast made explicit, the argument cast to the type the function \
             takes, the qualifier dropped and the function name upper-cased to its catalog \
             spelling. This node stores the text the user wrote (`crate::exec::ddl`'s \
             `index_expression`, where a `CASE` is the one shape that cannot round-trip and is \
             deparsed instead). Closing it means a deparser that reproduces PostgreSQL's own \
             coercion, which is a unit of its own and reaches every stored expression, not just \
             this one. The `name`-versus-`text` column type is the standing catalog choice on top.",
            "pg19_tsvector.txt:117",
        ),
        // **Two configurations, not thirty-two.** `pg_am`'s own comment states the rule this
        // follows: a row for a configuration nothing can be tokenised with would be a claim
        // rather than a report. `simple` is here now, `english` arrives with the stemmer.
        (
            "SELECT 'r', cfgname, nspname FROM pg_ts_config c JOIN pg_namespace n ON n.oid = c.cfgnamespace ORDER BY cfgname",
            "this node has the configurations it can tokenise with, which is simple and, once the \
             stemmer lands, english",
            "pg19_tsvector.txt:52",
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
            "pg19_tsvector.txt:84",
        ),
        (
            "SELECT 'r', typname, typtype, typcategory, typdelim, typlen FROM pg_type WHERE typname IN ('tsvector','tsquery','_tsvector','regconfig') ORDER BY typname",
            "no regconfig type: a configuration is named by a string argument here, never held as \
             a value",
            "pg19_tsvector.txt:53",
        ),
    ],
};

#[test]
fn every_tsvector_value_and_operator_answer_is_postgresql_19_s() {
    let whole = include_str!("corpus/pg19_tsvector.txt");
    let checked = parity::replay(whole, CORPUS_FIXTURE, &DIVERGENCES);
    assert!(
        checked > 70,
        "only {checked} statements ran; the corpus did not load"
    );
}
