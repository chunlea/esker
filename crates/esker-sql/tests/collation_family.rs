//! **Which expressions PostgreSQL will not derive a collation for**, and which contexts ask.
//!
//! `docs/plans/debts-v1.1.md` #19 recorded one row of this — a real server refuses
//! `GENERATED ALWAYS AS (upper('a')) STORED` with `42P22` and this node builds the column — and
//! the brief was to measure the family before deciding anything. The corpus header states both
//! halves beside the rows that measure them; the short version is:
//!
//! * an operation that **uses** a collation (case mapping, string comparison, `LIKE`) is refused
//!   when every input is a literal, and one that does not (`||`, `length`, `md5`, `substr`,
//!   `btrim`, `CASE`, `COALESCE`, a cast) is accepted. `replace` compares and `substr` does not,
//!   which is the pair that says this is not a rule about names;
//! * only a **column** settles it. An explicit `COLLATE` on a literal does not, in any placement;
//! * and a `DEFAULT` is **not** a collation-requiring context, while a generated column, an index
//!   key and a `CHECK` are — the three whose values get compared.
//!
//! This node has no collation derivation at all, so every refusal above is a divergence in the
//! **accepting** direction: it builds what a real server declines. Each is declared below rather
//! than fixed, because deriving a collation is a type-system change with its own decision to take.
//! The value the node computes is the value a real server would compute if it built the column —
//! `C` and `POSIX` are the only collations it has
//! ([ADR 0076](../../../docs/adr/0076-c-and-posix-are-the-collations-this-node-has.md)) and both
//! order by byte, so nothing here is a wrong *answer*; what differs is which statements exist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// Nothing: the corpus builds the two tables it needs.
const CORPUS_FIXTURE: &[&str] = &[];

/// What this node answers differently, and why.
const DIVERGENCES: parity::Divergences = parity::Divergences {
    types: &[],
    answers: &[
        (
            "ALTER TABLE g1co ADD COLUMN gc_upper_lit text GENERATED ALWAYS AS (upper('a')) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:78",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN gc_upper_collate text GENERATED ALWAYS AS (upper('a' COLLATE \"C\")) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:84",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN gc_upper_cast text GENERATED ALWAYS AS (upper('a'::text)) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:90",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN gc_lower_lit text GENERATED ALWAYS AS (lower('a')) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:93",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN gc_lt_lit boolean GENERATED ALWAYS AS (('a' < 'b')) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:102",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN gc_eq_lit boolean GENERATED ALWAYS AS (('a' = 'b')) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:105",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN gc_like_lit boolean GENERATED ALWAYS AS (('ab' LIKE 'a%')) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:111",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN gc_md5_lit text GENERATED ALWAYS AS (md5('a')) STORED",
            "**A function this node does not have, named.** PostgreSQL accepts `GENERATED ALWAYS AS (md5('a'))` -- `md5` is `provolatile = 'i'`, measured off `pg_proc` -- and this node answers `0A000 the function md5 is not supported`, the same refusal the query path gives for the same name. It used to answer `42P17 functions in index expression must be marked IMMUTABLE`, which was a true sentence about a false premise: `md5` appears nowhere in this crate, so what was wrong was the *reason* and not the refusal. `docs/plans/debts-v1.1.md` #26, and the record of why it had been left that way is kept in `tests/index_expression_volatility.rs`. The suite never sends `md5` -- 0 occurrences in the captured statements, censused -- so implementing it would be building what nothing asks for, which is the C2 contract's own reasoning.",
            "pg19_collation_family.txt:117",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN gc_replace_lit text GENERATED ALWAYS AS (replace('abc', 'b', 'x')) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:126",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN gc_upper_num text GENERATED ALWAYS AS (upper(1::text)) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:147",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN c_lit_collate text GENERATED ALWAYS AS (upper('a' COLLATE \"C\")) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:151",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN c_cast_collate text GENERATED ALWAYS AS (upper('a'::text COLLATE \"C\")) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:154",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN c_outside text GENERATED ALWAYS AS (upper('a') COLLATE \"C\") STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:157",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN c_cmp_collate text GENERATED ALWAYS AS ((('a' COLLATE \"C\") < 'b')) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:160",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN k_upper_collate text GENERATED ALWAYS AS (upper('a' COLLATE \"C\")) STORED",
            "**No collation derivation, so this node builds what a real server declines.** PostgreSQL refuses a *stored* expression whose collation it cannot derive from a column -- `42P22`, naming the operation: `upper()/lower() function`, `string comparison` or `LIKE`. The corpus header carries the whole family and its two halves: which operations need a collation at all (`replace` does and `substr` does not, measured), and the fact that only a **column** settles it -- an explicit `COLLATE` on a literal does not, in any placement, while the same expression in a *query* or a `DEFAULT` is answered. This node has no collation inference: `COLLATE` is recorded per column and never derived through an expression, so each of these builds and stores the value a real server would have computed if it had built it. C3 in the accepting direction, and `docs/plans/debts-v1.1.md` #19 is the row.",
            "pg19_collation_family.txt:164",
        ),
        (
            "SELECT 'r', a.attname, replace(pg_get_expr(d.adbin, d.adrelid), '|', '!') FROM pg_attribute a JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'g1co'::regclass AND a.attnum > 3 ORDER BY a.attnum",
            "**An explicit `COLLATE` is dropped from the stored expression.** `upper(t COLLATE \"C\")` prints `upper((t COLLATE \"C\"))` on a real server and `upper(t)` here: the parser reads the clause and the printed form loses it, so a reader cannot tell a column's own collation from an overridden one. The *value* agrees -- both collations this node has order by byte -- and the other two columns of this row agree to the character, which is what says the difference is the `COLLATE` and not the deparse. Part of #19.",
            "pg19_collation_family.txt:167",
        ),
        (
            "SELECT 'r', conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'g1co'::regclass AND contype = 'c' ORDER BY conname",
            "**A `CHECK` is stored as written, so its literals show no coercion.** `CHECK ((upper(t) = 'A'::text))` there, `CHECK ((upper(t) = 'A'))` here. Not collation: it is the fifth reader of the deparse rule, recorded as `docs/plans/debts-v1.1.md` #25 -- the four ADR 0090 lists plus this one -- and it carries the same read-back requirement, since a check's text is re-parsed to evaluate it on every write.",
            "pg19_collation_family.txt:177",
        ),
        (
            "SELECT 'r', (u COLLATE \"C\") < (t COLLATE \"POSIX\") FROM g1co",
            "**The other half of the same absence: two explicit collations that disagree.** PostgreSQL answers `42P21 collation mismatch between explicit collations` -- a different sqlstate from the indeterminate case, because this one is over-determined rather than under-determined -- and this node compares the two columns and answers. Both collations it has order by byte (ADR 0076), so the answer is the answer either would give; what is missing is the refusal. Its own line because it is a second sqlstate to implement.",
            "pg19_collation_family.txt:179",
        ),
    ],
};

#[test]
fn every_collation_answer_is_postgresql_19_s() {
    let checked = parity::replay(
        include_str!("corpus/pg19_collation_family.txt"),
        CORPUS_FIXTURE,
        &DIVERGENCES,
    );
    assert!(checked > 80, "the corpus shrank: {checked} statements");
}

/// **The context distinction, asserted directly**, because it is the half a reader would doubt.
///
/// The same expression is accepted in a `DEFAULT` and refused in a generated column *on a real
/// server*; this node accepts both. The test pins what the node does today so that the day a
/// collation rule lands, the pair that has to change is named and not searched for.
#[test]
fn a_default_and_a_generated_column_are_the_same_here_and_not_there() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE cf (id int8 PRIMARY KEY, t text)",
        // Accepted on both servers, and PostgreSQL prints the coercion.
        "ALTER TABLE cf ADD COLUMN d text DEFAULT upper('a')",
        // Accepted here, `42P22` there. The corpus declares it; this names it.
        "ALTER TABLE cf ADD COLUMN g text GENERATED ALWAYS AS (upper('a')) STORED",
        "INSERT INTO cf (id, t) VALUES (1, 'x')",
    ]);
    assert_eq!(
        node.rows("SELECT d, g FROM cf WHERE id = 1"),
        vec![vec!["A", "A"]],
        "both compute the value a real server would compute for the one it accepts"
    );
    // And the printed form of the default agrees with the oracle, which is the half that is not a
    // divergence: `upper('a'::text)`, the literal carrying its coercion.
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON \
             d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'cf'::regclass AND \
             a.attname = 'd'"
        ),
        vec![vec!["upper('a'::text)"]],
    );
}
