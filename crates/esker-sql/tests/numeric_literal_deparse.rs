//! **The type a printed numeric constant shows**, and the three forms it can take.
//!
//! `docs/plans/debts-v1.1.md` #22. `exec::ddl::deparse_literal` prints a number bare whatever its
//! type; PostgreSQL prints one of three forms and which one is a property of the **node** rather
//! than of the value — the corpus header states the rule beside the rows that measure it, and the
//! short version is: bare for a constant of the literal form's default type, `(N)::type` for a
//! cast node over one, `'N'::type` for a bare constant whose type is not the default.
//!
//! The corpus reads all four printers — a generated column, a `DEFAULT`, an index key and a
//! `CHECK` — because the rule is one deparser's and a fix that reached only one of them would look
//! like a fix.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the three tables it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[(
        "SELECT 'r', a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1nd'::regclass ORDER BY a.attnum",
        "**One of nine, and after #24 the only one: the *spelling*, not the form.** \
         `DEFAULT -1::bigint` is `(- (1)::bigint)` on a real server — `::` binds tighter than \
         unary minus, so it is an operator over a cast, not a cast over a negative constant — and \
         `(-1::BIGINT)` here, the expression as written with the type name upper-cased. \
         `debts-v1.1.md` #24 closed the other two shapes in this row: `DEFAULT (-1)::bigint` is \
         `('-1'::integer)::bigint` in both now, and so is `DEFAULT (-1.5)::double precision`. \
         What is left is not a printer gap. Both spellings fold to the same constant here, so \
         after lowering there is nothing to tell them apart and the printer would have to invent \
         one; keeping the unary minus as a node is a change to what the plan holds. Measured over \
         four target types in `tests/corpus/pg19_negative_constant.txt`, whose `u_*` rows are \
         this shape, and #24's row carries it.",
        "pg19_numeric_literal_deparse.txt:83",
    )],
};

#[test]
fn every_printed_numeric_constant_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_numeric_literal_deparse.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 40, "the corpus shrank: {checked} statements");
}

/// **The three forms, asserted where the corpus cannot say why.**
///
/// Each pair differs in exactly the thing its clause is about, so an implementation that gets the
/// clause wrong cannot pass both halves.
#[test]
fn the_form_is_a_property_of_the_node() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE nl (id int8 PRIMARY KEY, i4 integer, i8 bigint, nm numeric)",
        // bare: the literal form's own default type, written and cast-to-itself
        "ALTER TABLE nl ADD COLUMN a integer GENERATED ALWAYS AS (1) STORED",
        "ALTER TABLE nl ADD COLUMN b integer GENERATED ALWAYS AS (1::integer) STORED",
        // (N)::type: a cast node, written and coercion-inserted
        "ALTER TABLE nl ADD COLUMN c bigint GENERATED ALWAYS AS (1::bigint) STORED",
        "ALTER TABLE nl ADD COLUMN d boolean GENERATED ALWAYS AS (nm > 1) STORED",
        // bare again, and this is the pair that catches a rule about the column's width:
        // int8 > int4 is an operator, so nothing is coerced
        "ALTER TABLE nl ADD COLUMN e boolean GENERATED ALWAYS AS (i8 > 1) STORED",
        // 'N'::type: a bare constant whose type is not the default
        "ALTER TABLE nl ADD COLUMN f integer GENERATED ALWAYS AS (-1) STORED",
        "ALTER TABLE nl ADD COLUMN g bigint GENERATED ALWAYS AS (9223372036854775807) STORED",
    ]);
    let printed = |node: &mut parity::Node, column: &str| {
        node.rows(&format!(
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON \
             d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'nl'::regclass AND \
             a.attname = '{column}'"
        ))
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next())
        .unwrap_or_default()
    };
    assert_eq!(printed(&mut node, "a"), "1");
    assert_eq!(
        printed(&mut node, "b"),
        "1",
        "a cast to the type it already had shows nothing"
    );
    assert_eq!(printed(&mut node, "c"), "(1)::bigint");
    assert_eq!(
        printed(&mut node, "d"),
        "(nm > (1)::numeric)",
        "the comparison has no numeric > integer operator, so the literal is coerced"
    );
    assert_eq!(
        printed(&mut node, "e"),
        "(i8 > 1)",
        "int8 > int4 is an operator; a rule about the column's width would print a cast here"
    );
    assert_eq!(printed(&mut node, "f"), "'-1'::integer");
    assert_eq!(printed(&mut node, "g"), "'9223372036854775807'::bigint");
}

/// **A comparison's literal keeps its own type**, which is `debts-v1.1.md` #23.
///
/// PostgreSQL picks an operator — `int24gt` for `i2 > 1` — and leaves the constant an `integer`;
/// this node narrowed it to the column's type, so the tree held a `smallint` node and the printed
/// text said so. The **values** compared identically either way, which is why the only place it
/// showed is a deparse, and why `retype`'s own `numeric` arm already carried the sentence for it:
/// narrowing is what an *assignment* does, and `22003` is the right answer to an `INSERT` and the
/// wrong one to a `WHERE`.
///
/// `int4` and `int8` were already right and are asserted with it: their narrowing is a no-op
/// because the datum stays an `i64` whatever width the literal is *declared* (ADR 0087), so
/// `smallint` was the only width where the rule was visible — and a fix that only looked at
/// `smallint` would be a fix to the symptom.
#[test]
fn a_comparisons_literal_keeps_its_own_type() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE c23 (id int8 PRIMARY KEY, i2 smallint, i4 integer, i8 bigint)",
        "ALTER TABLE c23 ADD COLUMN c_i2 boolean GENERATED ALWAYS AS (i2 > 1) STORED",
        "ALTER TABLE c23 ADD COLUMN c_i4 boolean GENERATED ALWAYS AS (i4 > 1) STORED",
        "ALTER TABLE c23 ADD COLUMN c_i8 boolean GENERATED ALWAYS AS (i8 > 1) STORED",
        // Wider than the column, which is the case that was a **wrong refusal** — `22003 integer
        // out of range` — until the batch that measured this corpus.
        "ALTER TABLE c23 ADD COLUMN c_i2w boolean GENERATED ALWAYS AS (i2 > 100000) STORED",
    ]);
    assert_eq!(
        node.rows(
            "SELECT a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a \
             JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE a.attrelid = 'c23'::regclass AND a.attnum > 4 ORDER BY a.attnum"
        ),
        vec![
            vec!["c_i2", "(i2 > 1)"],
            vec!["c_i4", "(i4 > 1)"],
            vec!["c_i8", "(i8 > 1)"],
            vec!["c_i2w", "(i2 > 100000)"],
        ]
    );
    // **The values are what the narrowing was for**, so they are asserted beside the text: a
    // `smallint` compared against a literal too wide for it answers rather than raising, and the
    // one that fits answers the same as it always did.
    node.run("INSERT INTO c23 VALUES (1, 5, 5, 5)").unwrap();
    assert_eq!(
        node.rows("SELECT i2 > 1, i2 > 100000, i2 < 100000, i2 = 5 FROM c23"),
        vec![vec!["t", "f", "t", "t"]]
    );
    assert_eq!(
        node.rows("SELECT c_i2, c_i4, c_i8, c_i2w FROM c23"),
        vec![vec!["t", "t", "t", "f"]]
    );
}
