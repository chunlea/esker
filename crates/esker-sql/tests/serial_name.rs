//! **How a serial column's sequence is named**, against PostgreSQL 19beta1.
//!
//! `adapters/postgresql/serial_test.rb`'s two naming tests. `CollidedSequenceNameTest` makes two
//! columns whose naive `<table>_<column>_seq` is the same string, and
//! `LongerSequenceNameDetectionTest` uses a table name of exactly `NAMEDATALEN - 1` characters so
//! that the name cannot fit at all.
//!
//! Both are one function: `plan::ddl::make_object_name`, PostgreSQL's `makeObjectName`, which
//! makes a derived name fit by taking characters off the **longer of the table and the column**
//! until it does — so the `_seq` on the end is never what gives way. What was here before joined
//! the parts and clipped the result, which put the truncation in the wrong place and made all
//! three of the long table's sequences the same string.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus makes its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **Empty.** `seqtypid::regtype` was the last of them and is a `regtype` here now — a
    // `::regtype` over a column resolves the direction it is cast from, where it used to answer
    // the name as a `text`. `pg_class.relname` is a `name` (ADR 0084) and `relkind` a `"char"`
    // (ADR 0095), which emptied the rest. **Every value always agreed** — `integer` for a
    // `serial`'s sequence and `bigint` for a `bigserial`'s, and the names the collision and the
    // truncation produced.
    types: &[],
    answers: &[
        (
            "SELECT 'r', count(*) AS columns_that_OWN_a_sequence FROM pg_attribute a WHERE \
             a.attrelid = 'postgresql_serials'::regclass AND a.attnum > 0 AND \
             pg_get_serial_sequence('postgresql_serials', a.attname) IS NOT NULL",
            "**A plan-shape divergence, and it became visible the day a NULL stopped hiding it.** \
             PostgreSQL answers `2`; this node raises `42703 column \"oid\" of relation \
             \"postgresql_serials\" does not exist`. Neither server short-circuits `AND` — \
             measured, `WHERE a = 2 AND 1/0 = 1` raises on both — so the difference is not the \
             operator but which rows reach it: a real server's index scan on `pg_attribute` \
             applies `attrelid = 'postgresql_serials'::regclass` first and never calls the \
             function for another relation's row, while this node evaluates the whole predicate \
             over every row of a computed view, including catalog relations that have a column \
             called `oid`. Ordering the conjuncts cheapest-first here would close this statement \
             and open a worse hole: `WHERE a = 2 AND 1/0 = 1` would then answer where a real \
             server raises, which is the wrong-answer shape. Until this crate has a planner that \
             restricts a scan, the honest answer is the error. **This statement passed before, \
             and passed for the wrong reason**: `pg_get_serial_sequence` answered NULL for a \
             column that is not there, where PostgreSQL raises, so the `IS NOT NULL` filtered the \
             row and two bugs cancelled. Closing the function's own gap — which is what \
             `postgresql_adapter_test`'s `test_serial_sequence` and \
             `test_default_sequence_name_bad_table` need, because `default_sequence_name` rescues \
             the exception to build its fallback — uncancelled them.",
            "pg19_serial.txt:44",
        ),
        // **`last_value` counts the block that was reserved, not the rows that were inserted.**
        // Two `INSERT`s give `2` on a real server and `32` here, because a sequence hands out
        // `catalog::SEQUENCE_BATCH` at a time to the node that asks
        // ([ADR 0072](../../../docs/adr/0072-a-sequence-block-belongs-to-the-node-not-to-the-connection.md)),
        // and `last_value` reports how far the counter has been drawn down rather than how far it
        // has been used. `is_called` agrees, and so do **the ids themselves** — the two rows above
        // this one are `1` and `2` on both servers, which is what an `INSERT ... RETURNING` and a
        // fixture load depend on.
        //
        // A real server has the same gap for the same reason whenever `CACHE n` is set above 1;
        // what differs is that PostgreSQL defaults to `CACHE 1` and this node does not. Closing it
        // means reporting the block's *used* watermark rather than its reserved one, which is a
        // second number the allocator does not keep today — and `reset_pk_sequence!`, the caller
        // that reads this, sets the value rather than reads it.
        (
            "SELECT 'r', last_value, is_called FROM foo_id_seq",
            "a sequence block is reserved 32 at a time (ADR 0072), and last_value reports the reservation",
            "pg19_serial.txt:73",
        ),
    ],
};

#[test]
fn every_serial_name_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_serial.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
