//! **The parentheses `pg_get_expr` adds**, for a generated column and for a default alike.
//!
//! `virtual_column_test#test_schema_dumping` (run 105) failed on exactly one pair. The catalog was
//! identical on both servers and the dump was otherwise complete — every virtual column,
//! `stored: true`, and the nested casts `length((name)::text)` and `upper((name)::text)` as
//! PostgreSQL prints them — but the node wrote `as: "column1 + 1"` where the suite matches
//! `as: "\(column1 \+ 1\)"`.
//!
//! The rule is [`esker_sql::catalog::ExprShape`]'s, which was measured for an *index* expression
//! and asked in only that one place. `pg_attrdef.adbin` holds one expression and
//! `pg_get_expr(adbin, adrelid)` prints it for both kinds of column, so the shape decides both:
//! an operator takes one pair, a call and a value take none. It is answered where the text is
//! taken (`parse::lower`), because by then `'x)'::text` and `f(x)` are the same characters at the
//! ends.
//!
//! Same family as run 102's check-constraint parenthesisation, in the opposite direction: there
//! the node added a pair PostgreSQL omits.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the two tables it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why: **nothing, as of the deparser unit.**
///
/// This list held six entries and holds none. Four were about the two shapes below and two about
/// the type of `information_schema.columns.column_default`, and all six are gone for reasons worth
/// keeping, because each says something about how a register goes stale:
///
/// * **the nested pairs** (`(c1 * 2 + 3)` back as `((c1 * 2) + 3)`, `(c1 > 0 AND c2 > 0)` back as
///   `((c1 > 0) AND (c2 > 0))`) said they needed "a deparser this node does not have". It had one:
///   `exec::ddl::deparse` printed exactly this rule and was reachable from one caller, the index
///   key list. The entries were not wrong about the rule, they were wrong about the code, which
///   is what [ADR 0075](../../../docs/adr/0075-the-oracle-captures-live-in-the-repository.md)
///   asks a provenance line to make checkable — `docs/plans/debts-v1.1.md` #17;
/// * **a literal's coercion inside a call** (`upper('a')` back as `upper('a'::text)`) said a
///   default "is not stored deparsed, and giving it that treatment is the same unit". It was the
///   same unit, and this is it: one `pg_attrdef` row, one printer, three statements that store one;
/// * **`column_default`'s declared type** closed on somebody else's commit. It is
///   `character_data`, a domain over `character varying`, and the node said `text` until the
///   catalog's `name` unit landed. Nobody came back to this file, which is what
///   [ADR 0031](../../../docs/adr/0031-rails-compatibility-is-measured.md) rule 2 is for — "an
///   entry that starts passing is deleted, and the deletion is a commit" — and is why the
///   harness reports an agreeing entry rather than trusting a reader to notice.
///
/// Two of the six could not have been reported by rule 2 at all before this unit, and that was a
/// hole in the harness rather than in this file: rule 2 asked `actual == expected` for a listed
/// entry, where the corpus line declares no types and the comparison the rest of the harness uses
/// says an undeclared types column pins nothing. Fixed in `parity_harness/mod.rs` in the same
/// commit — an entry on a types-less line was previously *permanently* invisible to rule 2.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[],
};

#[test]
fn every_printed_expression_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_generated_parens.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 35, "the corpus shrank: {checked} statements");
}

/// What this node answers differently on the shape corpus, and why.
const SHAPE_DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "SELECT 'r', a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1dp2'::regclass AND a.attname = 'x_lit'",
            "**One integer literal type, so a literal's cast is gone before the printer runs.**          `GENERATED ALWAYS AS (1::bigint)` prints `(1)::bigint` on a real server and `1` here.          PostgreSQL's rule is `get_const_expr`'s: a numeric `Const` shows its type when that type          is not the one the literal form defaults to, and `1` defaults to `integer`, so an `int8`          `1` needs the cast to re-parse as itself. This node resolves every integer literal to one          type (`docs/plans/debts-v1.1.md` #12, and it is b4's), so `1` and `1::bigint` are the          same resolved literal and there is nothing left to print the cast from. Not a deparser          gap: the sibling row `x_lit_expr` — `(1 + 1)::bigint` — agrees, because there the cast is          over an operator and survives as a `Cast` node. The *default* spelling agrees too          (`d_cast`), from `parse::lower::cast_default_text`, which reads the written text before          anything folds it. **It survived the landing that was supposed to close it**, and that is worth recording because the guess was reasonable and wrong: ADR 0086 (a folded cast keeps the type it named) and ADR 0087 (an integer literal is the narrowest type that holds it) are both in the tree, so `1` is an `int4` and `1::bigint` an `int8` and the two are no longer the same literal. The row still differs one layer further out, in the **printer**: `exec::ddl::deparse_literal` prints a number bare whatever its type, while PostgreSQL's `get_const_expr` shows the type whenever it is not the one the literal form defaults to. So this is now a one-arm change with its own measurement to take -- which constants show a cast, for each of the six numeric types and in each position -- and is `docs/plans/debts-v1.1.md` #22 rather than #12.",
            "pg19_deparse_parens.txt:118",
        ),
        (
            "ALTER TABLE g1dp2 ADD COLUMN f_literal text GENERATED ALWAYS AS (upper('a')) STORED",
            "**No collation derivation, so a text call over nothing but literals is accepted          where a real server refuses it.** PostgreSQL answers `42P22 could not determine which          collation to use for upper() function`: a generated column's expression must have a          determinable collation, a literal argument carries none, and the column's own is not          consulted. Measured, and it is the reason this corpus tests a literal argument as a          *default* (`g1dp3`) rather than as a generated column. This node has no collation          inference at all — `COLLATE` is recorded per column and never derived through an          expression — so it builds the column and stores `upper('a'::text)`, which is what the          same expression prints as a default here and there. C3, in the direction that accepts          more than the oracle; the value it computes is the value a real server would compute if          it built it.",
            "pg19_deparse_parens.txt:141",
        ),
        (
            "SELECT 'r', a.attname, pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1dp2'::regclass AND a.attname = 'c_notin'",
            "**`NOT IN` prints as a quantifier this node's expression language does not have.** \
         PostgreSQL says `(c1 <> ALL (ARRAY[1, 2]))`. There is an `Expr::AnyArray` here and no \
         `AllArray` -- `= ANY` exists because a query needs it, `<> ALL` has never been written -- \
         so the printed form cannot be parsed back at all, and `reads_back` keeps the written \
         `(c1 NOT IN (1, 2))` rather than storing a string the next `INSERT` would raise `XX000` \
         on. Recorded as `docs/plans/debts-v1.1.md` #21: it is a **language** gap and not a \
         printer one, and the printer is already right for the day the node has the node.",
            "pg19_deparse_parens.txt:129",
        ),
    ],
};

/// **The deparser's specification**, one row per shape, taken from the oracle.
///
/// The corpus above asks the question a column at a time and was enough to find that the printed
/// form differs from the written one. It is not enough to say *how*: two of its rows carried the
/// whole nested-parentheses rule and a reader had to infer the rest. This one is 38 shapes chosen
/// to separate the rules from each other — precedence from association, a cast's pair from its
/// operand's, a call's argument from the call, a desugaring from a parenthesisation — and its
/// header states each rule beside the row that measures it.
///
/// Four of them are why this file exists rather than a second `ExprShape` variant:
/// `BETWEEN`, `IN` and `LIKE` come back as the *operators* they desugar to
/// (`((a >= x) AND (a <= y))`, `(a = ANY (ARRAY[…]))`, `(a ~~ 'p'::text)`) and `- 1` comes back as
/// `'-1'::integer`. No rule about parentheses over the written text can produce any of the four;
/// they need the tree, which is what [`esker_sql`]'s `exec::ddl::deparse` walks.
#[test]
fn every_deparsed_shape_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_deparse_parens.txt"),
        CORPUS_FIXTURE,
        &SHAPE_DIVERGENCES,
    );
    assert!(checked > 70, "the corpus shrank: {checked} statements");
}
