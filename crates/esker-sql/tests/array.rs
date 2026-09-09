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
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still
        // differs is one of the standing declared-type families listed on
        // `parity::Divergences::types`. The reason each one used to carry described an answer
        // that had stopped differing.
        // `pg_typeof`'s own `regtype`/`text` trade (ADR 0077), and nothing else: the two array
        // types it names agree since the `int4` rung, and both `format_type` calls always did.
        "SELECT pg_typeof('{1,2}'::int[]), pg_typeof(ARRAY[1,2]), format_type(1007, -1), \
         format_type(1009, -1)",
        // These two used to be listed below as refusals: a bare `VALUES` list was not a relation
        // and the statement could not run. It runs now and the **rows are right**; what is left is
        // that `array_agg` declares `text` whatever it collects, where a real server declares the
        // array type of its argument. That is the aggregate's result typing, not arrays, and it
        // shows in `tests/array_subquery.rs` and `tests/values_relation.rs` too. A third,
        // `SELECT array_agg(x) FROM (VALUES (NULL::int)) v(x)`, left with `Literal::TypedNull`:
        // the cast survives lowering, so the column is `integer` and the aggregate declares
        // `integer[]` — one of ADR 0047's four.
        "SELECT oid, typname, typlen, typinput, typelem, typdelim, typcategory FROM pg_type WHERE \
         typname IN ('_int4','_text') ORDER BY oid",
    ],
    answers: &[
        (
            "SELECT id FROM ar WHERE n @> '{1}' ORDER BY id",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
            "pg19_array.txt:86",
        ),
        (
            "SELECT array_length('{}'::int[], 1), array_ndims('{}'::int[]), array_dims('{}'::int[]), cardinality('{}'::int[])",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
            "pg19_array.txt:118",
        ),
        (
            "SELECT array_dims('{1,2,3}'::int[]), array_dims('{{1,2},{3,4}}'::int[])",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
            "pg19_array.txt:121",
        ),
        (
            "SELECT cardinality('{1,2,3}'::int[]), cardinality('{{1,2},{3,4}}'::int[]), array_ndims('{1,2,3}'::int[])",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
            "pg19_array.txt:122",
        ),
        (
            "SELECT ('{1,2,3}'::int[])[1:2]",
            "A **slice** (`a[1:2]`) and a second subscript (`a[1][2]`). One subscript of a one-dimensional array answers now; a slice returns an array, which is the constructor's slice, and `a[1]` of a two-dimensional array is NULL rather than a row — measured, and the line is here so it stays measured.",
            "pg19_array.txt:124",
        ),
        (
            "SELECT ('{{1,2},{3,4}}'::int[])[1][2], ('{{1,2},{3,4}}'::int[])[1]",
            "A **slice** (`a[1:2]`) and a second subscript (`a[1][2]`). One subscript of a one-dimensional array answers now; a slice returns an array, which is the constructor's slice, and `a[1]` of a two-dimensional array is NULL rather than a row — measured, and the line is here so it stays measured.",
            "pg19_array.txt:125",
        ),
        (
            "SELECT '{1,2}'::int[] @> '{1}'::int[], '{1}'::int[] <@ '{1,2}'::int[], '{1,2}'::int[] && '{2,3}'::int[]",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
            "pg19_array.txt:145",
        ),
        (
            "SELECT '{1,2}'::int[] @> '{}'::int[]",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
            "pg19_array.txt:146",
        ),
        (
            "SELECT NULL::int[] @> '{1}'::int[]",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
            "pg19_array.txt:147",
        ),
        (
            "SELECT '{1,2}'::int[] || '{3}'::int[], 3 || '{1,2}'::int[], '{1,2}'::int[] || 3",
            "The array **operators** — `@>`, `<@`, `&&`, `||` — which are the slice after the constructor. Nothing here is approximated in the meantime: each is `0A000` naming itself.",
            "pg19_array.txt:148",
        ),
        (
            "SELECT array_position('{a,b,c}'::text[], 'b'), array_remove('{1,2,1}'::int[], 1)",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
            "pg19_array.txt:149",
        ),
        (
            "SELECT array_to_string('{1,2,3}'::int[], ','), string_to_array('1,2,3', ',')",
            "`array_ndims`, `array_dims`, `array_remove`, `array_to_string`, `string_to_array` and `unnest`: more of the array function surface, none of which the schema files call. `array_length`, `array_lower`, `array_upper`, `array_position` and `cardinality` answer now, including over a two-dimensional array, which is what the shape is for.",
            "pg19_array.txt:150",
        ),
        // **`'{a}'::varchar[]` used to be here** and the entry said a fifth array type would be
        // "a variant, a tag and an ordering fixture, added when a schema asks for one". Sixteen
        // of them were added at once instead: a `typarray` that names a `pg_type` row which is
        // not there is what left `ActiveRecord` unable to quote *any* array, so the answer was
        // every array type rather than the next one (`tests/array_type_map.rs`).
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
