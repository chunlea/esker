//! **`'{…}'::text::regclass[]`** — the last row of wire v3 family **F7**, and it is not the same
//! rule as the scalar beside it.
//!
//! `tests/cast_to_vector.rs` measured every `'<value>'::text::<T>` over the probe list's 100
//! spellings and found this node refusing exactly two. One was `oidvector`, closed there; this is
//! the other, `0A000 a relation name read as a regclass without a catalog`.
//!
//! # What `pg_cast` says, and why the two directions differ
//!
//! Measured on 19beta1, 2026-09-10, `127.0.0.1:55432`:
//!
//! ```text
//! pg_cast rows for text -> regclass      1     so the scalar cast is that function
//! pg_cast rows for text -> regclass[]    0     so the array cast is an I/O conversion
//! ```
//!
//! A cast with a `pg_cast` row runs **that function**, and `text_regclass` resolves a **name**. A
//! cast without one is the target's **input function** over the source's text — here `array_in`
//! with `regclassin` per element, and `regclassin` reads all-digits as an **oid**. So:
//!
//! ```text
//! '1259'::text::regclass       42P01 relation "1259" does not exist    <- text_regclass, a name
//! '{1259}'::text::regclass[]   {pg_class}                              <- regclassin, an oid
//! '{1259}'::text[]::regclass[] 42P01 relation "1259" does not exist    <- text_regclass per element
//! ARRAY['1259']::regclass[]    {pg_class}                              <- unknown, so regclassin
//! ARRAY['1259'::text]::regclass[] 42P01                                <- text, so text_regclass
//! ```
//!
//! **The `text[]` row is the counter-example that makes this a rule rather than a coincidence**:
//! one step more through a real `text[]` and the digits become a name again, because that step has
//! a `pg_cast` row and this one does not.
//!
//! # What this node does about it
//!
//! The per-element resolution happens where the catalog is — `exec::cursor`'s cast arm, which
//! already resolves `Datum::Array` to `regclass[]` for the `oid[]` route — and the array grammar
//! stays `array_in`'s: braces, quoting, whitespace, `NULL` and the `[1:2]=` bound prefix are that
//! function's rules, and a second reader of the same grammar is how two spellings of one literal
//! come to disagree (`one-grammar-one-parser`, `debts-v1.1.md` #41).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

fn node() -> parity::Node {
    parity::Node::new(&["CREATE TABLE ra (id int8)", "CREATE TABLE rb (id int8)"])
}

/// **The I/O conversion, element by element**, with 19beta1's answer beside each.
#[test]
fn a_text_literal_becomes_a_regclass_array() {
    let mut node = node();
    for (written, answer) in [
        ("'{ra}'::text::regclass[]", "{ra}"),
        ("'{ra,rb}'::text::regclass[]", "{ra,rb}"),
        // `array_in`'s rules, not a second reader's: a quoted element, unquoted whitespace, an
        // explicit `NULL`, the empty array and the bound prefix.
        ("'{\"ra\"}'::text::regclass[]", "{ra}"),
        ("'{ ra }'::text::regclass[]", "{ra}"),
        ("'{ra,NULL}'::text::regclass[]", "{ra,NULL}"),
        ("'{}'::text::regclass[]", "{}"),
        ("'[1:2]={ra,rb}'::text::regclass[]", "{ra,rb}"),
        // A schema the `search_path` holds is not printed back, exactly as the scalar's is not.
        ("'{public.ra}'::text::regclass[]", "{ra}"),
        // **An oid nothing names prints back as itself** — `regclassout`'s rule, and the half that
        // says the digits went through `oidin` and not through a relation lookup.
        ("'{999999}'::text::regclass[]", "{999999}"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT ({written})::text")),
            vec![vec![answer]],
            "{written} is {answer} on 19beta1"
        );
        assert_eq!(
            node.rows(&format!("SELECT pg_typeof({written})::text")),
            vec![vec!["regclass[]"]],
            "and the type is the array, not the text it was written as"
        );
    }
}

/// **All digits are an oid here and a name one step away**, which is the whole of what `pg_cast`'s
/// missing row means.
#[test]
fn digits_are_an_oid_through_the_io_conversion_and_a_name_through_the_cast() {
    let mut node = node();
    let oid = node.rows("SELECT 'ra'::regclass::oid::text")[0][0].clone();
    assert_eq!(
        node.rows(&format!("SELECT ('{{{oid}}}'::text::regclass[])::text")),
        vec![vec!["{ra}"]],
        "the I/O conversion reads {oid} as an oid and prints the relation it names"
    );
    // **One step more and it is a name again.** `text[] -> regclass[]` coerces element by element
    // through the `pg_cast` row for `text -> regclass`, which resolves a name and nothing else.
    let answer = node
        .answer(&format!("SELECT '{{{oid}}}'::text[]::regclass[]"))
        .to_string();
    assert!(
        answer.starts_with("!42P01"),
        "through a real text[] the digits are a relation name, and there is none: {answer}"
    );
    // And a name that is not there is the same `42P01`, whichever way it arrives.
    assert!(
        node.answer("SELECT '{nosuch}'::text::regclass[]")
            .to_string()
            .starts_with("!42P01"),
        "a name nothing has is 42P01, not a wrong relation and not 0A000"
    );
    // A leading `-` is not all-digits, so it is a name — measured, and it is `regclassin`'s own
    // boundary rather than a guess about what "looks numeric" means.
    assert!(
        node.answer("SELECT '{-1}'::text::regclass[]")
            .to_string()
            .starts_with("!42P01"),
        "`-1` is a name because it is not all digits"
    );
}

/// **Every operand shape, because the one that worked was the one the lowering special-cases.**
///
/// The same sweep `cast_to_vector.rs` made for `oidvector`: a bare literal reaches
/// `lower_regclass_array`, and a column, a concatenation and a `::text` in the middle do not.
#[test]
fn a_regclass_array_resolves_from_any_operand() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ra (id int8)",
        "CREATE TABLE t (v text)",
        "INSERT INTO t VALUES ('{ra}')",
    ]);
    for written in [
        "'{ra}'::regclass[]",
        "'{ra}'::text::regclass[]",
        "('{' || 'ra}')::regclass[]",
        "(SELECT v FROM t)::regclass[]",
    ] {
        assert_eq!(
            node.rows(&format!("SELECT ({written})::text")),
            vec![vec!["{ra}"]],
            "{written} is {{ra}} on 19beta1"
        );
    }
    assert_eq!(
        node.rows("SELECT (v::regclass[])::text FROM t"),
        vec![vec!["{ra}"]],
        "and through a column, which is the shape r1's probe sent"
    );
}
