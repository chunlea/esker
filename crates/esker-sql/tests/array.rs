//! Arrays as a **column type**, against PostgreSQL 19beta1.
//!
//! `= ANY` over an array *expression* landed in phase 6a; an array as the declared type of a
//! column did not, and it is the second of the two blockers standing between this node and the
//! Rails suite's 426 files: `postgresql_specific_schema.rb`'s `bigint_array` declares `int8[]` and
//! `numeric[]` columns, and `ActiveRecord` fails **client-side** — `TypeError: can't quote Array` —
//! because the adapter is not told the column is an array.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // Three statements whose **rows agree and whose declared types do not**, each the standing
    // `pg_catalog` trade: `oid` and `name` and `"char"` are types this node does not have, and it
    // answers `bigint` and `text` — whose values are identical. The third is `array_agg`, which
    // builds its array as text: now that an array is a type it could build one, and that is the
    // slice after the constructor rather than part of storage.
    types: &[
        // These three used to be listed below as refusals: a bare `VALUES` list was not a relation
        // and the statement could not run. It runs now and the **rows are right**; what is left is
        // that `array_agg` declares `text` whatever it collects, where a real server declares the
        // array type of its argument. That is the aggregate's result typing, not arrays, and it
        // shows in `tests/array_subquery.rs` and `tests/values_relation.rs` too.
        "SELECT array_agg(x) FROM (VALUES (1),(2)) v(x)",
        "SELECT array_agg(x ORDER BY x DESC) FROM (VALUES (1),(2)) v(x)",
        "SELECT array_agg(x) FROM (VALUES (NULL::int)) v(x)",
        "SELECT oid, typname, typlen, typinput, typelem, typdelim, typcategory FROM pg_type WHERE \
         typname IN ('_int4','_text') ORDER BY oid",
        "SELECT 'integer[]'::regtype::oid, 'int4[]'::regtype::oid, '_int4'::regtype::oid, \
         'text[]'::regtype::oid",
    ],
    answers: &[
        (
            "SELECT pg_typeof('{1,2}'::int[]), pg_typeof(ARRAY[1,2]), format_type(1007, -1), format_type(1009, -1)",
            "`pg_typeof` is not built. What it would report is asserted directly instead: the four array types' OIDs and names are pinned in `crate::value`'s own tests and in `tests/pg_catalog.rs`, where `ActiveRecord`'s array type-map query now answers with all four.",
        ),
        (
            "SELECT id FROM ar WHERE n @> '{1}' ORDER BY id",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
        ),
        (
            "SELECT '{1,2,3}'::int[], ARRAY[1,2,3], ARRAY[1,2,3]::int[]",
            "**The rows agree and the element's width does not.** `ARRAY[1,2,3]` is an `integer[]` on a real server and a `bigint[]` here, because a bare integer constant is `int4` there and `int8` here — `tests/unknown_literal.rs`'s standing divergence, showing through the constructor. The values are identical and the same statement written `'{1,2,3}'::int[]` agrees on the type as well, which is the line beside each of these.",
        ),
        (
            "SELECT '{1,NULL,3}'::int[], ARRAY[1,NULL,3]",
            "**The rows agree and the element's width does not.** `ARRAY[1,2,3]` is an `integer[]` on a real server and a `bigint[]` here, because a bare integer constant is `int4` there and `int8` here — `tests/unknown_literal.rs`'s standing divergence, showing through the constructor. The values are identical and the same statement written `'{1,2,3}'::int[]` agrees on the type as well, which is the line beside each of these.",
        ),
        (
            "SELECT array_length('{}'::int[], 1), array_ndims('{}'::int[]), array_dims('{}'::int[]), cardinality('{}'::int[])",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
        ),
        (
            "SELECT array_dims('{1,2,3}'::int[]), array_dims('{{1,2},{3,4}}'::int[])",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
        ),
        (
            "SELECT cardinality('{1,2,3}'::int[]), cardinality('{{1,2},{3,4}}'::int[]), array_ndims('{1,2,3}'::int[])",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
        ),
        (
            "SELECT ('{1,2,3}'::int[])[1:2]",
            "A **slice** (`a[1:2]`) and a second subscript (`a[1][2]`). One subscript of a one-dimensional array answers now; a slice returns an array, which is the constructor's slice, and `a[1]` of a two-dimensional array is NULL rather than a row — measured, and the line is here so it stays measured.",
        ),
        (
            "SELECT ('{{1,2},{3,4}}'::int[])[1][2], ('{{1,2},{3,4}}'::int[])[1]",
            "A **slice** (`a[1:2]`) and a second subscript (`a[1][2]`). One subscript of a one-dimensional array answers now; a slice returns an array, which is the constructor's slice, and `a[1]` of a two-dimensional array is NULL rather than a row — measured, and the line is here so it stays measured.",
        ),
        (
            "SELECT 1 = ALL('{1,1}'::int[]), 1 = ALL('{1,2}'::int[])",
            "`= ANY` over an array **column value** and `= ALL` over any array. `= ANY` over an array *expression* has worked since phase 6a and still does — `id = ANY('{1,3}')` is in this corpus and agrees — and what is new is an array that arrives as a value rather than as text. The next slice, with the operators.",
        ),
        (
            "SELECT 1 = ANY('{}'::int[]), 1 = ALL('{}'::int[])",
            "`= ANY` over an array **column value** and `= ALL` over any array. `= ANY` over an array *expression* has worked since phase 6a and still does — `id = ANY('{1,3}')` is in this corpus and agrees — and what is new is an array that arrives as a value rather than as text. The next slice, with the operators.",
        ),
        (
            "SELECT '{1,2}'::int[] @> '{1}'::int[], '{1}'::int[] <@ '{1,2}'::int[], '{1,2}'::int[] && '{2,3}'::int[]",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
        ),
        (
            "SELECT '{1,2}'::int[] @> '{}'::int[]",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
        ),
        (
            "SELECT NULL::int[] @> '{1}'::int[]",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
        ),
        (
            "SELECT '{1,2}'::int[] || '{3}'::int[], 3 || '{1,2}'::int[], '{1,2}'::int[] || 3",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
        ),
        (
            "SELECT array_position('{a,b,c}'::text[], 'b'), array_remove('{1,2,1}'::int[], 1)",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
        ),
        (
            "SELECT array_to_string('{1,2,3}'::int[], ','), string_to_array('1,2,3', ',')",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
        ),
        (
            "SELECT '{1,2}'::int2[], '{1,2}'::numeric[], '{a}'::varchar[]",
            "A cast of the `ARRAY[]` constructor, which needs the constructor. And `int2[]`, `varchar[]`: this node has four array types — over `int8`, `int4`, `numeric` and `text` — which are the ones `ActiveRecord`'s schemas declare. A fifth is a variant, a tag and an ordering fixture, and is added when a schema asks for one.",
        ),
    ],
};

#[test]
fn every_array_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_array.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 80,
        "only {checked} statements ran; the corpus did not load"
    );
}
