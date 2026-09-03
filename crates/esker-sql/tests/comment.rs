//! `COMMENT ON` and `obj_description`/`col_description`, against PostgreSQL 19beta1.
//!
//! `ActiveRecord` writes a table's and a column's comment into its schema dump and reads them back
//! with the two functions — boot statements 29 and 32. Until this unit both functions answered
//! NULL for everything, which was right only because nothing could ever set a comment.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // `pg_attribute.attname` is the 64-byte `name` type on a real server and `text` here, which
    // compares identically and is the standing choice every catalog view in this crate makes. The
    // rows agree; only the declared type differs.
    types: &[
        "SELECT 'r', a.attname, col_description(a.attrelid, a.attnum) FROM pg_attribute a WHERE \
         a.attrelid = 'cm3'::regclass AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
        "SELECT 'r', i.relname, pg_catalog.obj_description(i.oid, 'pg_class') AS comment FROM \
         pg_class t INNER JOIN pg_index d ON t.oid = d.indrelid INNER JOIN pg_class i ON \
         d.indexrelid = i.oid WHERE i.relkind IN ('i','I') AND d.indisprimary = 'f' AND t.relname \
         = 'cm' ORDER BY i.relname",
    ],
    answers: &[
        // **`ALTER TABLE … RENAME` and `DROP COLUMN` are not implemented**, so the three
        // statements below are refused and the five that read `cm2` afterwards cannot find it.
        // They are kept rather than deleted because of what they measure: a comment is keyed by
        // the object and not by its name, so it survives a rename and dies with a dropped column.
        // Here that falls out of where the comment lives — a field of the record the rename
        // rewrites — so the day those statements land, these entries will start agreeing and this
        // test will say so.
        (
            "ALTER TABLE cm RENAME TO cm2",
            "ALTER TABLE ... RENAME TO is not implemented",
        ),
        (
            "ALTER TABLE cm2 RENAME COLUMN b TO c",
            "ALTER TABLE ... RENAME COLUMN is not implemented",
        ),
        (
            "ALTER TABLE cm2 DROP COLUMN c",
            "ALTER TABLE ... DROP COLUMN is not implemented",
        ),
        (
            "SELECT 'r', col_description('cm2'::regclass, 3)",
            "the rename that would have made `cm2` is refused above",
        ),
        (
            "SELECT 'r', obj_description('cm2'::regclass, 'pg_class')",
            "the rename that would have made `cm2` is refused above",
        ),
        (
            "SELECT 'r', obj_description('cm2'::regclass, 'pg_type')",
            "the rename that would have made `cm2` is refused above",
        ),
        // **`pg_description` is not a view here.** The comments are fields of the table record
        // rather than rows of a catalog table (ADR 0049), and nothing this node is for reads them
        // that way: `ActiveRecord` uses the two functions. A view over them is a small unit of its
        // own, and these two lines are the capture it would be written against — including the
        // shape, `(objoid, objsubid, description)` with the table comment at `objsubid` 0.
        (
            "SELECT 'r', objoid::regclass::text, objsubid, description FROM pg_description WHERE \
             objoid = 'cm3'::regclass ORDER BY objsubid",
            "pg_description is not a view here; the comments are fields of the table record",
        ),
        (
            "SELECT 'r', count(*) FROM pg_description WHERE description = 'qualified table comment'",
            "pg_description is not a view here; the comments are fields of the table record",
        ),
    ],
};

#[test]
fn every_comment_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_comment.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 55,
        "only {checked} statements ran; the corpus did not load"
    );
}
