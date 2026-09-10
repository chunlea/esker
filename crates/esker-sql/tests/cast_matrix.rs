//! **Every cast `pg_cast` defines between two types this node has, run against the evaluator.**
//!
//! `debts-v1.1.md` #43's first mechanism. The operand is a **column**, not a literal, and that is
//! the whole point: a literal under a cast is folded at lowering (`parse::lower::lower_cast`), so
//! a probe written `('1'::numeric)::money` measures the *fold* and never reaches
//! `exec::cursor`'s `Expr::Cast` arm. #42 stopped the fold discarding the node when the target is
//! `text` and could go no further precisely because that arm knows fewer conversions than the
//! fold does — this file is how many fewer.
//!
//! 94 pairs, taken from `pg_cast` on 19beta1 restricted to the types this node has, each with a
//! representative value of the source type and **each in its own savepoint**: a refusal that
//! aborts the transaction would otherwise swallow every probe after it, which is what the first
//! draft of this capture did.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT (c_money)::numeric FROM castprobe",
            "**#43 group 2 -- `money` converts in neither direction.** `numeric -> money` and `money -> numeric` are both `pg_cast` entries a real server computes, and both are `42846` here. This is the pair that opened #43: the fold used to do it at parse time, so `567.89::numeric::money` worked and `(c_numeric)::money` never did, and nothing said so until #42 moved a literal out of the fold's reach.",
            "pg19_cast_matrix.txt:140",
        ),
        (
            "SELECT (c_numeric)::money FROM castprobe",
            "**#43 group 2 -- `money` converts in neither direction.** `numeric -> money` and `money -> numeric` are both `pg_cast` entries a real server computes, and both are `42846` here. This is the pair that opened #43: the fold used to do it at parse time, so `567.89::numeric::money` worked and `(c_numeric)::money` never did, and nothing said so until #42 moved a literal out of the fold's reach.",
            "pg19_cast_matrix.txt:158",
        ),
        (
            "SELECT (c_interval)::time FROM castprobe",
            "**#43 group 3 -- two conversions of their own.** `interval -> time` is `00:00:00` on a real server (the day part is dropped, the clock part kept) and `22007` here, which is the node reading the interval's *text* rather than converting it. `regclass -> oid` is `22003 out of range` because a system relation's oid here is synthetic and wider than `int4` -- a fixture-sensitive answer, and the one row of this matrix whose fix may be the oid space rather than the cast.",
            "pg19_cast_matrix.txt:116",
        ),
        (
            "SELECT (c_regclass)::oid FROM castprobe",
            "**#43 group 3 -- two conversions of their own.** `interval -> time` is `00:00:00` on a real server (the day part is dropped, the clock part kept) and `22007` here, which is the node reading the interval's *text* rather than converting it. `regclass -> oid` is `22003 out of range` because a system relation's oid here is synthetic and wider than `int4` -- a fixture-sensitive answer, and the one row of this matrix whose fix may be the oid space rather than the cast.",
            "pg19_cast_matrix.txt:191",
        ),
        (
            "SELECT (c_bytea)::uuid FROM castprobe",
            "**#43 group 5 -- both refuse, with different sentences.** A `jsonb` object cast to a number is `cannot cast jsonb object to type numeric` there and `22P02 invalid input syntax` here, because this node routes the cast through the target's input function rather than refusing the *shape* first; `text -> regclass` differs only in the SQLSTATE this node attaches to the same sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:50",
        ),
        (
            "SELECT (c_jsonb)::bool FROM castprobe",
            "**#43 group 5 -- both refuse, with different sentences.** A `jsonb` object cast to a number is `cannot cast jsonb object to type numeric` there and `22P02 invalid input syntax` here, because this node routes the cast through the target's input function rather than refusing the *shape* first; `text -> regclass` differs only in the SQLSTATE this node attaches to the same sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:122",
        ),
        (
            "SELECT (c_jsonb)::float4 FROM castprobe",
            "**#43 group 5 -- both refuse, with different sentences.** A `jsonb` object cast to a number is `cannot cast jsonb object to type numeric` there and `22P02 invalid input syntax` here, because this node routes the cast through the target's input function rather than refusing the *shape* first; `text -> regclass` differs only in the SQLSTATE this node attaches to the same sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:125",
        ),
        (
            "SELECT (c_jsonb)::float8 FROM castprobe",
            "**#43 group 5 -- both refuse, with different sentences.** A `jsonb` object cast to a number is `cannot cast jsonb object to type numeric` there and `22P02 invalid input syntax` here, because this node routes the cast through the target's input function rather than refusing the *shape* first; `text -> regclass` differs only in the SQLSTATE this node attaches to the same sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:128",
        ),
        (
            "SELECT (c_jsonb)::numeric FROM castprobe",
            "**#43 group 5 -- both refuse, with different sentences.** A `jsonb` object cast to a number is `cannot cast jsonb object to type numeric` there and `22P02 invalid input syntax` here, because this node routes the cast through the target's input function rather than refusing the *shape* first; `text -> regclass` differs only in the SQLSTATE this node attaches to the same sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:134",
        ),
        (
            "SELECT (c_text)::regclass FROM castprobe",
            "**#43 group 5 -- both refuse, with different sentences.** A `jsonb` object cast to a number is `cannot cast jsonb object to type numeric` there and `22P02 invalid input syntax` here, because this node routes the cast through the target's input function rather than refusing the *shape* first; `text -> regclass` differs only in the SQLSTATE this node attaches to the same sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:209",
        ),
        (
            "SELECT (c_varchar)::regclass FROM castprobe",
            "**#43 group 5 -- both refuse, with different sentences.** A `jsonb` object cast to a number is `cannot cast jsonb object to type numeric` there and `22P02 invalid input syntax` here, because this node routes the cast through the target's input function rather than refusing the *shape* first; `text -> regclass` differs only in the SQLSTATE this node attaches to the same sentence. Refusals, so no value is wrong -- but a client that branches on SQLSTATE sees a different answer.",
            "pg19_cast_matrix.txt:266",
        ),
        (
            "SELECT (c_bool)::bpchar FROM castprobe",
            "**#43 group 4 -- both answer and the values differ.** Eight pairs, and they are renderings rather than conversions: `inet` loses its prefix length (`10.0.0.1` for `10.0.0.1/32`), `boolean` renders `t` where a real server writes `true` into a character type, `uuid -> bytea` gives the *text* of the uuid where a real server gives its sixteen bytes. Each is one output function, and none of them is the `Expr::Cast` arm being absent.",
            "pg19_cast_matrix.txt:11",
        ),
        (
            "SELECT (c_bool)::varchar FROM castprobe",
            "**#43 group 4 -- both answer and the values differ.** Eight pairs, and they are renderings rather than conversions: `inet` loses its prefix length (`10.0.0.1` for `10.0.0.1/32`), `boolean` renders `t` where a real server writes `true` into a character type, `uuid -> bytea` gives the *text* of the uuid where a real server gives its sixteen bytes. Each is one output function, and none of them is the `Expr::Cast` arm being absent.",
            "pg19_cast_matrix.txt:17",
        ),
        (
            "SELECT (c_bpchar)::xml FROM castprobe",
            "**#43 group 4 -- both answer and the values differ.** Eight pairs, and they are renderings rather than conversions: `inet` loses its prefix length (`10.0.0.1` for `10.0.0.1/32`), `boolean` renders `t` where a real server writes `true` into a character type, `uuid -> bytea` gives the *text* of the uuid where a real server gives its sixteen bytes. Each is one output function, and none of them is the `Expr::Cast` arm being absent.",
            "pg19_cast_matrix.txt:47",
        ),
        (
            "SELECT (c_inet)::bpchar FROM castprobe",
            "**#43 group 4 -- both answer and the values differ.** Eight pairs, and they are renderings rather than conversions: `inet` loses its prefix length (`10.0.0.1` for `10.0.0.1/32`), `boolean` renders `t` where a real server writes `true` into a character type, `uuid -> bytea` gives the *text* of the uuid where a real server gives its sixteen bytes. Each is one output function, and none of them is the `Expr::Cast` arm being absent.",
            "pg19_cast_matrix.txt:101",
        ),
        (
            "SELECT (c_inet)::text FROM castprobe",
            "**#43 group 4 -- both answer and the values differ.** Eight pairs, and they are renderings rather than conversions: `inet` loses its prefix length (`10.0.0.1` for `10.0.0.1/32`), `boolean` renders `t` where a real server writes `true` into a character type, `uuid -> bytea` gives the *text* of the uuid where a real server gives its sixteen bytes. Each is one output function, and none of them is the `Expr::Cast` arm being absent.",
            "pg19_cast_matrix.txt:107",
        ),
        (
            "SELECT (c_inet)::varchar FROM castprobe",
            "**#43 group 4 -- both answer and the values differ.** Eight pairs, and they are renderings rather than conversions: `inet` loses its prefix length (`10.0.0.1` for `10.0.0.1/32`), `boolean` renders `t` where a real server writes `true` into a character type, `uuid -> bytea` gives the *text* of the uuid where a real server gives its sixteen bytes. Each is one output function, and none of them is the `Expr::Cast` arm being absent.",
            "pg19_cast_matrix.txt:110",
        ),
        (
            "SELECT (c_oid)::regclass FROM castprobe",
            "**#43 group 4 -- both answer and the values differ.** Eight pairs, and they are renderings rather than conversions: `inet` loses its prefix length (`10.0.0.1` for `10.0.0.1/32`), `boolean` renders `t` where a real server writes `true` into a character type, `uuid -> bytea` gives the *text* of the uuid where a real server gives its sixteen bytes. Each is one output function, and none of them is the `Expr::Cast` arm being absent.",
            "pg19_cast_matrix.txt:164",
        ),
        (
            "SELECT (c_uuid)::bytea FROM castprobe",
            "**#43 group 4 -- both answer and the values differ.** Eight pairs, and they are renderings rather than conversions: `inet` loses its prefix length (`10.0.0.1` for `10.0.0.1/32`), `boolean` renders `t` where a real server writes `true` into a character type, `uuid -> bytea` gives the *text* of the uuid where a real server gives its sixteen bytes. Each is one output function, and none of them is the `Expr::Cast` arm being absent.",
            "pg19_cast_matrix.txt:248",
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
