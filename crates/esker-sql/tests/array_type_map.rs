//! The `pg_type` rows an array column needs before `ActiveRecord` can quote one.
//!
//! Run 53's ranking: `TypeError: can't quote Array`, **43 tests over 2 files**, 42 of them
//! `array_test.rb`. It is a *Ruby* error and not a bug in the adapter: `ActiveRecord` registers an
//! array type by the element type's `typarray` oid, and a `typarray` pointing at a row that is not
//! there registers nothing — so the column is an ordinary one as far as the adapter knows, and
//! quoting a Ruby `Array` for it falls through to `quote` and raises before a statement is sent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty.** The four catalog type families closed it — `name` (ADR 0084), `"char"`
    // (ADR 0095), `oid` (ADR 0097) and `regproc` (ADR 0098) — and `pg_typeof` answers a
    // `regtype` on both sides (ADR 0093). The *values* never changed a character.
    types: &[
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
    ],
    answers: &[
        // **`regproc` *is* a type here now** (ADR 0098), and these two stopped being about it:
        // the statement runs, and what differs is the **count** — this node's `pg_type` holds the
        // types it has, which is the design the next entry states. The third statement of the
        // group left this list entirely, because `dangling_typelem` is 0 on both.
        //
        // `typinput = 'array_in'` with no cast is still `22P02 invalid input syntax for type oid`
        // on both servers, measured: the unknown literal resolves to `oid` and not to `regproc`,
        // because `=` over a `regproc` is `oideq`. `tests/reg_proc.rs` asserts it.
        (
            "SELECT 'r', count(*) FROM pg_type WHERE typinput = 'array_in'::regproc",
            "The predicate answers now; the count is this node's own — its `pg_type` lists the \
             types it has, which is the decision the entry below states.",
            "pg19_array_type_map.txt:50",
        ),
        (
            "SELECT 'r', count(*) = count(*) FILTER (WHERE typcategory = 'A') AS \
             array_in_implies_category_A FROM pg_type WHERE typinput = 'array_in'::regproc",
            "An aggregate `FILTER` clause, which this node does not have. The `regproc` half of \
             this entry's reason closed with ADR 0098; the `FILTER` half is its own gap and is \
             asked again below through `typinput::text`.",
            "pg19_array_type_map.txt:52",
        ),
        // **This node's `pg_type` is the types it has**, which is the whole design: the rows are
        // derived from `ColumnType::ALL` so that a type cannot be added and left out of its own
        // catalog. A real server ships 370 array types and this one has the 23 it can store. The
        // number is not the question the capture asks — the three checks that are, all answer.
        (
            "SELECT 'r', count(*) FROM pg_type WHERE typcategory = 'A'",
            "this node's pg_type holds the types it has, not PostgreSQL's whole catalogue",
            "pg19_array_type_map.txt:51",
        ),
        // **The standing `varchar`/`text` trade, seen through a subscript.** `pg_typeof(tags)` is
        // `character varying[]` on both, because an array *value* carries its element type; an
        // element pulled out of one is a `Datum::Text`, and a `Datum` has no `Varchar` variant —
        // `text`, `varchar` and `bpchar` are one representation and three types here, which is
        // what `Datum::fits` is about. Answering `character varying` would mean a value that
        // remembers a type its bytes do not distinguish, which is the citext shape and is a
        // change to the value vocabulary rather than to arrays.
        // **`_record` is the exception the capture warns about**, and this node has no
        // pseudo-types: `array_in` implies `typcategory = 'A'` for every row here and for all but
        // one row there. The warning is why the two columns are populated from what each means
        // rather than derived from one another — which is what makes this pair a divergence of
        // one missing row and not of a wrong rule.
        (
            "SELECT 'r', count(*) AS array_in_but_not_category_a FROM pg_type WHERE \
             typinput::text = 'array_in' AND typcategory <> 'A'",
            "the one row this counts on a real server is the pseudo-type _record",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', typname, typcategory FROM pg_type WHERE typinput::text = 'array_in' AND \
             typcategory <> 'A' ORDER BY typname",
            "_record is a pseudo-type and this node has none",
            "UNMEASURED",
        ),
    ],
};

#[test]
fn every_array_type_map_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array_type_map.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 35,
        "only {checked} statements ran; the corpus did not load"
    );
}
