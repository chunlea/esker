//! A domain in a schema, and what `CASCADE` takes with it.
//!
//! Four defects in the [ADR 0065](../../../docs/adr/0065-a-domain-is-a-name-and-a-constraint-over-a-base-type.md)
//! unit, found by the harness's provenance probe rather than by its own corpus — which is the point
//! worth keeping. Each was invisible to the capture that shipped with the feature:
//!
//! * **A schema-qualified domain could be created and never referenced.** `CREATE DOMAIN r.d`
//!   succeeded and `CREATE TABLE r.t (v r.d)` was `0A000 the type r.d is not supported`, about a
//!   type the catalog held. Column types were read one part at a time.
//! * **A `CHECK` written `check (value > 0)` made its column unusable.** `VALUE` was renamed in the
//!   stored *text*, case-sensitively, so the lowercase spelling left the name alone and **every**
//!   insert — the satisfying value and the violating one alike — failed with
//!   `column "value" does not exist`. The capture only ever wrote it uppercase.
//! * **`DROP SCHEMA … CASCADE` orphaned the schema's domains.** A type is not a name record, so
//!   nothing in `DROP SCHEMA` could see one: the schema went, the domain survived with a record key
//!   naming a schema that no longer existed, and it then showed in `pg_type` under `public`, did not
//!   resolve by name, and could not be dropped. **An orphan a later `DROP` cannot remove** is the
//!   shape that cost run 55 its last hundred files.
//! * **`DROP DOMAIN … CASCADE` did not honour `CASCADE`** — it answered with the hint telling you to
//!   write the clause you had just written.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own schema, domains and tables.
const CORPUS_FIXTURE: &[&str] = &[];

const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT 'r', conname, contype FROM pg_constraint WHERE contypid = 'ds_ci'::regtype",
        "**The row is there; the `::regtype` in the comparison is not resolved to an oid.** \
         `contypid` now carries the domain and the constraint has its row — `SELECT conname FROM \
         pg_constraint WHERE contypid <> 0` finds it — but a bare `'ds_ci'::regtype` lowers to the \
         type's *name*, and comparing that against a `bigint` column is `22P02`.\n\nThis is the \
         limitation `tests/regtype_user.rs` already declares in the same words, over
         `WHERE enumtypid = 'mood'::regtype`: the oid form is chosen when `::oid` is written, and a \
         comparison is a position that wants the oid without saying so. Deciding it from the \
         column being compared against is its own unit and would close both.",

"pg19_domain_schema.txt:33",
)],
};

#[test]
fn every_domain_schema_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_domain_schema.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 25,
        "only {checked} statements ran; the corpus did not load"
    );
}
