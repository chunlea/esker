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
    // **The standing catalog type trade, and every one of these rows agrees.** `typname` is a
    // `name` on a real server, `typdelim` and `typcategory` are `"char"`, the oids are `oid` and
    // `typinput` is a `regproc`; all of them are `text` or `bigint` here, with the same characters
    // and the same numbers in them. `pg_typeof` is the same trade one step over — a `regtype`
    // there, `text` here, which is what `'x'::regtype` already answers.
    types: &[
        "SELECT 'r', t.oid, t.typname, t.typelem, t.typdelim, t.typinput, t.typtype, \
         t.typbasetype FROM pg_type as t LEFT JOIN pg_range as r ON t.oid = r.rngtypid WHERE \
         t.typname IN \
         ('_int4','_text','_varchar','_timestamp','_timestamptz','_date','_numeric','_uuid','_bool','_jsonb') \
         ORDER BY t.typname",
        "SELECT 'r', b.typname AS base, b.typarray, a.typname AS array_name, a.typelem, a.typelem \
         = b.oid AS points_back FROM pg_type b JOIN pg_type a ON a.oid = b.typarray WHERE \
         b.typname IN \
         ('varchar','int4','timestamp','timestamptz','numeric','text','uuid','bool','date') ORDER \
         BY b.typname",
        "SELECT 'r', typname, typcategory, typelem <> 0 AS has_element, typinput FROM pg_type \
         WHERE typname IN ('_int4','_varchar','int4','varchar') ORDER BY typname",
        "SELECT 'r', pg_typeof(ARRAY['a','b']::varchar[]), pg_typeof(ARRAY[1.5]::numeric[]), \
         pg_typeof(ARRAY[true])",
        "SELECT 'r', b.typname, b.typarray <> 0 AS has_array, a.typname AS array_name, a.typelem \
         = b.oid AS points_back FROM pg_type b JOIN pg_type a ON a.oid = b.typarray WHERE \
         b.typname IN ('hstore','citext') ORDER BY b.typname",
        "SELECT 'r', b.typname, b.oid < 10000 AS oid_is_builtin, a.oid < 10000 AS \
         array_oid_is_builtin FROM pg_type b JOIN pg_type a ON a.oid = b.typarray WHERE b.typname \
         IN ('hstore','citext','varchar') ORDER BY b.typname",
        "SELECT 'r', t.typname, t.typelem::regtype::text AS element, t.typlen FROM pg_type t \
         WHERE t.typname IN \
         ('_varchar','_timestamp','_bool','_date','_uuid','_jsonb','_float8','_bytea') ORDER BY \
         t.typname",
        "SELECT 'r', b.typname, b.typarray, a.typname AS array_name FROM pg_type b JOIN pg_type a \
         ON a.oid = b.typarray WHERE b.typname IN \
         ('bytea','bpchar','float4','float8','interval','json','oid','time') ORDER BY b.typname",
    ],
    answers: &[
        // **`regproc` is not a type here**, and the three statements that need it are the
        // capture's own conformance checks. They are not lost: each is asked again below with
        // `typinput::text`, the spelling both servers read — and `typinput = 'array_in'` with no
        // cast at all is `22P02 invalid input syntax for type oid` on a real server, measured,
        // because the unknown literal resolves to `oid` and not to `regproc`.
        (
            "SELECT 'r', count(*) FROM pg_type WHERE typinput = 'array_in'::regproc",
            "regproc is not a type here; asked again below through typinput::text",
        ),
        (
            "SELECT 'r', count(*) = count(*) FILTER (WHERE typcategory = 'A') AS \
             array_in_implies_category_A FROM pg_type WHERE typinput = 'array_in'::regproc",
            "regproc, and an aggregate FILTER clause; asked again below through typinput::text",
        ),
        (
            "SELECT 'r', count(*) AS dangling_typelem FROM pg_type t WHERE t.typinput = \
             'array_in'::regproc AND NOT EXISTS (SELECT 1 FROM pg_type e WHERE e.oid = t.typelem)",
            "regproc; **the same check runs below** through typinput::text and answers 0, which \
             is the half of this statement that is about arrays",
        ),
        // **This node's `pg_type` is the types it has**, which is the whole design: the rows are
        // derived from `ColumnType::ALL` so that a type cannot be added and left out of its own
        // catalog. A real server ships 370 array types and this one has the 23 it can store. The
        // number is not the question the capture asks — the three checks that are, all answer.
        (
            "SELECT 'r', count(*) FROM pg_type WHERE typcategory = 'A'",
            "this node's pg_type holds the types it has, not PostgreSQL's whole catalogue",
        ),
        // **`box` is not a type here**, and these three exist to say that `typdelim` is not always
        // a comma — `box` uses `;`, the one exception in a real server's catalogue. Every type
        // this node has uses `,`, so the rule the capture warns about is recorded and cannot be
        // demonstrated: a hard-coded comma would be wrong on a server with a `box` and is right
        // on this one. The day a geometric type lands here, these three become the test for it.
        (
            "SELECT 'r', typname, typdelim FROM pg_type WHERE typname IN \
             ('int4','text','varchar','box','_box') ORDER BY typname",
            "box is not a type here, so the one delimiter that is not a comma has no row",
        ),
        (
            "SELECT 'r', b.typname, b.typdelim AS element_delim, a.typname AS array_name, \
             a.typdelim AS array_row_delim FROM pg_type b JOIN pg_type a ON a.oid = b.typarray \
             WHERE b.typname = 'box'",
            "box is not a type here",
        ),
        (
            "SELECT 'r', '{(1,1),(0,0);(3,3),(2,2)}'::box[], \
             array_length('{(1,1),(0,0);(3,3),(2,2)}'::box[], 1)",
            "box is not a type here",
        ),
        // **The standing `varchar`/`text` trade, seen through a subscript.** `pg_typeof(tags)` is
        // `character varying[]` on both, because an array *value* carries its element type; an
        // element pulled out of one is a `Datum::Text`, and a `Datum` has no `Varchar` variant —
        // `text`, `varchar` and `bpchar` are one representation and three types here, which is
        // what `Datum::fits` is about. Answering `character varying` would mean a value that
        // remembers a type its bytes do not distinguish, which is the citext shape and is a
        // change to the value vocabulary rather than to arrays.
        (
            "SELECT 'r', pg_typeof(tags), pg_typeof(tags[1]), array_length(tags, 1) FROM atm",
            "an array element is a Datum::Text: varchar and text are one representation here",
        ),
        // **`_record` is the exception the capture warns about**, and this node has no
        // pseudo-types: `array_in` implies `typcategory = 'A'` for every row here and for all but
        // one row there. The warning is why the two columns are populated from what each means
        // rather than derived from one another — which is what makes this pair a divergence of
        // one missing row and not of a wrong rule.
        (
            "SELECT 'r', count(*) AS array_in_but_not_category_a FROM pg_type WHERE \
             typinput::text = 'array_in' AND typcategory <> 'A'",
            "the one row this counts on a real server is the pseudo-type _record",
        ),
        (
            "SELECT 'r', typname, typcategory FROM pg_type WHERE typinput::text = 'array_in' AND \
             typcategory <> 'A' ORDER BY typname",
            "_record is a pseudo-type and this node has none",
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
