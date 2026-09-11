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

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[
        // **Moved here from `answers` by parity rule 4**: the rows agree, and what still differs
        // is one of the standing declared-type families listed on `parity::Divergences::types`.
        //
        // The reason they used to carry was about the *answer*, and it had stopped being true:
        // "the reverse direction answers now — `t.typelem::regtype` is how `ActiveRecord` reads
        // what an array type is over, which is the caller this half never had — and every value
        // below is byte-identical: `23` is `integer`, `1007` is `integer[]`, oid 0 is `-` and an
        // unknown number prints as itself." What is left is the trade a `regtype` always was: a
        // type of its own on a real server, four bytes holding an oid that print as a name, and
        // `text` here — so `RowDescription` differs and the characters do not.
        // A real server's `regtype` is a type of its own — four bytes holding an OID that print as
        // the type's name. This node has no `regtype`, so `'x'::regtype` answers the **name**, as
        // text: the value is byte-identical and only `RowDescription`'s OID differs, `text` where
        // a real server says `regtype`. The same trade `pg_catalog.rs` makes for `pg_type.oid`.
        "SELECT 'int4'::regtype",
        "SELECT 'varchar'::regtype",
        // **`::oid` used to answer a `bigint` here and does not any more.** `oid` has been a
        // real `ColumnType` since its own unit; this spelling had not caught up, and closing that
        // drift deleted 27 entries from this list — every `'x'::regtype::oid` in it agreed on the
        // value all along and now agrees on the type as well (ADR 0031's rule 2).
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
        "SELECT 'int4[]'::regtype::oid",
        // Every remaining spelling the corpus asks for, answering the right OID and declaring
        // `bigint` where a real server declares `oid` — the same one trade as the lines above,
        // and the reason this list is long rather than deep. `numeric`, `decimal`, `date` and
        // `time` are in it because they resolve now: `value::type_by_name` derives its names from
        // `ColumnType::ALL`, so a type that exists is a name that resolves.
    ],
    answers: &[
        (
            "SELECT pg_typeof('int4'::regtype)",
            "`pg_typeof` is not implemented at all, so this is `0A000` naming the function rather \
             than a wrong type — the honest answer under contract C2. It is in the corpus because \
             it is the statement that would *prove* the divergence above: a real server says the \
             expression's type is `regtype`, and this node would say `text`.",
            "UNMEASURED",
        ),
        (
            "SELECT 'float'::regtype::oid, 'float(24)'::regtype::oid, 'float(25)'::regtype::oid, \
             'float(53)'::regtype::oid",
            FLOAT_PRECISION,
            "pg19_regtype.txt:75",
        ),
        (
            "SELECT 'float(0)'::regtype::oid",
            FLOAT_PRECISION,
            "pg19_regtype.txt:76",
        ),
        (
            "SELECT 'float(54)'::regtype::oid",
            FLOAT_PRECISION,
            "pg19_regtype.txt:77",
        ),
        (
            "SELECT 'time without time zone'::regtype::oid, 'timetz'::regtype::oid, \
             'time with time zone'::regtype::oid",
            NO_SUCH_TYPE,
            "pg19_regtype.txt:82",
        ),
        // **`SELECT '1'::regtype` was here and is closed**, 2026-09-10: a bare number in a type
        // name is an *oid*, which is `regtypein`'s own rule and now `value::oid_spelled`'s, one
        // reader for the three roads a `regtype` arrives by (the literal, a `text` per row, and
        // an array element). It came out of the cast matrix's residue, where the same defect was
        // four rows of `<integer>[] -> regtype[]` — the scalar and the array being one rule, as
        // this entry's own reason had said. `a_regtype_written_as_digits_is_an_oid` pins it.
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

/// **`regtypein` reads all digits as an oid**, which is the same rule `regclassin` has and the
/// last of the cast matrix's `42846 / ok` residue.
///
/// `'23'::regtype` is `integer` on a real server and was `42704 type "23" does not exist` here:
/// the name went to the catalog's user-type lookup, which is right for `'mood'` and wrong for a
/// number. Measured on 19beta1, 2026-09-10, every row below being that server's answer:
///
/// ```text
/// '23'::regtype                     integer      '999999'::regtype    999999
/// '23'::text::regtype               integer      'integer'::regtype   integer
/// '{23,25}'::text::regtype[]        {integer,text}
/// '{23,25}'::integer[]::regtype[]   {integer,text}
/// '23 25'::int2vector::regtype[]    [0:1]={integer,text}
/// ```
///
/// **An oid no type names prints its own digits** rather than raising, exactly as a `regclass`
/// does — the name is the *output* function and an oid is always a legal input to it (ADR 0077).
/// The boundary is **digits**, and what is not digits is still a name: `'-1'::regtype` is a syntax
/// error on 19beta1 (`invalid type name "-1"`), the sign making it a name rather than a number.
#[test]
fn a_regtype_written_as_digits_is_an_oid() {
    let mut node = parity::Node::new(&[]);
    for (written, answer) in [
        ("'23'::regtype", "integer"),
        ("'25'::regtype", "text"),
        ("'1043'::regtype", "character varying"),
        // An oid nothing names, which is not an error.
        ("'999999'::regtype", "999999"),
        // The name road, unchanged.
        ("'integer'::regtype", "integer"),
        ("'int4'::regtype", "integer"),
        // Through a `text`, which is the I/O conversion rather than a `pg_cast` row.
        ("'23'::text::regtype", "integer"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT ({written})::text")),
            vec![vec![answer]],
            "{written}"
        );
    }
    // The arrays, all three spellings, and the vector — which is what made this the residue's
    // last two rows rather than four.
    for (written, answer) in [
        ("'{23,25}'::text::regtype[]", "{integer,text}"),
        ("'{23,25}'::integer[]::regtype[]", "{integer,text}"),
        ("'23 25'::int2vector::regtype[]", "[0:1]={integer,text}"),
        ("'23 25'::oidvector::regtype[]", "[0:1]={integer,text}"),
    ] {
        assert_eq!(
            node.rows(&format!("SELECT ({written})::text")),
            vec![vec![answer]],
            "{written}"
        );
    }
    // And a name that is not a type is still `42704`, which is the half this rule must not take.
    assert!(
        node.answer("SELECT 'nosuchtype'::regtype")
            .to_string()
            .starts_with("!42704"),
        "a name nothing names is still undefined"
    );
}
