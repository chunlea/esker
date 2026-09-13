//! **The refusals the user decided to keep**, each with the text it answers today.
//!
//! These are not gaps waiting for a unit: they are places where this node deliberately answers
//! differently from PostgreSQL, by a ruling recorded in `docs/plans/phase-9-rails.md` or in an
//! accepted ADR. A declared divergence with no test is a sentence in a document that nothing
//! checks — so each one is pinned here, and if the refusal ever changes these go red and the
//! record gets read again (ADR 0031, rule 2).
//!
//! **PostgreSQL accepts every statement below.** Measured on 19beta1 as the superuser the suite
//! connects as:
//!
//! ```text
//! DELETE FROM pg_depend WHERE objid = 0                                  -> DELETE 0
//! UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE …       -> UPDATE 0
//! SELECT 'pg_class'::regclass::integer                                   -> 1259
//! ```
//!
//! So these are divergences in the strict sense — a real server answers and this one refuses —
//! and each refusal is the *answer the user chose*, not an accident of the parser.
//!
//! **Two sources, and they are not the same kind of ruling.** The first two come from the Rails
//! plan, where the user weighed a suite's failures against what the feature would cost. The
//! fourth comes from an **accepted ADR's own consequences**: it was decided once, as a
//! consequence of keeping `u64` relation ids, and the rows below are that decision arriving at a
//! second surface two days later. Both are the user's; only the second is already written down
//! somewhere a reader will find it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

// **The "DO minimal" ruling was reversed by the user on 2026-09-13** (ADR 0113): a `DO` body in
// the PL/pgSQL subset runs, and the two tests that pinned it as refused moved to where a running
// block is tested — `tests/plpgsql_do.rs::a_loop_that_executes_a_field_runs` holds the loop this
// file refused, and `tests/do_block.rs` holds `create_enum`'s guard.

/// **A write to a system catalog: `42501`, whatever the writer's rights.**
///
/// `postgresql_adapter_test.rb`'s `test_pk_and_sequence_for_with_collision_pg_class_oid` deletes
/// from `pg_depend` to detach a sequence from its column — a deliberate corruption of the catalog,
/// to see whether the adapter still answers. A real server lets a superuser do it; this node's
/// catalog is computed from records rather than stored as tables, so there is no row to delete and
/// no honest way to pretend there is. Refusing is the ADR 0031 call: a wrong answer where a
/// refusal is available is the worse of the two.
///
/// The same guard is what `check_all_foreign_keys_valid!`'s `DO` block reaches now that it runs:
/// `UPDATE pg_catalog.pg_constraint SET convalidated = false` is refused for this reason and not for
/// the `DO` one (ADR 0113, `docs/plans/plpgsql-subset.md` §6).
#[test]
fn a_write_to_a_system_catalog_is_refused() {
    let mut node = parity::Node::new(&[]);
    for (sql, relation) in [
        ("DELETE FROM pg_depend WHERE objid = 1", "pg_depend"),
        (
            "UPDATE pg_catalog.pg_constraint SET convalidated = false WHERE conname = 'x'",
            "pg_constraint",
        ),
        ("INSERT INTO pg_class (relname) VALUES ('x')", "pg_class"),
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            format!("!42501 permission denied: \"{relation}\" is a system catalog"),
            "{sql}"
        );
    }
}

/// **A relation's `regclass` does not fit an `oid`, and that is [ADR 0097]'s own boundary.**
///
/// Relation ids here are `u64` — a catalog view's id sits near `i64::MAX` and a primary key's
/// index is derived from its table's — and an `oid` is four bytes. So a cast that narrows one is
/// `22003`, and the ADR says so in its consequences, in the same words this node answers in:
///
/// > **A real relation's `regclass` cannot be cast to `oid`, and so `min`/`max` over one cannot
/// > answer.** This is the decision's own boundary rather than a defect: relation ids are `u64`
/// > and an `oid` is four bytes, so `'pg_class'::regclass::oid` on a live relation is
/// > `value 9223372036854774786 is out of range for type oid`. […] Ruled 2026-09-09 not to fix:
/// > narrowing the id would be the allocation change the first consequence above describes, and
/// > answering a truncated oid would be a wrong value under a right type.
///
/// The four rows here are that ruling reaching a second surface: the cast matrix put every ordered
/// pair of the wire v3 probe list's types to both servers, and `regclass -> integer|oid` and their
/// arrays were the last four rows where this node refuses and 19beta1 answers
/// (`tests/captures/pg19_cast_matrix.txt`). **Ruled again 2026-09-11, on the ADR's text, to
/// declare rather than to fix** — the alternative is a truncated oid, which is the wrong value
/// under the right type that the ADR rejected.
///
/// **What still answers is the point of the pin**: the id is a `bigint` and a `regclass` prints as
/// a name, so the two spellings a client actually uses are unaffected. Only the narrowing refuses.
///
/// [ADR 0097]: ../../../docs/adr/0097-an-oid-is-four-bytes-and-a-derived-one-is-not.md
#[test]
fn a_relations_regclass_does_not_narrow_to_an_oid() {
    let mut node = parity::Node::new(&[]);
    // 19beta1 answers `1259` to the first two and `{1259}` to the arrays, with `pg_class`'s own
    // oid — measured, and the number is a real server's rather than this node's.
    for (sql, refusal) in [
        (
            "SELECT 'pg_class'::regclass::integer",
            "!22003 integer out of range",
        ),
        (
            "SELECT 'pg_class'::regclass::oid",
            "!22003 value \"9223372036854774786\" is out of range for type oid",
        ),
        (
            "SELECT '{pg_class}'::regclass[]::integer[]",
            "!22003 integer out of range",
        ),
        (
            "SELECT '{pg_class}'::regclass[]::oid[]",
            "!22003 value \"9223372036854774786\" is out of range for type oid",
        ),
    ] {
        assert_eq!(node.answer(sql).to_string(), refusal, "{sql}");
    }
    // **The same four through a bound parameter, and the same sentence.** A declared divergence
    // that refuses with one code as a literal and another through a bind is two boundaries
    // wearing one ruling: `$1::regclass::oid` was `0A000 the cast $1::oid is not supported` —
    // a refusal *by name*, from the lowering, which says nothing about the id at all. Measured and
    // fixed 2026-09-11 on the coordinator's ruling: one boundary, one sentence, and the sentence
    // is the ADR's.
    for (sql, refusal) in [
        (
            "SELECT $1::regclass::integer",
            "!22003 integer out of range",
        ),
        (
            "SELECT $1::regclass::oid",
            "!22003 value \"9223372036854774786\" is out of range for type oid",
        ),
    ] {
        assert_eq!(bound(sql, "pg_class"), refusal, "{sql}");
    }
    for (sql, refusal) in [
        (
            "SELECT $1::regclass[]::integer[]",
            "!22003 integer out of range",
        ),
        (
            "SELECT $1::regclass[]::oid[]",
            "!22003 value \"9223372036854774786\" is out of range for type oid",
        ),
    ] {
        assert_eq!(bound(sql, "{\"pg_class\"}"), refusal, "{sql}");
    }

    // **And the two spellings that are not a narrowing still answer**, which is what keeps this a
    // boundary rather than a hole: the id is a `bigint` and the name is what a `regclass` prints.
    assert_eq!(
        node.rows("SELECT ('pg_class'::regclass)::bigint"),
        vec![vec!["9223372036854774786"]]
    );
    assert_eq!(
        node.rows("SELECT ('pg_class'::regclass)::text"),
        vec![vec!["pg_class"]]
    );
}

/// One statement through `Parse`/`Describe`/`Bind`/`Execute` with **no declared type** — the shape
/// a driver sends — answering `ok` or the `!SQLSTATE message` of whichever step refused.
///
/// Here rather than in the harness because two tests in this repository drive the extended
/// protocol and they want different things out of it: this one wants the sentence, and
/// `bind_infers_over_the_wire.rs` wants the wire's type OID.
fn bound(sql: &str, value: &str) -> String {
    use esker_sql::pgwire::message::{Frontend, Target};

    let mut node = parity::Node::new(&[]);
    let mut session = esker_sql::pgwire::session::Session::new();
    let mut send = |message: &Frontend, node: &mut parity::Node| {
        let mut out = Vec::new();
        session.handle(message, &mut node.executor, &mut out);
        let text = String::from_utf8_lossy(&out).to_string();
        let parts: Vec<&str> = text.split('\u{0}').collect();
        if parts
            .iter()
            .any(|part| *part == "SERROR" || *part == "VERROR")
        {
            let code = parts
                .iter()
                .find(|part| part.starts_with('C') && part.len() == 6)
                .map_or("?????", |part| &part[1..]);
            let message = parts
                .iter()
                .find(|part| part.starts_with('M'))
                .map_or("", |part| &part[1..]);
            return format!("!{code} {message}");
        }
        "ok".to_owned()
    };
    for message in [
        Frontend::Parse {
            statement: "d".to_owned(),
            sql: sql.to_owned(),
            param_types: Vec::new(),
        },
        Frontend::Describe {
            target: Target::Statement,
            name: "d".to_owned(),
        },
        Frontend::Bind {
            portal: "e".to_owned(),
            statement: "d".to_owned(),
            param_formats: Vec::new(),
            params: vec![Some(value.as_bytes().to_vec())],
            result_formats: Vec::new(),
        },
        Frontend::Execute {
            portal: "e".to_owned(),
            max_rows: 0,
        },
    ] {
        let answer = send(&message, &mut node);
        if answer.starts_with('!') {
            return answer;
        }
    }
    "ok".to_owned()
}
