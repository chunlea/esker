//! An **enum as a column value** — stored, read back, and ordered by — against PostgreSQL 19beta1.
//!
//! ADR 0050's first unit. The label's **ordinal** is what is stored (`pg_enum.enumsortorder` is
//! PostgreSQL's own sort key too) and `ColumnDef::user_type` carries the type's identity, so
//! ordering, `=`, grouping and indexing are all the ordinal's and the label is rendered through
//! the catalog on the way out.
//!
//! The fact the unit exists for: **declaration order is sort order and it is not the alphabet.**
//! `mood` is `('sad','ok','happy')`, so `'happy'::mood < 'ok'::mood` is `f`. A node that stored
//! the label as text would answer `t` and would sort `happy, ok, sad` — plausible in isolation,
//! wrong against the capture, and invisible to any test that only round-trips one value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own type and tables.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // **The standing `name`-is-`text` trade**, and `character varying` for `data_type`: the
    // `information_schema` views declare their own domain types on a real server and `text` here,
    // and the rows are identical. Nothing to do with enums — the `USER-DEFINED` and the `udt_name`
    // in those rows are what this unit is about, and both agree.
    types: &[],
    answers: &[
        // **`pg_type` holds this node's own types and the tenant's, and PostgreSQL's built-in
        // ranges and domains are neither.** `daterange` is a *value* here (`crate::value::range`)
        // with no `pg_type` row, and `cardinal_number` and friends are `information_schema`'s
        // domains, which this node has as columns rather than as types. Not about enums at all —
        // `mood` is in the answer and is right — and it closes when those two get rows of their
        // own, which is the range unit ADR 0050 puts second.
        (
            "SELECT 'r', typname, typtype FROM pg_type WHERE typtype IN ('r','e','d') ORDER BY \
             typname",
            "no pg_type rows for the built-in ranges and information_schema's domains",
            "UNMEASURED",
        ),
        // **A name that is nobody's type is `42704` there and `0A000` here**, and this is the one
        // place the pass cannot do better: after the catalog says no, the name is either a type
        // PostgreSQL has and this node has not built — `money`, `tsvector` — or a name that is no
        // type at all, and nothing here can tell them apart. Naming the type in a `0A000` is the
        // honest half; claiming it does not exist would be wrong for the first case, which is the
        // commoner one.
        (
            "SELECT 'r', 'happy'::nosuchtype",
            "a name the catalog does not have is either an unbuilt type or no type; 0A000 says \
             which is not known",
            "UNMEASURED",
        ),
        // **An enum value in a relation that is not a table has no column to carry its type.**
        // ADR 0050 puts the identity on the `ColumnDef` and ADR 0053 keeps the label only where
        // the cast *is* the projection; a `VALUES` list is neither, so these two print the
        // ordinal. Every ordering in them is right — `min`/`max` are the first and last
        // **declared** and `ORDER BY` sorts `sad, ok, happy` — which is the half that would be
        // hard; what is missing is the rendering. The fix is the synthetic `TableDef` of ADR 0048
        // carrying `user_type`, which is where a `VALUES` list's column types already come from,
        // and it closes both lines at once.
        (
            "SELECT 'r', min(v), max(v) FROM (VALUES ('sad'::mood), ('happy'::mood), \
             ('ok'::mood)) t(v)",
            "a VALUES list's synthetic TableDef carries no user type, so the ordinal prints",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', v FROM (VALUES ('happy'::mood), ('sad'::mood), ('ok'::mood)) t(v) ORDER \
             BY v",
            "a VALUES list's synthetic TableDef carries no user type, so the ordinal prints",
            "UNMEASURED",
        ),
        // **The standing constant-width divergence used to be here, in two sentences that are
        // otherwise identical**: a bare integer constant's *datum* is an `int8`, so the type this
        // node named in the refusal was `bigint` where a real server says `integer` — same
        // SQLSTATE, same shape, same HINT, one word. **Closed by #73**, which made the write path
        // ask the literal rather than the datum, and this harness is what said so: it refuses to
        // pass while a row is declared a divergence and agrees. The two rows now match, so the
        // entries are gone rather than struck.
        // `DO $$ … $$` was here, refused by name. This entry predicted its own unit would be
        // "a large one — it is a language, not a statement"; the measurement said otherwise.
        // All 36 `DO` statements the suite sends are `create_enum`'s one template, so the block
        // runs and the entry is deleted (ADR 0031 rule 2) — `do_block.rs` is where it lives.
        // Note the spacing: this line writes `EXISTS (SELECT`, the suite writes `EXISTS ( SELECT`,
        // and the recogniser tokenises rather than matching text, so both are the same template.
    ],
};

#[test]
fn every_enum_value_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_enum_value.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 80,
        "only {checked} statements ran; the corpus did not load"
    );
}
