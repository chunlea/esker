//! `json` and `jsonb` against PostgreSQL 19beta1's own answers.
//!
//! Tier 2's first type and the first format addition since ADR 0033.
//! `docs/adr/0042-json-and-jsonb-are-two-types-and-one-of-them-is-not-a-key.md` is what the
//! capture decided; `r1-harness`'s triage is why this unit came before the others.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds its own table.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
///
/// Forty-six lines and **nine reasons**, which is the useful shape: each constant below is one
/// argument, and every line that shares it points at the same one rather than restating it. What
/// is *not* here is as important — every line this node answered wrongly rather than refusing was
/// fixed before this landed, because a wrong answer is not a divergence.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: TYPES,
    answers: ANSWERS,
};

/// Statements whose **rows** are right and whose declared type is not.
///
/// Statements whose **rows** are right and whose declared type is not.
///
/// **One entry, and it was eighteen.** Seventeen of them said the same thing: a `json` or `jsonb`
/// value is a `Datum::Text`, so a folded cast threw the declared type away and `RowDescription`
/// carried `text`'s OID where a real server carries `json`'s or `jsonb`'s. A **column** of either
/// type always reported correctly — a column's type comes from the catalog rather than from its
/// values — and only a bare literal or cast lost it, which is why no test but this list saw it.
/// The `name` unit closed it for every shared representation at once (ADR 0086): a folded cast
/// keeps its `Expr::Cast` node when the value cannot speak for itself, and that node is what
/// `expr_type` reads.
///
/// `Datum` still has no `jsonb` variant, and the `COMPARISON` divergence below is still that fact
/// — this half of it never needed one.
const TYPES: &[&str] = &[
    // **Moved here from `answers` by parity rule 4**: the rows agree and what still differs is the
    // declared type, which is one of the standing families — see `parity::Divergences::types`.
    // `typlen`, `typinput` and `typcategory` are a `smallint`, a `regproc` and a `"char"` on a real
    // server and `text` here; the catalog's own columns are their own units.
    "SELECT oid, typname, typlen, typinput, typcategory FROM pg_type WHERE typname IN \
     ('json','jsonb') ORDER BY oid",
    // `pg_typeof`'s own `regtype`/`text` trade (ADR 0077). The two type names it answers are right
    // since a folded cast started keeping the type it named (ADR 0086), and both `format_type`
    // calls always were.
    "SELECT pg_typeof('{}'::json), pg_typeof('{}'::jsonb), format_type(114, -1), \
     format_type(3802, -1)",
];

/// One of `DIVERGENCES`' seven reasons.
const ORDER: &str = "The same refusal for the same reason: a byte sort is not `jsonb`'s order, which puts \
     `null` below `1.00` where the bytes put it above.";
/// One of `DIVERGENCES`' seven reasons.
const CONTAINMENT: &str = "Containment and the editing operators, likewise not built. `@>` is named in this \
     unit's scope and lands with the extraction operators; the rest are a later unit. \
     Each is `0A000` naming itself.";
/// One of `DIVERGENCES`' seven reasons.
const MESSAGES: &str = "The refusal is the right one and its **text** differs: PostgreSQL adds a `DETAIL` \
     naming the position or the token, which this node does not carry for these two \
     SQLSTATEs. The SQLSTATE and the sentence agree; the detail line does not.";
/// One of `DIVERGENCES`' seven reasons.
const COMPARISON: &str = "**Comparison over `json` or `jsonb` is refused rather than answered from the \
     bytes**, which is what ADR 0042 turns on: `'1.0'::jsonb = '1.00'::jsonb` is `t` on a \
     real server and byte equality says `f`, and `jsonb` sorts by *kind* before value. \
     `Datum` has no json variant — these types share `text`'s representation, which is \
     safe for `varchar` and `bpchar` because their comparison *is* text comparison and is \
     not safe here. Giving `jsonb` a `Datum` of its own is what closes this, and it is \
     the `real` unit's lesson one layer up: a type may share another's representation \
     only if it shares its comparison. `json` has no comparison operators at all on a \
     real server, so refusing there is closer still than refusing `jsonb`.";
/// One of `DIVERGENCES`' seven reasons.
const OPERATORS: &str = "**`#>` and `#>>` are what is left of this reason.** `->` and `->>` were \
     here too and are built now, and the nine lines that declared them agreed the moment they \
     were — which is what these rows are for: they were written as the specification of a unit \
     that had not happened, and the ratchet said when it had. The path operators take an array of \
     keys rather than one, so they are their own unit; `0A000` naming the operator is contract \
     C2's answer for a construct that parses and does not run.";
/// One of `DIVERGENCES`' seven reasons.
const FUNCTIONS: &str = "A `json` function this node does not have, named under contract C2. None is in this \
     unit's scope; the corpus carries them so the unit that adds them starts from the \
     measurement.";
/// One of `DIVERGENCES`' seven reasons.
const CASTS: &str = "**Both refuse a `jsonb` *object* cast to a scalar; the code and the \
     message differ.** PostgreSQL rejects it in the cast itself, `22023 cannot cast jsonb \
     object to type integer`, where this node renders the object to its text and hands that \
     to `integer`s input function, which refuses the rendered object as `22P02 invalid \
     input syntax for type integer`. A refusal of the right shape with the wrong code, \
     closing when the cast checks the `jsonb` *kind* before rendering. The scalar casts \
     this reason also covered now agree: `::text` landed with the json unit and `::numeric` \
     with this one.";
/// Every statement this node answers differently, each pointing at one reason above.
const ANSWERS: &[(&str, &str, &str)] = &[
    ("SELECT id, b FROM js ORDER BY b", ORDER, "pg19_json.txt:69"),
    (
        "SELECT id FROM js WHERE b @> '{\"a\":2}'",
        CONTAINMENT,
        "pg19_json.txt:72",
    ),
    ("SELECT '{bad}'::json", MESSAGES, "pg19_json.txt:90"),
    ("SELECT ''::json", MESSAGES, "pg19_json.txt:91"),
    ("SELECT '{bad}'::jsonb", MESSAGES, "pg19_json.txt:92"),
    ("SELECT '\"\\u0000\"'::jsonb", MESSAGES, "pg19_json.txt:96"),
    (
        "SELECT ('\"\\u0000\"'::json)::jsonb",
        MESSAGES,
        "pg19_json.txt:97",
    ),
    (
        "SELECT '{\"a\":1}'::json = '{\"a\":1}'::json",
        COMPARISON,
        "pg19_concat.txt:42",
    ),
    (
        "SELECT '{\"a\":1}'::json = '{\"a\":1}'::jsonb",
        COMPARISON,
        "pg19_json.txt:99",
    ),
    (
        "SELECT '{\"a\":1}'::jsonb = '{\"a\": 1}'::jsonb",
        COMPARISON,
        "pg19_json.txt:100",
    ),
    (
        "SELECT '1.0'::jsonb = '1.00'::jsonb",
        COMPARISON,
        "pg19_json.txt:101",
    ),
    (
        "SELECT '{\"a\":1}'::jsonb = '{\"a\":1.0}'::jsonb",
        COMPARISON,
        "pg19_json.txt:102",
    ),
    (
        "SELECT '{\"a\":1}'::jsonb <> '{\"a\":2}'::jsonb",
        COMPARISON,
        "pg19_json.txt:103",
    ),
    (
        "SELECT '{\"a\":1}'::jsonb < '{\"b\":1}'::jsonb",
        COMPARISON,
        "pg19_json.txt:104",
    ),
    (
        "SELECT 'true'::jsonb > '1'::jsonb, '\"s\"'::jsonb > '1'::jsonb, '[]'::jsonb \
             > '{}'::jsonb",
        COMPARISON,
        "pg19_json.txt:105",
    ),
    (
        "SELECT '{\"a\":{\"b\":2}}'::json #> '{a,b}', '{\"a\":{\"b\":2}}'::json #>> \
             '{a,b}'",
        OPERATORS,
        "pg19_json.txt:115",
    ),
    (
        "SELECT '{\"a\":1,\"b\":2}'::jsonb @> '{\"a\":1}'::jsonb, '{\"a\":1}'::jsonb \
             <@ '{\"a\":1,\"b\":2}'::jsonb",
        CONTAINMENT,
        "pg19_json.txt:116",
    ),
    (
        "SELECT '{\"a\":1,\"b\":2}'::json @> '{\"a\":1}'::json",
        CONTAINMENT,
        "pg19_json.txt:117",
    ),
    (
        "SELECT '{\"a\":1}'::jsonb ? 'a'",
        CONTAINMENT,
        "pg19_json.txt:118",
    ),
    (
        "SELECT '{\"a\":1}'::jsonb || '{\"b\":2}'::jsonb, '{\"a\":1,\"b\":2}'::jsonb \
             - 'a'",
        CONTAINMENT,
        "pg19_json.txt:119",
    ),
    (
        "SELECT '{\"a\":1}'::jsonb #- '{a}'",
        CONTAINMENT,
        "pg19_json.txt:121",
    ),
    (
        "SELECT json_typeof('1'::json), jsonb_typeof('\"s\"'::jsonb), \
             jsonb_typeof('null'::jsonb)",
        FUNCTIONS,
        "pg19_json.txt:122",
    ),
    (
        "SELECT jsonb_typeof('[]'::jsonb), jsonb_typeof('{}'::jsonb), \
             jsonb_typeof('1.0'::jsonb)",
        FUNCTIONS,
        "pg19_json.txt:123",
    ),
    (
        "SELECT jsonb_array_length('[1,2,3]'::jsonb), \
             json_array_length('[1,2,3]'::json)",
        FUNCTIONS,
        "pg19_json.txt:124",
    ),
    (
        "SELECT jsonb_object_keys('{\"b\":1,\"a\":2}'::jsonb)",
        FUNCTIONS,
        "pg19_json.txt:125",
    ),
    (
        "SELECT jsonb_strip_nulls('{\"a\":null,\"b\":1}'::jsonb)",
        FUNCTIONS,
        "pg19_json.txt:126",
    ),
    (
        "SELECT jsonb_build_object('b',1,'a',2), jsonb_build_array(1,'x')",
        FUNCTIONS,
        "pg19_json.txt:127",
    ),
    (
        "SELECT to_jsonb('1 day'::interval), to_json('2020-01-01'::date)",
        FUNCTIONS,
        "pg19_json.txt:128",
    ),
    (
        "SELECT json_agg(x), jsonb_agg(x) FROM (VALUES (1), (2)) v(x)",
        FUNCTIONS,
        "pg19_json.txt:129",
    ),
    ("SELECT '{\"a\":1}'::jsonb::int", CASTS, "pg19_json.txt:134"),
    (
        "SELECT NULL::jsonb IS NULL, '{\"a\":1}'::jsonb = NULL",
        COMPARISON,
        "pg19_json.txt:137",
    ),
];

#[test]
fn every_json_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_json.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(
        checked > 70,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// `jsonb`'s canonical form: three normalisations at once, and two of the three are not guessable.
///
/// Keys reordered **by length then bytes** — one example cannot tell that from lexicographic
/// order, so two are here — duplicates dropped with the **last** winning, and a space after every
/// colon and comma. A `json` beside each shows what is *not* done to it.
#[test]
fn jsonb_canonicalises_where_json_keeps_the_text() {
    let mut node = parity::Node::new(&[]);
    for (sql, expected) in [
        (
            "SELECT '{\"b\":1, \"a\":2}'::json, '{\"b\":1, \"a\":2}'::jsonb",
            vec!["{\"b\":1, \"a\":2}", "{\"a\": 2, \"b\": 1}"],
        ),
        (
            "SELECT '{\"a\":1,\"a\":2}'::json, '{\"a\":1,\"a\":2}'::jsonb",
            vec!["{\"a\":1,\"a\":2}", "{\"a\": 2}"],
        ),
        (
            "SELECT '{  \"a\"  :  1  }'::json, '{  \"a\"  :  1  }'::jsonb",
            vec!["{  \"a\"  :  1  }", "{\"a\": 1}"],
        ),
        // Length first: lexicographic order would put `bb` after `ccc`.
        (
            "SELECT '{\"bb\":1,\"a\":2,\"ccc\":3}'::jsonb",
            vec!["{\"a\": 2, \"bb\": 1, \"ccc\": 3}"],
        ),
        // Then bytes, within one length.
        (
            "SELECT '{\"ab\":1,\"ba\":2,\"aa\":3}'::jsonb",
            vec!["{\"aa\": 3, \"ab\": 1, \"ba\": 2}"],
        ),
        // All the way down, not just at the top level.
        (
            "SELECT '{\"a\":{\"z\":1,\"b\":{\"y\":1,\"a\":2}}}'::jsonb",
            vec!["{\"a\": {\"b\": {\"a\": 2, \"y\": 1}, \"z\": 1}}"],
        ),
    ] {
        assert_eq!(node.rows(sql), vec![expected], "{sql}");
    }
}

/// A jsonb number is a `numeric`, so the scale is kept and the exponent is not.
///
/// `1.00` stays `1.00` and `1e2` becomes `100` — one preserved and one not, because both are what
/// `numeric` does with that text. `1E400` becomes four hundred digits rather than staying an
/// exponent, which is the same rule taken to its end.
#[test]
fn a_jsonb_number_keeps_its_scale_and_loses_its_exponent() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT '1.0'::json, '1.0'::jsonb, '1.00'::jsonb, '1e2'::jsonb"),
        vec![vec!["1.0", "1.0", "1.00", "100"]]
    );
    let long = node.rows("SELECT '1E400'::jsonb");
    let digits = long
        .first()
        .and_then(|row| row.first())
        .map_or(0, String::len);
    assert_eq!(digits, 401, "1E400 should expand to a 401-digit integer");
}

/// A chain of casts runs each step, because the steps are not interchangeable.
///
/// `'{"b":1,"a":2}'::jsonb::json` is `{"a": 2, "b": 1}`: the `jsonb` canonicalised it and the
/// `json` stored *that*. Reading the original text as `json` directly answers the input unchanged,
/// which is a different value.
#[test]
fn a_chain_of_casts_applies_each_step_in_turn() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT '{\"b\":1,\"a\":2}'::jsonb::json"),
        vec![vec!["{\"a\": 2, \"b\": 1}"]]
    );
    assert_eq!(
        node.rows("SELECT '{\"b\":1,\"a\":2}'::json"),
        vec![vec!["{\"b\":1,\"a\":2}"]]
    );
}

/// A NUL escape is the one input that tells the two types apart.
///
/// `json` stores it, because `json` stores the text. `jsonb`'s stored form is text and a NUL
/// cannot be in one, so it answers `22P05` — the only one in this project — and casting a stored
/// `json` that contains one to `jsonb` raises it later, which is what makes `json`'s permissiveness
/// safe rather than a trap.
#[test]
fn a_nul_escape_splits_the_two_types() {
    let mut node = parity::Node::new(&[]);
    let escape = "'\"\\u0000\"'";
    assert_eq!(
        node.rows(&format!("SELECT {escape}::json")),
        vec![vec!["\"\\u0000\""]]
    );
    for sql in [
        format!("SELECT {escape}::jsonb"),
        format!("SELECT ({escape}::json)::jsonb"),
    ] {
        let error = node.run(&sql).unwrap_err();
        assert_eq!(error.sqlstate(), "22P05", "{sql}");
    }
}

/// Comparison is **refused**, not answered from the bytes, and that is the point of ADR 0042.
///
/// `'1.0'::jsonb = '1.00'::jsonb` is `t` on a real server while the two print differently, so byte
/// equality is not `jsonb` equality; `jsonb` also sorts by kind before value. `Datum` has no json
/// variant — these types share `text`'s representation, which is safe for `varchar` and `bpchar`
/// because their comparison *is* text comparison, and is not safe here. Refusing is contract C2;
/// answering `f` would be a wrong answer.
#[test]
fn comparing_json_is_refused_rather_than_answered_wrongly() {
    let mut node = parity::Node::new(&[]);
    for sql in [
        "SELECT '1.0'::jsonb = '1.00'::jsonb",
        "SELECT '{\"a\":1}'::jsonb < '{\"b\":1}'::jsonb",
        "SELECT '{\"a\":1}'::json = '{\"a\":1}'::json",
    ] {
        let error = node.run(sql).unwrap_err();
        assert_eq!(error.sqlstate(), "0A000", "{sql}");
    }

    // And over a column, where the type is on the plan rather than in the syntax.
    node.run("CREATE TABLE j (id int8 PRIMARY KEY, b jsonb)")
        .unwrap();
    let error = node.run("SELECT id FROM j ORDER BY b").unwrap_err();
    assert_eq!(error.sqlstate(), "0A000");
}

/// A `jsonb` column round-trips through the row codec, which is what a format addition has to do.
#[test]
fn a_json_column_stores_and_reads_back() {
    let mut node = parity::Node::new(&[]);
    for statement in [
        "CREATE TABLE docs (id int8 PRIMARY KEY, j json, b jsonb)",
        "INSERT INTO docs VALUES (1, '{\"b\":1, \"a\":2}', '{\"b\":1, \"a\":2}')",
        "INSERT INTO docs VALUES (2, NULL, NULL)",
    ] {
        node.run(statement).unwrap();
    }
    assert_eq!(
        node.rows("SELECT id, j, b FROM docs ORDER BY id"),
        vec![
            vec!["1", "{\"b\":1, \"a\":2}", "{\"a\": 2, \"b\": 1}"],
            vec!["2", "\\N", "\\N"],
        ]
    );
}
