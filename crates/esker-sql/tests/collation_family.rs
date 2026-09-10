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
//! * and **one context asks**: a generated column. A `DEFAULT`, an index expression, an index
//!   predicate and a `CHECK` all accept `upper('a')` — corrected 2026-09-10 against this file's
//!   own corpus rows, which said so from the start while its header said `42P22` for three of
//!   them. The reading that came with the wrong table — *"the three whose values get compared"* —
//!   is refuted by it, an index key and a `CHECK` being compared and not asking.
//!
//! **And there are two mechanisms, which an empty table cannot tell apart.** A conflict between
//! two *implicit* collations (`u < v`, `C` against `POSIX`) is an **evaluation-time** error, in a
//! query and in a generated column alike, and never fires on an empty table; a generated column's
//! expression needing a collation derivable from a column is a **DDL** check, with rows or
//! without. Every probe in this corpus ran against an empty table, which is why the first reading
//! merged them.
//!
//! This node has no collation derivation at all, so every refusal above is a divergence in the
//! **accepting** direction: it builds what a real server declines. Each is declared below rather
//! than fixed until
//! [ADR 0096](../../../docs/adr/0096-a-collation-is-derived-from-a-column-or-from-nothing.md),
//! which is the decision and which is written from this file's measurements.
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
    ],
};

/// **A `COLLATE` written on a stored expression is printed back**, which is what keeping the
/// clause in the plan is for — [ADR 0096](../../../docs/adr/0096-a-collation-is-derived-from-a-column-or-from-nothing.md)'s
/// first family.
///
/// Measured on 19beta1: `upper(t COLLATE "C")` is stored and printed as `upper((t COLLATE "C"))`,
/// with a pair of its own, and this node printed `upper(t)` — the catalog disagreeing with the
/// statement that wrote it. The corpus row it came off is `pg19_collation_family.txt`'s
/// `pg_get_expr` read-back, and this is the same fact asserted where a reader looks for it.
///
/// **And the value is unchanged**, which is the half that says the clause is about ordering: both
/// collations this node has are byte order (ADR 0076), so nothing a row holds moves.
#[test]
fn a_collate_clause_survives_into_the_stored_expression() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE cc (id int8 PRIMARY KEY, t text)",
        "ALTER TABLE cc ADD COLUMN g text GENERATED ALWAYS AS (upper(t COLLATE \"C\")) STORED",
        "INSERT INTO cc (id, t) VALUES (1, 'ab')",
    ]);
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON \
             d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'cc'::regclass AND \
             a.attname = 'g'"
        ),
        vec![vec!["upper((t COLLATE \"C\"))"]]
    );
    assert_eq!(node.rows("SELECT g FROM cc"), vec![vec!["AB"]]);
    // **And a bare column reference keeps none**, so the pair above is the clause and not a
    // parenthesis this node adds to everything.
    let mut node = parity::Node::new(&[
        "CREATE TABLE cd (id int8 PRIMARY KEY, t text)",
        "ALTER TABLE cd ADD COLUMN g text GENERATED ALWAYS AS (upper(t)) STORED",
    ]);
    assert_eq!(
        node.rows(
            "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attribute a JOIN pg_attrdef d ON \
             d.adrelid = a.attrelid AND d.adnum = a.attnum WHERE a.attrelid = 'cd'::regclass AND \
             a.attname = 'g'"
        ),
        vec![vec!["upper(t)"]]
    );
}

/// **Two explicit `COLLATE` clauses that disagree are `42P21`** — [ADR 0096](../../../docs/adr/0096-a-collation-is-derived-from-a-column-or-from-nothing.md)'s
/// second family, and the one rule of the three that needs no scope at all.
///
/// It is decidable from the expression's own shape, so it runs at lowering and covers a query, an
/// `ORDER BY` and a generated column from one place — which is what a real server does. Every row
/// below was measured on 19beta1 on 2026-09-10, and **the rule is wider than a comparison**:
/// `||`, `COALESCE` and `CASE` raise it too, and it propagates up through a function.
///
/// **No `HINT`**, which is the pair of facts that tells it from `42P22`: there the user named
/// nothing and the hint asks for a clause; here they named two and there is nothing to suggest.
#[test]
fn two_explicit_collations_that_disagree_are_a_mismatch() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g (t text, u text COLLATE \"C\", v text COLLATE \"POSIX\")",
        "INSERT INTO g VALUES ('a','b','c')",
    ]);
    let mismatch = "!42P21 collation mismatch between explicit collations \"C\" and \"POSIX\"";
    for statement in [
        // Two columns, each with a clause of its own.
        "SELECT (u COLLATE \"C\") < (t COLLATE \"POSIX\") FROM g",
        // **Concatenation**, which needs no collation at all and still merges the two.
        "SELECT ('a' COLLATE \"C\") || ('b' COLLATE \"POSIX\") FROM g",
        // **Up through a function**: the clause inside `upper` is what the comparison meets.
        "SELECT upper('a' COLLATE \"C\") < ('b' COLLATE \"POSIX\") FROM g",
        "SELECT COALESCE('a' COLLATE \"C\", 'b' COLLATE \"POSIX\") FROM g",
        "SELECT CASE WHEN true THEN 'a' COLLATE \"C\" ELSE 'b' COLLATE \"POSIX\" END FROM g",
        // Not only the target list.
        "SELECT t FROM g ORDER BY (u COLLATE \"C\") < (v COLLATE \"POSIX\")",
    ] {
        assert_eq!(node.answer(statement).to_string(), mismatch, "{statement}");
    }
}

/// **And the three shapes that are *not* a mismatch**, each measured, because a rule with no lower
/// bound refuses more than it was asked to.
///
/// * the **same** clause on both sides is fine;
/// * **explicit against implicit** is fine — the explicit one wins, which is the whole of what
///   `Derivation` is for;
/// * a **nested** clause is fine, and the **outer** one wins:
///   `(('a' COLLATE "C") COLLATE "POSIX") < 'b'` is answered on 19beta1, so a rule that collected
///   every clause in the tree and compared them would refuse a statement a real server answers.
#[test]
fn a_clause_that_wins_is_not_a_mismatch() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g (t text, u text COLLATE \"C\", v text COLLATE \"POSIX\")",
        "INSERT INTO g VALUES ('a','b','c')",
    ]);
    for statement in [
        "SELECT ('a' COLLATE \"C\") < ('b' COLLATE \"C\") FROM g",
        "SELECT ('a' COLLATE \"C\") < v FROM g",
        "SELECT ((\'a\' COLLATE \"C\") COLLATE \"POSIX\") < 'b' FROM g",
    ] {
        assert_eq!(node.rows(statement), vec![vec!["t"]], "{statement}");
    }
    // And the value is still the operand's: a clause is an ordering, never a rendering.
    assert_eq!(node.rows("SELECT t COLLATE \"C\" FROM g"), vec![vec!["a"]]);
}

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
