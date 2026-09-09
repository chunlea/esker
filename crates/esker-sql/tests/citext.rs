//! `citext` as a column type and a value, against PostgreSQL 19beta1.
//!
//! The other half of run 47's ranking row 7. `tests/hstore.rs` is the first half.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus installs its own extension and builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog type trade — `name`, `oid`, `"char"` and `regproc` answered as `text`
    // and `bigint`, whose *values* are identical, which is why all three rows agree. These are the
    // adapter's own boot queries and what it reads out of them is the typname, the typcategory and
    // the typinput: `citext`, `S`, `citextin`, all right.
    types: &[
        "SELECT 'r', t.typname, t.typelem, t.typdelim, t.typinput, t.typtype, t.typbasetype, \
         t.typcategory, t.typlen FROM pg_type as t WHERE t.typname IN ('citext') ORDER BY \
         t.typname",
    ],
    answers: &[
        // **The standing `text` collation divergence, and the citext answers beside it are
        // right.** `'B' < 'a'` over plain `text` is `f` on the oracle, whose database collation is
        // `en_US.utf8`, and `t` here, where the key space is byte ordered — `crate::row` has the
        // argument and `tests/corpus/pg19_order.txt` declares it for `text`. The two *citext*
        // comparisons in the same row agree, which is the half this unit is about.
        (
            "SELECT 'r', 'B'::citext < 'a'::citext, 'B' < 'a', 'B'::citext > 'a'::citext",
            "the standing text collation divergence; both citext answers agree",
            "UNMEASURED",
        ),
        // **Three functions this node does not have**, none of them citext's: `string_agg` and
        // `||` are the pair `tests/aggregate_type.rs` has declared since the array unit, and
        // `length` has never been built. Each sits in a `SAVEPOINT` so one `0A000` cannot take the
        // rest of the file — the statements that measure what citext *does* run after them.
        (
            "SELECT 'r', string_agg(v::text, ' ; ' ORDER BY v) FROM (VALUES \
             ('B'::citext),('a'),('C'),('b'),('A')) t(v)",
            "string_agg is not built; the ordering it would show is measured by ORDER BY below",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', length('ABC'::citext)",
            "length is not built, for any type",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', pg_typeof('x'::citext || 'y')",
            "|| is not built over text, so the citext concatenation it would demote to has \
             nothing to demote to",
            "UNMEASURED",
        ),
        // **The right refusal in the wrong sentence.** Both raise `23505` and neither builds the
        // index; PostgreSQL has a message for a *build* that finds duplicates — `could not create
        // unique index … Key (cival)=(Cased Text) is duplicated`, quoting the stored spelling —
        // where this node reports the one an insert gets. The build path here checks each row
        // against the index it is filling, so the collision is found as an insert would find it.
        // What the suite reads is the failure, and both fail; the sentence is the gap.
        (
            "CREATE UNIQUE INDEX cit_cival_uidx ON cit (cival)",
            "a unique index that cannot be built reports the insert's sentence, not the build's",
            "pg19_hstore_citext.txt:108",
        ),
    ],
};

#[test]
fn every_citext_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_citext.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 35,
        "only {checked} statements ran; the corpus did not load"
    );
}
