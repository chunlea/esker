//! **The casts `pg_cast` defines between the shapes and scalars below, run against the
//! evaluator** — ninety-four of them, and see the paragraph on what is *not* here.
//!
//! `debts-v1.1.md` #43's first mechanism. The operand is a **column**, not a literal, and that is
//! the whole point: a literal under a cast is folded at lowering (`parse::lower::lower_cast`), so
//! a probe written `('1'::numeric)::money` measures the *fold* and never reaches
//! `exec::cursor`'s `Expr::Cast` arm. #42 stopped the fold discarding the node when the target is
//! `text` and could go no further precisely because that arm knows fewer conversions than the
//! fold does — this file is how many fewer.
//!
//! **157 pairs**, each with a representative value of the source type and **each in its own
//! savepoint**: a refusal that aborts the transaction would otherwise swallow every probe after
//! it, which is what the first draft of this capture did.
//!
//! **It was 94, and 94 was not every pair.** The first draft probed `pg_cast` restricted to the
//! thirty-four types `castprobe` declares a column for, and said in this comment that those were
//! the types this node has. They were not: `int2`, `int4` and `int8` were missing from both axes,
//! and `pg_cast`'s integer rows are its largest family. `castprobe2` at the end of the capture
//! adds the three columns and the sixty-six pairs they unlock. The count is now every `pg_cast`
//! row between two of the **forty-one** scalars this node has that the oracle also has — `citext`,
//! `hstore`, `ltree` and `lquery` are node types the oracle has no extension for, and are the
//! whole of what is still unmeasured. A matrix's completeness is a property of its *fixture*, and
//! nothing in a generated file compares the fixture against the claim over it.
//!
//! **`char` in a cast is `character(1)`, not the one-byte type.** Three probes in the first half
//! — `(c_bpchar)::char`, `(c_text)::char`, `(c_varchar)::char` — therefore measure a cast to
//! `bpchar`, which is a pair this file probes anyway, and the four real `"char"` targets are in
//! the second half with their quotes. That is why 157 pairs are covered by 160 probes.
//!
//! **23 of the 157 still disagree**, and there is still no pair anywhere where this node answers
//! and PostgreSQL refuses. It was 33 before `money`, `regproc`, `regtype` and `interval -> time`,
//! 49 before the conversions that do not go through the text (`value::convert_without_text`), and
//! 63 before the geometric fourteen. What is left:
//!
//! ```text
//!  3  the two catalogs differ                 int2/int4/int8 -> regproc: the oid is right and
//!                                             this node's `pg_proc` has fifteen functions
//!  7  both answer, the values differ          inet, boolean, and the rest of #44's renderings
//! 10  both refuse, the sentence differs       jsonb's shape, refused after the input function
//!  3  ADR 0097's oid space                    regclass -> oid, int4, int8                   (#45)
//! ```
//!
//! **#43's first mechanism is done**: every `pg_cast` pair this node and the oracle share either
//! answers what a real server answers or is declared above with a reason that is not a missing
//! conversion. The last one to go was `"char" -> int4`, which is the shape worth remembering: a
//! `"char"` and a `text` are the same `Datum::Text`, so the *value* could not say which cast it
//! was — the **plan** could, and `exec::cursor::declared_type_of` asks it. The general fix, a
//! `Cast` node carrying the type it casts *from*, is still the right one and is still unwritten;
//! this is the one pair that needed it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT (c_regclass)::oid FROM castprobe",
            "**Not a conversion gap: the boundary of ADR 0097, ruled 2026-09-09 (`debts-v1.1.md` #45).** A relation id here is a `u64` and an `oid` is four bytes, so `'pg_class'::regclass::oid` on a live relation is `22003 value ... is out of range for type oid` where a real server answers a small number. Narrowing the id is the allocation change ADR 0097 weighed and declined, and answering a truncated oid would be a wrong value wearing a right type. Measured here before the ruling and kept as the row that shows what that boundary looks like from the cast's side; the aggregates reach the same edge through the implicit cast, which is how #45 was found.",
            "pg19_cast_matrix.txt:200",
        ),
        (
            "SELECT (c_bytea)::uuid FROM castprobe",
            "**#44 group 5 -- both refuse, with different sentences.** It opened as ten and is **three**. The seven that went were the `jsonb` family, and they were the group's own example: a `jsonb` object cast to a number was `22P02 invalid input syntax` here because the whole document went to the target's input function, where PostgreSQL asks the *kind* first and answers `22023 cannot cast jsonb object to type numeric`. Paid 2026-09-10 by `value::json::cast_to_scalar`, which both the evaluator's cast and the literal fold ask -- the literal spelling had a declared divergence of its own in `pg19_json.txt` and went with it. What is left is **two mechanisms and neither is an ordering**: `bytea -> uuid` differs in the sentence a length check writes (`invalid input length for type uuid`, with a DETAIL naming the bytes), and `text -> regclass` and `varchar -> regclass` differ only in the SQLSTATE this node attaches to the identical sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:59",
        ),
        (
            "SELECT (c_text)::regclass FROM castprobe",
            "**#44 group 5 -- both refuse, with different sentences.** It opened as ten and is **three**. The seven that went were the `jsonb` family, and they were the group's own example: a `jsonb` object cast to a number was `22P02 invalid input syntax` here because the whole document went to the target's input function, where PostgreSQL asks the *kind* first and answers `22023 cannot cast jsonb object to type numeric`. Paid 2026-09-10 by `value::json::cast_to_scalar`, which both the evaluator's cast and the literal fold ask -- the literal spelling had a declared divergence of its own in `pg19_json.txt` and went with it. What is left is **two mechanisms and neither is an ordering**: `bytea -> uuid` differs in the sentence a length check writes (`invalid input length for type uuid`, with a DETAIL naming the bytes), and `text -> regclass` and `varchar -> regclass` differ only in the SQLSTATE this node attaches to the identical sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:218",
        ),
        (
            "SELECT (c_varchar)::regclass FROM castprobe",
            "**#44 group 5 -- both refuse, with different sentences.** It opened as ten and is **three**. The seven that went were the `jsonb` family, and they were the group's own example: a `jsonb` object cast to a number was `22P02 invalid input syntax` here because the whole document went to the target's input function, where PostgreSQL asks the *kind* first and answers `22023 cannot cast jsonb object to type numeric`. Paid 2026-09-10 by `value::json::cast_to_scalar`, which both the evaluator's cast and the literal fold ask -- the literal spelling had a declared divergence of its own in `pg19_json.txt` and went with it. What is left is **two mechanisms and neither is an ordering**: `bytea -> uuid` differs in the sentence a length check writes (`invalid input length for type uuid`, with a DETAIL naming the bytes), and `text -> regclass` and `varchar -> regclass` differ only in the SQLSTATE this node attaches to the identical sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:275",
        ),
        (
            "SELECT (c_bpchar)::xml FROM castprobe",
            "**#44 group 4 -- both answer and the values differ.** It opened as eight pairs and is **two**, and the two that went are the two that were named: `uuid -> bytea` left with #43's first mechanism (it was never a rendering -- it is the same text round trip seen from the other side), and `boolean` into a character type left on 2026-09-10, because `ToText` and `||` both had the rule that a boolean prints `t` and *casts* to `true` and the evaluator's `Expr::Cast` arm did not -- one fact, three readers, `crate::value::cast_text_under` is the one now. The `inet` prefix went the same day and by the same route: an address keeps the prefix its output function hides, `parse::lower` had had that rule for a literal since the address unit, and the evaluator's cast -- every cast over a *column* -- had not. **What is left is not a rendering at all**, which is the group's own lesson: `bpchar -> xml` is the blank padding of `character(4)`, a `bpchar` question wearing an `xml` name, and `oid -> regclass` prints a name where a real server prints the number for an oid no relation has, which is the catalog and ADR 0097's boundary. A group named from its symptom held four mechanisms and three of them left with rows that were about something else.",
            "pg19_cast_matrix.txt:56",
        ),
        (
            "SELECT (c_oid)::regclass FROM castprobe",
            "**#44 group 4 -- both answer and the values differ.** It opened as eight pairs and is **two**, and the two that went are the two that were named: `uuid -> bytea` left with #43's first mechanism (it was never a rendering -- it is the same text round trip seen from the other side), and `boolean` into a character type left on 2026-09-10, because `ToText` and `||` both had the rule that a boolean prints `t` and *casts* to `true` and the evaluator's `Expr::Cast` arm did not -- one fact, three readers, `crate::value::cast_text_under` is the one now. The `inet` prefix went the same day and by the same route: an address keeps the prefix its output function hides, `parse::lower` had had that rule for a literal since the address unit, and the evaluator's cast -- every cast over a *column* -- had not. **What is left is not a rendering at all**, which is the group's own lesson: `bpchar -> xml` is the blank padding of `character(4)`, a `bpchar` question wearing an `xml` name, and `oid -> regclass` prints a name where a real server prints the number for an oid no relation has, which is the catalog and ADR 0097's boundary. A group named from its symptom held four mechanisms and three of them left with rows that were about something else.",
            "pg19_cast_matrix.txt:173",
        ),
        (
            "SELECT (c_regclass)::int8 FROM castprobe",
            "**Not a conversion gap: ADR 0097's boundary again (`debts-v1.1.md` #45).** A relation id here is a `u64`, so `'pg_class'::regclass::int4` is `22003 integer out of range` and `::int8` answers `9223372036854774786` where a real server says `1259`. The same edge `regclass -> oid` sits on, reached by the other two casts `regclass` has. Recorded rather than fixed, for the reason #45 gives.",
            "pg19_cast_matrix.txt:475",
        ),
        (
            "SELECT (c_regclass)::int4 FROM castprobe",
            "**Not a conversion gap: ADR 0097's boundary again (`debts-v1.1.md` #45).** A relation id here is a `u64`, so `'pg_class'::regclass::int4` is `22003 integer out of range` and `::int8` answers `9223372036854774786` where a real server says `1259`. The same edge `regclass -> oid` sits on, reached by the other two casts `regclass` has. Recorded rather than fixed, for the reason #45 gives.",
            "pg19_cast_matrix.txt:478",
        ),
        (
            "SELECT (c_int8)::regproc FROM castprobe2",
            "**The cast is right and the *name* is this node's catalog.** `65::regproc` is `int4eq` on a real server because oid 65 is `int4eq` in every PostgreSQL, and this node's `pg_proc` holds the fifteen functions `pg_catalog::BUILTIN_FUNCTIONS` lists — a measured list of what the suite asks for, not a copy of PostgreSQL's several thousand. So the conversion happens, the `regproc` carries oid 65, and it prints as `65` because there is no name for it here. **That is PostgreSQL's own rule for an oid it cannot name**, measured beside this: `999999::regproc` is `999999` there. Not a missing conversion and not a rendering bug — the two servers hold different catalogs, which is the same reason `char_type.rs` declares its `pg_class` counts. `debts-v1.1.md` #43.",
            "pg19_cast_matrix.txt:322",
        ),
        (
            "SELECT (c_int2)::regproc FROM castprobe2",
            "**The cast is right and the *name* is this node's catalog.** `65::regproc` is `int4eq` on a real server because oid 65 is `int4eq` in every PostgreSQL, and this node's `pg_proc` holds the fifteen functions `pg_catalog::BUILTIN_FUNCTIONS` lists — a measured list of what the suite asks for, not a copy of PostgreSQL's several thousand. So the conversion happens, the `regproc` carries oid 65, and it prints as `65` because there is no name for it here. **That is PostgreSQL's own rule for an oid it cannot name**, measured beside this: `999999::regproc` is `999999` there. Not a missing conversion and not a rendering bug — the two servers hold different catalogs, which is the same reason `char_type.rs` declares its `pg_class` counts. `debts-v1.1.md` #43.",
            "pg19_cast_matrix.txt:358",
        ),
        (
            "SELECT (c_int4)::regproc FROM castprobe2",
            "**The cast is right and the *name* is this node's catalog.** `65::regproc` is `int4eq` on a real server because oid 65 is `int4eq` in every PostgreSQL, and this node's `pg_proc` holds the fifteen functions `pg_catalog::BUILTIN_FUNCTIONS` lists — a measured list of what the suite asks for, not a copy of PostgreSQL's several thousand. So the conversion happens, the `regproc` carries oid 65, and it prints as `65` because there is no name for it here. **That is PostgreSQL's own rule for an oid it cannot name**, measured beside this: `999999::regproc` is `999999` there. Not a missing conversion and not a rendering bug — the two servers hold different catalogs, which is the same reason `char_type.rs` declares its `pg_class` counts. `debts-v1.1.md` #43.",
            "pg19_cast_matrix.txt:394",
        ),
    ],
};

#[test]
fn every_cast_pg_defines_is_the_cast_this_node_performs() {
    let checked = parity::replay(
        include_str!("corpus/pg19_cast_matrix.txt"),
        FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 90,
        "only {checked} statements ran; the matrix is not being read"
    );
}
