//! **`void` has no array type** — wire v3 family **F5**, and it is a fact about PostgreSQL's
//! catalog rather than about this enum.
//!
//! Measured over all 100 distinct type spellings of the wire v3 probe list, four array shapes each
//! (`tests/captures/pg19_array_of_void.txt`): on 19beta1 **`void` is the only one that refuses**,
//! and it refuses all four with `42704 could not find array type for data type void`. Everything
//! else answers — including `json`, `xml`, `point` and the nine array types `GREATEST` refuses,
//! because building an array asks nothing of the element but its type.
//!
//! **The predicate is `void`, not "this node has no array for it".** `ArrayValue::array_of` is
//! `None` for `lquery`, `int2vector` and `oidvector` too, and 19beta1 answers `lquery[]`,
//! `int2vector[]` and `oidvector[]` for those — refusing them would turn a wrong *type* into a
//! refused *statement*, which is the worse direction (ADR 0031). They are pinned below at what
//! this node answers today, with the family that owns each.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// PostgreSQL's whole sentence, which has no `DETAIL` and no `HINT`.
const REFUSAL: &str = "!42704 could not find array type for data type void";

/// **All four array shapes over `void`.**
///
/// Three of them go through the `ARRAY[…]` constructor — a subscript and an `unnest` both need one
/// built first — so the gate is there and `array_agg` needs its own. Both are asserted, because a
/// gate on the constructor alone would leave `array_agg(void)` answering.
#[test]
fn no_array_shape_accepts_a_void() {
    let mut node = parity::Node::new(&[]);
    for sql in [
        "SELECT ARRAY[NULL::void]",
        "SELECT (ARRAY[NULL::void])[1]",
        "SELECT u FROM unnest(ARRAY[NULL::void]) u",
        "SELECT array_agg(x) FROM (SELECT NULL::void AS x) t",
    ] {
        assert_eq!(node.answer(sql).to_string(), REFUSAL, "{sql}");
    }
}

/// **The lower bound: every other type still builds an array**, including the ones other families
/// refuse for other reasons. `GREATEST(json, json)` is `42883` and `ARRAY[json]` is `json[]` on the
/// same server — an array constructor compares nothing.
#[test]
fn every_other_type_still_builds_an_array() {
    let mut node = parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS hstore"]);
    for (written, array) in [
        ("'{\"a\":1}'::json", "json[]"),
        ("'<a/>'::xml", "xml[]"),
        ("'(1,2)'::point", "point[]"),
        ("'a=>1'::hstore", "hstore[]"),
        ("1::int8", "bigint[]"),
        ("'2020-01-01'::date", "date[]"),
        ("'[1,2]'::int4range", "int4range[]"),
        ("'11111111-1111-1111-1111-111111111111'::uuid", "uuid[]"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof(ARRAY[{written}])")),
            vec![vec![array]],
            "ARRAY[{written}] is {array} on 19beta1"
        );
    }
}

/// **Three spellings this unit does not close**, pinned at today's answer with the family that
/// owns each — 19beta1 answers an array type for all three and this node answers `text[]`.
///
/// `lquery[]`, `int2vector[]` and `oidvector[]` are types this node does not have: closing them
/// adds a `ColumnType`, which is five places and a format decision, not a gate. **F6**, and
/// [ADR 0107](../../../docs/adr/0107-a-borrowed-representation-needs-somewhere-to-carry-its-identity.md)
/// is the decision — `lquery` in its step one, the two vectors in its step two.
#[test]
fn the_three_arrays_this_node_does_not_have() {
    let mut node = parity::Node::new(&["CREATE EXTENSION IF NOT EXISTS ltree"]);
    for (written, pg) in [
        ("'a.*'::lquery", "lquery[]"),
        ("'1 2'::int2vector", "int2vector[]"),
        ("'1 2'::oidvector", "oidvector[]"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof(ARRAY[{written}])")),
            vec![vec!["text[]"]],
            "19beta1 answers {pg} here; F6 owns it, and refusing instead would be the worse \
             direction"
        );
    }
}
