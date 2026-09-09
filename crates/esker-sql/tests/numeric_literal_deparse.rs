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
    answers: &[
        (
            "SELECT 'r', a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1nl'::regclass AND a.attnum > 6 ORDER BY a.attnum",
            "**Three of this row's 32 columns differ, and none of the three is about the form a \
         constant prints in** — the forms all agree now. What differs is which *node* the tree \
         holds:\n\
         \n\
         `c_i2_lit` — `i2 > 1` is `(i2 > 1)` there and `(i2 > (1)::smallint)` here, because \
         `exec::query::retype` narrows a comparison's literal to the column's type while \
         PostgreSQL picks an `int24` operator and leaves the literal an `integer`. The values \
         compare identically; only the printed text sees it. Narrowing is right for an assignment \
         and this is a comparison, which is the sentence `retype`'s own `numeric` arm already \
         carries — extending it to every width is its own unit, and it is b4's literal ladder.\n\
         \n\
         `c_i8_cast` — `i8 > 1::bigint` is `(i8 > (1)::bigint)` there and `(i8 > 1)` here: the \
         written cast is folded into the constant and then normalised to `Literal::Integer`, \
         which loses the `int8`. Same cause as `k_neg_par`.\n\
         \n\
         `f_grt`/`f_grt_i8` — `greatest(i4, 1)` is `GREATEST(i4, 1)` there. PostgreSQL \
         **upper-cases the four functions that are grammar productions** rather than catalog \
         entries — `COALESCE`, `GREATEST`, `LEAST`, `NULLIF`, measured — and it coerces the \
         literal to the common type (`GREATEST(i8, (1)::bigint)`). Here they keep their written \
         text, because `deparse`'s `CatalogFunc` arm is a `name(...)` placeholder and \
         `exec::ddl::reads_back` refuses to store one. Giving that arm its arguments would change \
         the printed text of every expression over a catalog function at once, with no measurement \
         behind most of them, so it is named as a debt rather than done here.",
            "pg19_numeric_literal_deparse.txt:81",
        ),
        (
            "SELECT 'r', a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1nd'::regclass ORDER BY a.attnum",
            "**One of nine, and it is the written text rather than a deparse.** `DEFAULT \
         -1::bigint` is `(- (1)::bigint)` on a real server — `::` binds tighter than unary minus, \
         so it is an operator over a cast — and `(-1::BIGINT)` here, which is `sqlparser`'s own \
         rendering of the expression as written, type name upper-cased and the cast un-deparsed. \
         The other eight agree, including the three forms this unit implemented and the fact that \
         a `DEFAULT` stores the expression rather than the assignment cast to the column. Closing \
         this row means the `DEFAULT` path deparsing a shape it currently keeps verbatim, which is \
         the same seam [ADR 0090](../../../docs/adr/0090-a-stored-expression-is-deparsed-by-the-statement-that-writes-it.md)'s \
         allow-list draws and the same reason: what is kept verbatim is kept because deparsing it \
         would lose something, and here it loses the case of a type name.",
            "pg19_numeric_literal_deparse.txt:83",
        ),
        (
            "SELECT 'r', conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'g1nc'::regclass ORDER BY conname",
            "**A `CHECK` is stored as written and is the fifth reader of this rule**, which is the \
         shape of every bug in this family: `pg_get_constraintdef` prints \
         `CHECK ((i8 > (1)::bigint))` there and `CHECK ((i8 > 1::BIGINT))` here — `sqlparser`'s \
         upper-cased type name in text that never passed through `deparse`. The other two \
         constraints in the row agree to the character, which is what says the difference is the \
         cast and not the constraint. Routing a `CHECK` through the deparser is the fifth caller \
         after the four ADR 0090 lists, and it carries the same read-back requirement: the text is \
         re-parsed to evaluate the constraint on every write.",
            "pg19_numeric_literal_deparse.txt:91",
        ),
    ],
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
