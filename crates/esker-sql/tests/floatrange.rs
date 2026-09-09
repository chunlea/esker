//! A column of a **user-defined range type**, against PostgreSQL 19beta1.
//!
//! Run 64's row: 46 tests in `adapters/postgresql/range_test.rb`, whose `setup` declares *two*
//! user range types in one transaction and adds a column of each — `floatrange`
//! (`subtype = float8`) and `stringrange` (`subtype = varchar`). Either one missing fails all 46,
//! so the unit is both.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus creates its own types and its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // The standing catalog type trade, twice now rather than three times: `typname` is a `name`
    // on a real server and `rngsubtype::regtype` is a `regtype`, both `text` here. The
    // `information_schema` line left this list when its columns took `name` and
    // `character varying` — the row agrees whole (ADR 0031 rule 2). **Every value agrees** on the
    // two that remain, and the values are what they ask: `floatrange` is `typtype` `r` and
    // `typcategory` `R`, and its `rngsubtype` is `double precision`.
    types: &[
        "SELECT 'r', t.typname, r.rngsubtype::regtype FROM pg_range r JOIN pg_type t ON t.oid = \
         r.rngtypid WHERE t.typname IN ('floatrange','stringrange') ORDER BY 2",
    ],
    answers: &[
        // **A `varchar` bound comes back as `text`.** The value is right — `["ca""t","do\g")`
        // round-trips byte for byte, which is what `range_test.rb` reads — and it is the *bound's
        // declared type* that differs: `Datum::Text` is what a `varchar` is here, and
        // `pg_typeof(upper(string_range))` reads the value's own type. The same trade every
        // `varchar` in this node makes, arriving one level deeper.
        // **The refusal is right and the type name in it is the representation.** A user range
        // column holds `ColumnType::FloatRange`, whose `name()` is `float8range` — deliberately
        // not a PostgreSQL type name, because PostgreSQL has none for a range over `float8` and
        // the *real* name is the catalog's. `pg_typeof`, `information_schema` and the wire all
        // say `floatrange`, because all three read the oid; these two messages are built from a
        // `ColumnType` with no oid in reach. The `42883` and its DETAIL and HINT agree.
        (
            "SELECT 'r', float_range = '[0.5,0.7]'::numrange FROM fr WHERE id = 101",
            "the message names the representation, not the declared type",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', min(float_range) FROM fr",
            "the message names the representation, not the declared type",
            "UNMEASURED",
        ),
        // **A subtype nobody declared.** `42704 type "nosuchtype" does not exist` there and the
        // standing `0A000` here, which is lowering's: a bare type name is not resolved until the
        // executor has the catalog, because it might be a user type — and a range's subtype
        // *may* be one on a real server, so the name cannot be checked earlier than a column's.
        // Same class of divergence as a `CREATE TABLE` naming a type nobody declared.
        (
            "CREATE TYPE norange AS RANGE (subtype = nosuchtype)",
            "an unresolved type name is 0A000 here and 42704 there",
            "UNMEASURED",
        ),
        // **A range column cannot be indexed here, and can be there.** Every range has a default
        // btree operator class on a real server; this node has no key encoding for one
        // (`esker_keys::row::is_index_key`), so the index is refused rather than built. Without
        // the refusal the index *was* built and the first row written into it hit the row codec's
        // internal "an index key column of type json, jsonb, hstore or a range" — the same hole
        // the `point` unit found for `CREATE INDEX` on a `json` column, in the half that is this
        // node's own gap rather than PostgreSQL's rule.
        (
            "CREATE INDEX fr_range_idx ON fr (float_range)",
            "a range is an index key there and not here; refused rather than built",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_user_range_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_floatrange.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 40,
        "only {checked} statements ran; the corpus did not load"
    );
}
