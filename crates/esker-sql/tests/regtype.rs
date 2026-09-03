//! `'x'::regtype::oid` against PostgreSQL 19beta1's own answers — the cast the scoreboard named.
//!
//! Three scoreboard runs stopped at rung 2 on one statement. ADR 0033 scoped this cast with the
//! type surface "because neither moves the ladder alone", tier 1 shipped without it, and run 3
//! measured exactly the outcome the ADR predicted: six types landed and the ladder moved by zero
//! rungs. `docs/bench/rails-scoreboard.md` run 3 has the numbers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: every statement is a constant expression.
const CORPUS_FIXTURE: &[&str] = &[];

/// One of `DIVERGENCES`' reasons: a type this node does not have.
///
/// `'x'::regtype` answering an OID for a type that cannot be stored or sent would hand a client a
/// number this node can do nothing with — the same argument `pg_catalog.rs` makes for keeping
/// `pg_type` short. A name it does not have is a name that does not exist here, and each closes
/// when its type lands: `interval` 1186, `timetz` 1266, `uuid` 2950, the array types, and the
/// three system types `oid` 26, `name` 19 and `regtype` 2206.
const NO_SUCH_TYPE: &str = "A type this node does not have, so its name does not resolve: `42704` \
     rather than an OID a client could do nothing with. Each closes when its type does.";

/// `float(p)` picks a *different type* by its precision.
const FLOAT_PRECISION: &str = "**A precision on `float` selects the type**: `float(24)` is `real` \
     and `float(25)` is `double precision`, and the bounds are their own `22023`s — `at least 1 \
     bit`, `less than 54 bits`. This node takes no typmod on `float` at all (`value::takes_typmod` \
     says so, and the two float widths are distinct types here rather than one parameterised \
     one), so the whole spelling is `42704`. The only typmod in PostgreSQL that changes which \
     type you get, and it needs the float pair to be modelled as one type to close.";

/// `pg_typeof` is not implemented for any type.
const PG_TYPEOF: &str = "`pg_typeof` is not implemented at all, so this is `0A000` naming the \
     function rather than a wrong type — the honest answer under contract C2. Several of these \
     lines would *prove* the `regtype`-is-`text` divergence above if the function existed.";

/// Names with quoting or a schema on them.
const NAME_SYNTAX: &str = "**A type name is parsed, not compared.** PostgreSQL reads \
     `'\"int4\"'::regtype` and `'pg_catalog.int4'::regtype` with its own name grammar — quoting \
     suppresses the alias folding, so `'\"int4\"'` is 23 and `'\"integer\"'` is `42704`, and a \
     schema qualifier is stripped when it is `pg_catalog`. This node lower-cases and looks the \
     whole string up, so every one of these is `42704`. It closes when the name is tokenised \
     rather than matched.";

/// The other direction: an OID back to a name.
const REVERSE: &str = "**The reverse direction is not implemented**: `23::regtype` is `integer` \
     on a real server and an unknown number prints as itself (`999999::regtype` is `999999`, not \
     an error). This node has no cast from a number to a `regtype` at all, so it is `0A000` \
     naming the cast. The forward direction is what the scoreboard needed and what ADR 0033 \
     scoped; this half has had no caller.";

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // A real server's `regtype` is a type of its own — four bytes holding an OID that print as
        // the type's name. This node has no `regtype`, so `'x'::regtype` answers the **name**, as
        // text: the value is byte-identical and only `RowDescription`'s OID differs, `text` where
        // a real server says `regtype`. The same trade `pg_catalog.rs` makes for `pg_type.oid`.
        "SELECT 'integer'::regtype",
        "SELECT 'int4'::regtype",
        "SELECT 'varchar'::regtype",
        // `::oid` answers a `bigint` here, for the same reason: this node has no four-byte `oid`
        // type either, and the *value* is what a client reads.
        "SELECT 'integer'::regtype::oid",
        "SELECT 'int4'::regtype::oid",
        "SELECT 'bigint'::regtype::oid",
        "SELECT 'int8'::regtype::oid",
        "SELECT 'smallint'::regtype::oid",
        "SELECT 'character varying'::regtype::oid",
        "SELECT 'character varying(255)'::regtype::oid",
        "SELECT 'varchar'::regtype::oid",
        "SELECT 'text'::regtype::oid",
        "SELECT 'character(3)'::regtype::oid",
        "SELECT 'bpchar'::regtype::oid",
        "SELECT 'timestamp(6) without time zone'::regtype::oid",
        "SELECT 'timestamp'::regtype::oid",
        "SELECT 'timestamp with time zone'::regtype::oid",
        "SELECT 'boolean'::regtype::oid",
        "SELECT 'bytea'::regtype::oid",
        "SELECT 'real'::regtype::oid",
        "SELECT 'double precision'::regtype::oid",
        "SELECT 'INTEGER'::regtype::oid",
        "SELECT '23'::oid",
        // **Array names resolve now**, so these answer the right OID and declare `bigint` where a
        // real server declares `oid` — the same one trade as every line above, and no longer the
        // "no such type" they were listed under. There is still no array *storage*: nothing can
        // create a column of one, and only the name is being asked for here.
        "SELECT 'integer[]'::regtype::oid, 'int4[]'::regtype::oid, 'text[]'::regtype::oid",
        "SELECT 'integer[][]'::regtype::oid, 'integer[3]'::regtype::oid",
        "SELECT '_int4'::regtype::oid",
        "SELECT 'int4[]'::regtype::oid",
        // Every remaining spelling the corpus asks for, answering the right OID and declaring
        // `bigint` where a real server declares `oid` — the same one trade as the lines above,
        // and the reason this list is long rather than deep. `numeric`, `decimal`, `date` and
        // `time` are in it because they resolve now: `value::type_by_name` derives its names from
        // `ColumnType::ALL`, so a type that exists is a name that resolves.
        "SELECT 'int'::regtype::oid, 'int4'::regtype::oid",
        "SELECT 'character varying'::regtype::oid, 'varchar'::regtype::oid",
        "SELECT 'character'::regtype::oid, 'char'::regtype::oid, 'bpchar'::regtype::oid",
        "SELECT 'timestamp'::regtype::oid, 'timestamp without time zone'::regtype::oid",
        "SELECT 'timestamptz'::regtype::oid, 'timestamp with time zone'::regtype::oid",
        "SELECT 'bool'::regtype::oid, 'boolean'::regtype::oid",
        "SELECT 'text'::regtype::oid, 'bytea'::regtype::oid",
        "SELECT 'int8'::regtype::oid, 'bigint'::regtype::oid",
        "SELECT 'int2'::regtype::oid, 'smallint'::regtype::oid",
        "SELECT 'float4'::regtype::oid, 'real'::regtype::oid",
        "SELECT 'float8'::regtype::oid, 'double precision'::regtype::oid",
        "SELECT 'numeric'::regtype::oid, 'decimal'::regtype::oid",
        "SELECT 'character varying(1024)'::regtype::oid",
        "SELECT 'character varying(1)'::regtype::oid",
        "SELECT 'numeric(10,2)'::regtype::oid",
        "SELECT 'timestamp(6)'::regtype::oid",
        "SELECT 'timestamp(9)'::regtype::oid",
        "SELECT 'INTEGER'::regtype::oid, 'Integer'::regtype::oid",
        "SELECT '  integer  '::regtype::oid",
        "SELECT 'int4 '::regtype::oid, ' int4'::regtype::oid",
        "SELECT 'character varying(1024)'::regtype",
        "SELECT 'time'::regtype, 'timestamptz'::regtype",
    ],
    answers: &[
        (
            "SELECT pg_typeof('int4'::regtype)",
            "`pg_typeof` is not implemented at all, so this is `0A000` naming the function rather \
             than a wrong type — the honest answer under contract C2. It is in the corpus because \
             it is the statement that would *prove* the divergence above: a real server says the \
             expression's type is `regtype`, and this node would say `text`.",
        ),
        (
            "SELECT 'float'::regtype::oid, 'float(24)'::regtype::oid, 'float(25)'::regtype::oid, \
             'float(53)'::regtype::oid",
            FLOAT_PRECISION,
        ),
        ("SELECT 'float(0)'::regtype::oid", FLOAT_PRECISION),
        ("SELECT 'float(54)'::regtype::oid", FLOAT_PRECISION),
        (
            "SELECT pg_typeof(NULL::float), pg_typeof(NULL::float(24)), pg_typeof(NULL::float(25))",
            FLOAT_PRECISION,
        ),
        (
            "SELECT 'decimal'::regtype::oid, pg_typeof(NULL::decimal), \
             pg_typeof(NULL::decimal(10,2))",
            PG_TYPEOF,
        ),
        (
            "SELECT 'oid'::regtype::oid, 'name'::regtype::oid, 'regtype'::regtype::oid",
            NO_SUCH_TYPE,
        ),
        (
            "SELECT 'date'::regtype::oid, 'time'::regtype::oid, 'interval'::regtype::oid",
            NO_SUCH_TYPE,
        ),
        (
            "SELECT 'time without time zone'::regtype::oid, 'timetz'::regtype::oid, \
             'time with time zone'::regtype::oid",
            NO_SUCH_TYPE,
        ),
        (
            "SELECT 'uuid'::regtype::oid, 'json'::regtype::oid, 'jsonb'::regtype::oid",
            NO_SUCH_TYPE,
        ),
        ("SELECT '\"int4\"'::regtype::oid", NAME_SYNTAX),
        ("SELECT '\"varchar\"'::regtype::oid", NAME_SYNTAX),
        ("SELECT '\"integer\"'::regtype::oid", NAME_SYNTAX),
        ("SELECT 'pg_catalog.int4'::regtype::oid", NAME_SYNTAX),
        (
            "SELECT 'date'::regtype, 'numeric'::regtype, 'uuid'::regtype, 'json'::regtype, \
             'jsonb'::regtype, 'interval'::regtype",
            NO_SUCH_TYPE,
        ),
        ("SELECT 23::regtype, 1043::regtype", REVERSE),
        ("SELECT 1007::regtype, 1009::regtype", REVERSE),
        ("SELECT 999999::regtype", REVERSE),
        ("SELECT 999999::regtype::text", REVERSE),
        (
            "SELECT '1'::regtype",
            "PostgreSQL reads a bare number in a type name as an **OID**, so `'1'::regtype` is \
             `1` rather than a lookup failure. The same reverse direction as `23::regtype`, \
             reached through the forward spelling.",
        ),
        (
            "SELECT 'integer'::regtype = 23",
            "A `regtype` **is** an OID on a real server, so comparing one with an integer is `t`. \
             Here `'integer'::regtype` is `text` and the comparison is `42883 operator does not \
             exist: text = bigint`. The declared consequence of the `regtype`-is-`text` trade at \
             the top of this file, and the statement that shows what it costs.",
        ),
        (
            "SELECT pg_typeof('integer'::regtype), pg_typeof('integer'::regtype::oid)",
            PG_TYPEOF,
        ),
        (
            "SELECT oid, typname, typlen, typcategory FROM pg_type WHERE typname IN \
             ('date','time','numeric','uuid','json','jsonb','interval') ORDER BY oid",
            "**`pg_type.typlen` is a column this node's catalog view does not have**, so the \
             statement is `42703` before any row is built. `date`, `time`, `numeric`, `json` and \
             `jsonb` all have correct rows — `tests/pg_catalog.rs` asserts them in ActiveRecord's \
             own query — and `uuid` and `interval` would be absent in any case.",
        ),
    ],
};

#[test]
fn every_regtype_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_regtype.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 24,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The statement the ladder stops on, answered.
///
/// Named on its own rather than left inside the corpus replay, because this one line is what three
/// scoreboard runs were waiting for and a future reader should be able to find it by name.
#[test]
fn the_statement_that_stopped_rung_2_answers() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 'integer'::regtype::oid"),
        vec![vec!["23"]]
    );
}

/// Statement 390 of `schema.rb`, which stopped **all 367 suite files** in r1's run 18.
///
/// `SELECT 'decimal(3,2)'::regtype::oid` was `42704 type "decimal(3,2)" does not exist` on a node
/// whose `pg_type` already listed `numeric` at 1700 — because `pg_type` derives itself from
/// `ColumnType::ALL` and the name table did not. Two sources for one fact, and the one nobody
/// read drifted.
///
/// The fix is that there is now one source: `value::type_by_name` resolves from `ColumnType::ALL`
/// too. This test is the blocker itself, plus the two other tier-2 types that had drifted the
/// same way and were invisible to the suite because nothing had asked for them yet.
#[test]
fn the_statement_that_stopped_every_suite_file() {
    let mut node = parity::Node::new(&[]);
    for (statement, expect) in [
        // The blocker, in the spelling Rails writes.
        ("SELECT 'decimal(3,2)'::regtype::oid", "1700"),
        ("SELECT 'decimal'::regtype::oid", "1700"),
        ("SELECT 'numeric'::regtype::oid", "1700"),
        ("SELECT 'numeric(10,2)'::regtype::oid", "1700"),
        // Missing the same way and never asked for, so never seen.
        ("SELECT 'date'::regtype::oid", "1082"),
        ("SELECT 'time'::regtype::oid", "1083"),
        ("SELECT 'time without time zone'::regtype::oid", "1083"),
    ] {
        assert_eq!(node.rows(statement), vec![vec![expect]], "{statement}");
    }

    // And the number beside the name is **read**, not skipped: a bound that a real server
    // refuses is refused here, which is what makes discarding the rest of it safe.
    for (statement, sqlstate, message) in [
        (
            "SELECT 'numeric(1001,0)'::regtype::oid",
            "22023",
            "NUMERIC precision 1001 must be between 1 and 1000",
        ),
        (
            "SELECT 'character varying(0)'::regtype::oid",
            "22023",
            "length for type varchar must be at least 1",
        ),
        (
            "SELECT 'timestamp(-1)'::regtype::oid",
            "42601",
            "syntax error at or near \"-\"",
        ),
    ] {
        let error = node.run(statement).unwrap_err();
        assert_eq!(error.sqlstate(), sqlstate, "{statement}");
        assert_eq!(error.to_string(), message, "{statement}");
    }
}

/// Statement 766 of `schema.rb`: an **array name** resolves, for every type this node has.
///
/// There is no array *storage* here and nothing can create a column of one. What resolves is the
/// name, because `ActiveRecord` asks `pg_type` for it before it asks for anything else — and
/// `'decimal[]'::regtype` is where the suite stopped.
///
/// The element name goes through the same `ColumnType::ALL`-derived resolution the scalar does,
/// so `decimal[]` works for the same reason `decimal` does; the only hand-written part is
/// `value::array_oid`, an exhaustive match a new type has to answer. The numbers are not
/// derivable — `_int4` is 1007 and `_int8` is 1016, out of order with their elements, and `_json`
/// is 199 where `json` is 114 — so each one is measured.
#[test]
fn an_array_name_resolves_for_every_type_this_node_has() {
    let mut node = parity::Node::new(&[]);

    // The statement the suite stopped on, and its internal spelling.
    assert_eq!(
        node.rows("SELECT 'decimal[]'::regtype::oid"),
        vec![vec!["1231"]]
    );
    assert_eq!(
        node.rows("SELECT 'numeric[]'::regtype::oid"),
        vec![vec!["1231"]]
    );
    assert_eq!(
        node.rows("SELECT '_numeric'::regtype::oid"),
        vec![vec!["1231"]]
    );

    // The ones the corpus already pinned, which used to be `42704`.
    for (name, oid) in [
        ("integer[]", "1007"),
        ("int4[]", "1007"),
        ("_int4", "1007"),
        ("text[]", "1009"),
        ("bigint[]", "1016"),
        ("uuid[]", "2951"),
        ("interval[]", "1187"),
        ("oid[]", "1028"),
        ("json[]", "199"),
        ("jsonb[]", "3807"),
        ("timestamp[]", "1115"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT '{name}'::regtype::oid")),
            vec![vec![oid]],
            "{name}"
        );
    }

    // **A shape is not part of the type**: dimensions are read and thrown away.
    for name in ["integer[][]", "integer[3]", "integer[3][4]"] {
        assert_eq!(
            node.rows(&format!("SELECT '{name}'::regtype::oid")),
            vec![vec!["1007"]],
            "{name}"
        );
    }

    // And it prints back the way a real server prints it: the element's name with `[]`.
    assert_eq!(
        node.rows("SELECT '_int4'::regtype, 'numeric[]'::regtype"),
        vec![vec!["integer[]", "numeric[]"]]
    );
}
