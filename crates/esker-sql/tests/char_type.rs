//! **`"char"` is a one-byte type, oid 18**, against PostgreSQL 19beta1.
//!
//! r1's wire sweep filed six rows: `pg_class.relkind`, `pg_constraint.contype`,
//! `pg_type.typcategory` and `pg_type.typdelim` are `"char"` (18) on a real server and `text` (25)
//! here. It is the catalog's own one-character type — **not** `character(1)`, which is `bpchar` —
//! and the quotes are part of how it is written, because `char` unquoted means `bpchar`.
//!
//! Two facts are the ones reasoning gets backwards, and each has a test below:
//!
//! * **`typcategory` is `Z`**, its own group rather than `S` with the strings. That one letter is
//!   what makes `CASE WHEN true THEN 'r'::"char" ELSE 'x'::text END` a `42804` **while
//!   `'r'::text = 'r'::"char"` is `t`**: the two compare and have no common type.
//! * **The byte is what is kept, not the character.** `'é'::"char"` is the first *byte* of a
//!   two-byte character, which is not valid UTF-8 on its own — so it prints as the octal escape
//!   `\303` and `octet_length` counts that escape's four characters.
//!
//! `min`/`max` decay to `text`, which makes this the fourth member of that arm after `varchar`,
//! `name` and `cidr`; `array_agg` keeps `"char"[]`; and `||` is **ambiguous** (`42725`) rather
//! than missing.
//!
//! Measured in `tests/captures/pg19_char_type.txt`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::pgwire::session::Execute;

#[path = "parity_harness/mod.rs"]
mod parity;

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    // What is left is not about `"char"` at all: `'r'::"char"::int4` differs only in `int4`'s own
    // width, which the literal ladder settled (ADR 0087). The `oid` and `regproc` halves of this
    // sentence closed with their own units (ADR 0097, ADR 0098). Every row agrees.
    types: &["SELECT 'r'::\"char\"::text, 'r'::\"char\"::int4, 65::int4::\"char\""],
    answers: &[
        // The oracle's catalog and this node's are different databases, so a count over `pg_class`
        // is each server's own. They are in the corpus because they are how a client *uses* the
        // type — `relkind = 'r'` is the predicate every schema reader writes — and what they prove
        // is that the comparison happens at all.
        (
            "SELECT count(*) FROM pg_class WHERE relkind = 'r'",
            "The two servers hold different catalogs, so the count is each server's own; what the \
             row is here for is that an unquoted literal compares against a `\"char\"` column.",
            "pg19_char_type.txt:123",
        ),
        (
            "SELECT count(*) FROM pg_class WHERE relkind = 'r'::\"char\"",
            "The same count, written with the cast.",
            "pg19_char_type.txt:124",
        ),
        (
            "SELECT relkind, count(*) FROM pg_class GROUP BY relkind ORDER BY relkind",
            "The same: each server's own catalog, grouped. The *shape* is the assertion — a \
             `\"char\"` groups and orders — and `tests/char_type.rs` pins that over rows this \
             node makes itself.",
            "pg19_char_type.txt:140",
        ),
        (
            "SELECT pg_typeof(contype) FROM pg_constraint LIMIT 1",
            "The oracle's `pg_constraint` has rows and this corpus builds no table, so there is \
             nothing here for the `LIMIT 1` to read. The column's declared type is asserted \
             directly in `the_catalog_columns_are_char`, over a table this node makes itself.",
            "pg19_char_type.txt:120",
        ),
    ],
};

#[test]
fn every_char_answer_is_postgresql_19_s() {
    let checked = parity::replay(include_str!("corpus/pg19_char_type.txt"), &[], &DIVERGENCES);
    assert!(
        checked > 30,
        "only {checked} statements ran; the corpus did not load"
    );
}

/// The declared type a client is told, **through a `Describe`** — the path r1's sweep reads.
fn described(node: &mut parity::Node, statement: &str) -> Vec<u32> {
    node.describe(statement)
        .unwrap()
        .fields
        .expect("a SELECT returns rows")
        .into_iter()
        .map(|field| field.type_oid)
        .collect()
}

/// **The four catalog columns r1 filed**, over the wire. 18 is `"char"`.
#[test]
fn the_catalog_columns_are_char() {
    let mut node = parity::Node::new(&["CREATE TABLE ct (id int8 PRIMARY KEY, v int8)"]);
    assert_eq!(
        described(&mut node, "SELECT relkind AS v FROM pg_class LIMIT 1"),
        vec![18]
    );
    assert_eq!(
        described(&mut node, "SELECT contype AS v FROM pg_constraint LIMIT 1"),
        vec![18]
    );
    assert_eq!(
        described(
            &mut node,
            "SELECT typcategory, typdelim FROM pg_type LIMIT 1"
        ),
        vec![18, 18]
    );
    // And the values are unchanged, which is what makes this a declared type rather than an answer.
    assert_eq!(
        node.rows("SELECT relkind FROM pg_class WHERE relname = 'ct'"),
        vec![vec!["r"]]
    );
    assert_eq!(
        node.rows("SELECT typcategory, typdelim FROM pg_type WHERE typname = 'text'"),
        vec![vec!["S", ","]]
    );
}

/// **One byte, and the byte is what is kept.**
#[test]
fn it_keeps_one_byte_and_not_one_character() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows("SELECT 'r'::\"char\", length('r'::\"char\"), octet_length('r'::\"char\")"),
        vec![vec!["r", "1", "1"]]
    );
    // Truncated to the first byte, silently — this is not a `22001`.
    assert_eq!(
        node.rows("SELECT 'abc'::\"char\", length('abc'::\"char\")"),
        vec![vec!["a", "1"]]
    );
    // The empty string is legal, is zero characters, and is **not** NULL.
    assert_eq!(
        node.rows("SELECT ''::\"char\", length(''::\"char\"), ''::\"char\" IS NULL"),
        vec![vec!["", "0", "f"]]
    );
    // **The byte, not the character**: `é` is two bytes, the first is kept, and it is not valid
    // UTF-8 on its own — so it prints as an octal escape and `octet_length` counts the escape.
    assert_eq!(
        node.rows("SELECT 'é'::\"char\", octet_length('é'::\"char\")"),
        vec![vec!["\\303", "4"]]
    );
    // Which is also why the `int4` casts are the byte's value in both directions.
    assert_eq!(
        node.rows("SELECT 'r'::\"char\"::int4, 65::int4::\"char\""),
        vec![vec!["114", "A"]]
    );
}

/// **It compares with `text` and has no common type with it**, which is `typcategory` `Z`.
#[test]
fn it_compares_with_text_and_shares_no_common_type() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT 'r'::\"char\" = 'r', 'r' = 'r'::\"char\", 'r'::\"char\" = 'r'::text, \
             'r'::text < 's'::\"char\""
        ),
        vec![vec!["t", "t", "t", "t"]]
    );
    // Byte order, so every capital precedes every lower-case letter.
    assert_eq!(
        node.rows("SELECT 'r'::\"char\" < 's'::\"char\", 'A'::\"char\" < 'a'::\"char\""),
        vec![vec!["t", "t"]]
    );
    assert_eq!(
        node.rows("SELECT 'r'::\"char\" IN ('r','v')"),
        vec![vec!["t"]]
    );
    // **And no common type, which is the half that does not follow from the half above.**
    let error = node
        .run("SELECT CASE WHEN true THEN 'r'::\"char\" ELSE 'x'::text END")
        .unwrap_err();
    assert_eq!(error.sqlstate(), esker_sql::sqlstate::DATATYPE_MISMATCH);
    assert_eq!(
        error.to_string(),
        "CASE types text and \"char\" cannot be matched"
    );
    // Two of its own do have one.
    assert_eq!(
        node.rows("SELECT coalesce('r'::\"char\", 'x'::\"char\")"),
        vec![vec!["r"]]
    );
}

/// `min`/`max` decay to `text`; `array_agg` keeps the type; `||` is **ambiguous**.
#[test]
fn the_aggregates_and_the_operator() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE ch (id int8 PRIMARY KEY, k \"char\")",
        "INSERT INTO ch VALUES (1,'r'),(2,'v'),(3,'i')",
    ]);
    // 25 is `text`: the fourth member of the arm `varchar`, `name` and `cidr` are on.
    assert_eq!(described(&mut node, "SELECT min(k) AS v FROM ch"), vec![25]);
    assert_eq!(described(&mut node, "SELECT max(k) AS v FROM ch"), vec![25]);
    assert_eq!(
        node.rows("SELECT min(k), max(k) FROM ch"),
        vec![vec!["i", "v"]]
    );
    // 1002 is `_char`: the aggregate keeps the type where `min` decays.
    assert_eq!(
        described(&mut node, "SELECT array_agg(k ORDER BY id) AS v FROM ch"),
        vec![1002]
    );
    assert_eq!(
        node.rows("SELECT array_agg(k ORDER BY id) FROM ch"),
        vec![vec!["{r,v,i}"]]
    );
    // A `"char"` groups and orders, which is what the corpus's `pg_class` rows show over a catalog
    // this node does not control.
    assert_eq!(
        node.rows("SELECT k, count(*) FROM ch GROUP BY k ORDER BY k"),
        vec![vec!["i", "1"], vec!["r", "1"], vec!["v", "1"]]
    );
    // **Ambiguous, not missing**: a real server has a candidate at every string width and cannot
    // choose, which is `42725` rather than the `42883` a wrong type gets.
    let error = node.run("SELECT 'r'::\"char\" || 'x'").unwrap_err();
    assert_eq!(error.sqlstate(), esker_sql::sqlstate::AMBIGUOUS_FUNCTION);
}

/// The catalog carries both rows, and `format_type` prints the quotes.
#[test]
fn pg_type_has_the_char_rows() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT oid, typname, typlen, typtype, typcategory, typdelim, typinput, typarray \
             FROM pg_type WHERE oid IN (18, 1002) ORDER BY oid"
        ),
        vec![
            vec!["18", "char", "1", "b", "Z", ",", "charin", "1002"],
            vec!["1002", "_char", "-1", "b", "A", ",", "array_in", "0"],
        ]
    );
    // **With the quotes**, which is how it must be written and how PostgreSQL prints it.
    assert_eq!(
        node.rows("SELECT format_type(18, -1), format_type(1002, -1)"),
        vec![vec!["\"char\"", "\"char\"[]"]]
    );
}

/// **The rule, not the four rows r1 filed**: every catalog column a real server declares oid 18
/// is one here.
///
/// The four r1 measured — `relkind`, `contype`, `typcategory`, `typdelim` — are what a wire sweep
/// happens to look at, and stopping there would have left fourteen columns of the same type
/// answering `text` for no reason but that nobody had asked. The census is one query rather than a
/// list to remember (`captures/pg19_char_type.txt`): a real server has **46** such columns, of
/// which this node declares the eighteen below. Each carries the corpus line where this tree
/// already recorded the oracle calling it `"char"`, taken when that corpus was captured.
///
/// Asserted through `describe` rather than through a row, because most of these views are empty
/// without a fixture and the declared type is the whole question — the same reason the enum's OID
/// needed `tests/enum_extended_protocol.rs`.
#[test]
fn every_catalog_column_a_real_server_calls_char_is_one_here() {
    let mut node = parity::Node::new(&[]);
    for (view, column, cited) in [
        ("pg_am", "amtype", "corpus/pg19_pg_trgm.txt:15"),
        (
            "pg_attribute",
            "attidentity",
            "corpus/pg19_activerecord_schema_dump.txt:45",
        ),
        (
            "pg_attribute",
            "attgenerated",
            "corpus/pg19_catalog_attribute.txt:68",
        ),
        ("pg_cast", "castcontext", "pg19_assignment_cast_date.txt:42"),
        ("pg_cast", "castmethod", "pg19_assignment_cast_date.txt:42"),
        ("pg_class", "relkind", "pg19_char_type.txt:119"),
        ("pg_class", "relpersistence", "pg19_temp_table.txt:46"),
        ("pg_constraint", "contype", "pg19_char_type.txt:120"),
        (
            "pg_constraint",
            "confupdtype",
            "pg19_foreign_key_options.txt:91",
        ),
        (
            "pg_constraint",
            "confdeltype",
            "pg19_foreign_key_options.txt:91",
        ),
        // **`deptype` and `partstrat` have no `\gdesc` line in this tree** — nothing in the suite
        // reads either through a corpus — so their citation is the census itself, which is the
        // measurement that found them rather than a list I remembered.
        ("pg_depend", "deptype", "pg19_char_type.txt:71"),
        ("pg_partitioned_table", "partstrat", "pg19_char_type.txt:71"),
        ("pg_proc", "prokind", "pg19_trigger_function.txt:80"),
        ("pg_proc", "provolatile", "pg19_default_uuid.txt:57"),
        ("pg_trigger", "tgenabled", "pg19_trigger_function.txt:114"),
        ("pg_type", "typtype", "pg19_array_type_map.txt:45"),
        ("pg_type", "typcategory", "pg19_char_type.txt:121"),
        ("pg_type", "typdelim", "pg19_char_type.txt:121"),
    ] {
        let statement = format!("SELECT {column} FROM {view}");
        let parsed = esker_sql::parse::parse_statements(&statement).unwrap();
        let described = node.executor.describe(&parsed[0], &[]).unwrap();
        let fields = described.fields.expect("a SELECT returns rows");
        assert_eq!(
            fields[0].type_oid, 18,
            "{view}.{column} is declared {} and a real server says 18 ({cited})",
            fields[0].type_oid
        );
    }
}

/// **A `name` is 64 `"char"`s**, so its `typelem` names a row that is there.
///
/// It was a zero for exactly as long as the type was missing, which was the honest answer: a
/// `typelem` pointing at an absent `pg_type` row is what `array_delimiter.rs::no_typarray_dangles`
/// forbids one column over. Measured — `pg_type.typelem` for `name` is 18 on a real server, and
/// `_char`'s is 18 as well.
#[test]
fn a_name_is_made_of_chars_and_says_so() {
    let mut node = parity::Node::new(&[]);
    assert_eq!(
        node.rows(
            "SELECT typname, oid, typelem, typlen FROM pg_type \
             WHERE typname IN ('char', 'name', '_char', '_name') ORDER BY oid"
        ),
        vec![
            vec!["char", "18", "0", "1"],
            vec!["name", "19", "18", "64"],
            vec!["_char", "1002", "18", "-1"],
            vec!["_name", "1003", "19", "-1"],
        ]
    );
    // The pointer resolves, which is the property the zero was protecting.
    assert!(
        node.rows(
            "SELECT t.typname, t.typelem FROM pg_type t WHERE t.typelem <> 0 \
             AND NOT EXISTS (SELECT 1 FROM pg_type e WHERE e.oid = t.typelem)"
        )
        .is_empty(),
        "a typelem points at a pg_type row that is not there"
    );
}
