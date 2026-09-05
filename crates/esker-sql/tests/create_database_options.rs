//! `CREATE DATABASE`'s option list — the statement `rake db:create` sends.
//!
//! The list is cut out of the source before `sqlparser` sees it and carried beside the tree
//! (`crate::parse::strip_create_database_options`), which is the mechanism this crate already has
//! for a clause the parser cannot read. What the options *mean* on a cluster with one encoding and
//! one collation is decided in the lowering, and `tests/database.rs` pins that; this file pins the
//! oracle.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus creates its own databases.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "CREATE DATABASE vlcapE2 LC_COLLATE = 'C'",
            "**This one is the oracle's locale, not a rule**, and it is the clearest statement of \
             what `LC_COLLATE` alone means: PostgreSQL refuses it because *its* default template \
             carries the container's `en_US.utf8`, and this node accepts it because its only \
             collation **is** `C`. Both servers apply the same rule — a new collation must match \
             the template's — to two different templates. The refusal returns the moment the \
             collation asked for is not the one this node has, which is the line below it here \
             and the `LC_COLLATE = 'en_US.utf8'` case in `tests/database.rs`.",
            "UNMEASURED",
        ),
        (
            "CREATE DATABASE vlcapE7 ENCODING = 'SQL_ASCII'",
            "**`0A000` rather than PostgreSQL's `22023`, and the difference is the HINT.** \
             PostgreSQL's sentence says the encoding is incompatible with the *template's* and \
             hints at `template0` — an escape hatch that works there and cannot work here, because \
             this node has one encoding and every template it has is in it. A message whose HINT \
             cannot be followed is worse than a refusal that names the construct, so what is \
             answered is the encoding by name. `SQL_ASCII` is on PostgreSQL's list of encodings, \
             which is why it is this rather than the `42704` a name nobody has would get — the \
             line above measures that half and agrees.",
            "UNMEASURED",
        ),
        (
            "SELECT count(*) FROM pg_database WHERE datname LIKE 'vlca%' OR datname LIKE 'vlq%'",
            "The wake of the first entry rather than a divergence of its own: a capture is one \
             session, so a statement PostgreSQL refused and this node ran is a database it has and \
             the oracle never made. `vlcapE2` is that database, and it is the one this count is \
             over. It is deleted when the collation rule above stops depending on the container's \
             locale — which is to say when the corpus is recaptured on a `C` cluster.",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_create_database_option_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_create_database_options.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}
