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
    types: &[
        "SELECT 'r', column_name, data_type, udt_name FROM information_schema.columns WHERE \
         table_name = 'postgresql_enums' ORDER BY ordinal_position",
        "SELECT 'r', column_name, column_default FROM information_schema.columns WHERE table_name \
         = 'postgresql_enums' AND column_name = 'good_mood'",
        // **`ActiveRecord`'s own `enum_types()` query, and the rows agree now**: `pg_enum` is a
        // view over the type records, so the labels come back in declaration order with
        // `enumsortorder` 1, 2, 3. What is left is the same trade one line up — `typname` and
        // `nspname` are `name` on a real server and `text` here, so `array_agg` of the labels is
        // `name[]` there and `text[]` here, with the same three strings in it.
        "SELECT 'r', type.typname AS name, n.nspname AS schema, array_agg(enum.enumlabel ORDER BY \
         enum.enumsortorder) AS value FROM pg_enum AS enum JOIN pg_type AS type ON (type.oid = \
         enum.enumtypid) JOIN pg_namespace n ON type.typnamespace = n.oid WHERE n.nspname = ANY \
         (current_schemas(false)) GROUP BY type.OID, n.nspname, type.typname",
        // **The four `pg_enum` probes, and every row of every one of them agrees.** `typname`
        // and `enumlabel` are `name` on a real server and `text` here — so `array_agg` of the
        // labels is `name[]` there and `text[]` here — and `pg_typeof` answers a `regtype` there
        // and `text` here, which is the trade `'x'::regtype` already makes. `enumsortorder` is a
        // `real` on **both**, which is the one column of this view that had to be got right
        // rather than traded: it is not the label's index.
        "SELECT 'r', t.typname, e.enumlabel, e.enumsortorder FROM pg_enum e JOIN pg_type t ON \
         t.oid = e.enumtypid WHERE t.typname IN ('mood','tense','emptymood') ORDER BY t.typname, \
         e.enumsortorder",
        "SELECT 'r', t.typname, count(*) FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid \
         WHERE t.typname IN ('mood','tense','emptymood') GROUP BY t.typname ORDER BY t.typname",
        "SELECT 'r', e.enumsortorder, pg_typeof(e.enumsortorder) FROM pg_enum e JOIN pg_type t ON \
         t.oid = e.enumtypid WHERE t.typname = 'mood' ORDER BY e.enumsortorder",
        "SELECT 'r', t.typname, array_agg(e.enumlabel ORDER BY e.enumsortorder) FROM pg_enum e \
         JOIN pg_type t ON t.oid = e.enumtypid WHERE t.typname IN ('mood','tense') GROUP BY \
         t.typname ORDER BY t.typname",
    ],
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
        // **A cast *to* a user-defined type is not built**, which is one gap wearing four
        // statements. `'happy'::mood` needs the catalog at a point where a cast is lowered without
        // one — the same road `CREATE TABLE t (c mood)` took, and the same answer: the name is
        // carried and the executor resolves it. Nothing here is approximated in the meantime; each
        // is `0A000` naming the type, and the two that a real server *refuses* refuse here too, so
        // the divergence is the code and not the outcome. It is the next unit, with `::regtype`
        // below, because both are "a user type is a name you can write in an expression".
        // **`||` over text is unbuilt for every type**, which is where this statement stops — the
        // cast in front of it is right, and `'happy'::mood::text` on the line above proves it.
        // `tests/citext.rs` declares the same operator for the same reason.
        (
            "SELECT 'r', ('happy'::mood)::text || '!'",
            "|| over text is not built for any type",
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
        // **`'mood'::regtype` resolves against this node's own type names and not the catalog.**
        // The lowering answers `42704` for a name `crate::value::named_type` does not have, which
        // is the right answer for a typo and the wrong one for a type somebody declared. It needs
        // the statement-level pass `::regclass` already has (`Executor::resolve_regclass`), and it
        // is the reason the line below has no rows to find.
        (
            "SELECT 'r', enumlabel, enumsortorder FROM pg_enum WHERE enumtypid = \
             'mood'::regtype ORDER BY enumsortorder",
            "'x'::regtype does not resolve a user-defined type's name to its oid",
            "UNMEASURED",
        ),
        // **The standing constant-width divergence, in three sentences that are otherwise
        // identical**: a bare integer constant is `int8` here and `int4` there, so the type this
        // node names in the refusal is `bigint` where a real server says `integer`. Same SQLSTATE,
        // same shape, same HINT — one word differs, and it is the word `tests/unknown_literal.rs`
        // declares everywhere else.
        (
            "INSERT INTO postgresql_enums (current_mood) VALUES (1)",
            "a bare integer constant is int8 here and int4 there",
            "UNMEASURED",
        ),
        (
            "UPDATE postgresql_enums SET current_mood = 1 WHERE id = 1",
            "a bare integer constant is int8 here and int4 there",
            "UNMEASURED",
        ),
        (
            "SELECT 'r', id FROM postgresql_enums WHERE current_mood = 1",
            "a bare integer constant is int8 here and int4 there",
            "UNMEASURED",
        ),
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
