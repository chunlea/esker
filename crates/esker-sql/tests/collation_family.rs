//! **Which expressions PostgreSQL will not derive a collation for**, and which contexts ask.
//!
//! `docs/plans/debts-v1.1.md` #19 recorded one row of this — a real server refuses
//! `GENERATED ALWAYS AS (upper('a')) STORED` with `42P22` and this node builds the column — and
//! the brief was to measure the family before deciding anything. The corpus header states both
//! halves beside the rows that measure them; the short version is:
//!
//! * an operation that **uses** a collation is refused when every input is a literal, and one
//!   that does not is accepted. `replace` compares and `substr` does not, which is the pair that
//!   says this is not a rule about names. **Six operation names, not three** — the placement-by-
//!   placement corpus reached `upper()`, `lower()` and `string comparison`; asking 43 shapes in
//!   one session (`which_operations_need_a_collation`, `tests/captures/pg19_collation_operations.txt`)
//!   added `initcap() function`, `ILIKE` beside `LIKE`, and `regular expression`, and found
//!   `greatest`, `least`, `nullif`, `strpos`, `split_part`, `string_to_array` and
//!   `array_position` in the `string comparison` bucket;
//! * only a **column** settles it. An explicit `COLLATE` on a literal does not, in any placement —
//!   `upper(t COLLATE "C")` builds and `upper('a' COLLATE "C")` is `42P22`, **both being
//!   explicit**, which is what says a clause names an ordering and does not make one derivable;
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
//! **This node had no collation derivation at all and now has three of ADR 0096's four rules**, so
//! the sixteen divergences this file declared are down to **two**: `md5`, which is a function this
//! node does not have (`debts-v1.1.md` #26), and `upper('a'::text COLLATE "C")`, which is a
//! *parser* gap — `sqlparser` has no infix production for the clause after a cast, so the same
//! expression with parentheses gets the right `42P22`.
//! [ADR 0096](../../../docs/adr/0096-a-collation-is-derived-from-a-column-or-from-nothing.md) is
//! the decision and it is written from this file's measurements.
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
            "ALTER TABLE g1co ADD COLUMN gc_md5_lit text GENERATED ALWAYS AS (md5('a')) STORED",
            "**A function this node does not have, named.** PostgreSQL accepts `GENERATED ALWAYS AS (md5('a'))` -- `md5` is `provolatile = 'i'`, measured off `pg_proc` -- and this node answers `0A000 the function md5 is not supported`, the same refusal the query path gives for the same name. It used to answer `42P17 functions in index expression must be marked IMMUTABLE`, which was a true sentence about a false premise: `md5` appears nowhere in this crate, so what was wrong was the *reason* and not the refusal. `docs/plans/debts-v1.1.md` #26, and the record of why it had been left that way is kept in `tests/index_expression_volatility.rs`. The suite never sends `md5` -- 0 occurrences in the captured statements, censused -- so implementing it would be building what nothing asks for, which is the C2 contract's own reasoning.",
            "pg19_collation_family.txt:117",
        ),
        (
            "ALTER TABLE g1co ADD COLUMN c_cast_collate text GENERATED ALWAYS AS (upper('a'::text COLLATE \"C\")) STORED",
            "**A parser gap, and no longer a collation one** -- which is the last of the sixteen this file declared and the only one left that is about `COLLATE` at all. Since ADR 0096's third family this node gives the right `42P22` for the *same* expression written `upper(('a'::text) COLLATE \"C\")`; without the parentheses it is `42601 No infix parser for token COLLATE`, because `sqlparser` has no infix production for the clause **after a cast**. So the value a real server refuses is refused here too, and what differs is the sentence and the class. Measured 2026-09-10; `docs/plans/debts-v1.1.md` #19 kept the row for the one operand shape the grammar cannot read.",
            "pg19_collation_family.txt:154",
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
/// The same expression is accepted in a `DEFAULT` and refused in a generated column, on both
/// servers now — this test used to say *"this node accepts both"* and named the pair that would
/// have to change on the day a collation rule landed. That day was 2026-09-10 (ADR 0096, third
/// family), so the pair is here, changed, and what it pins is that the `DEFAULT` did **not**
/// move: a rule written one context too wide would have taken it with the generated column.
#[test]
fn a_default_is_not_a_collation_requiring_context_and_a_generated_column_is() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE cf (id int8 PRIMARY KEY, t text)",
        // Accepted on both servers, and PostgreSQL prints the coercion.
        "ALTER TABLE cf ADD COLUMN d text DEFAULT upper('a')",
        "INSERT INTO cf (id, t) VALUES (1, 'x')",
    ]);
    // And the generated column is the refusal, naming the operation.
    assert_eq!(
        node.answer("ALTER TABLE cf ADD COLUMN g text GENERATED ALWAYS AS (upper('a')) STORED")
            .to_string(),
        "!42P22 could not determine which collation to use for upper() function \
         HINT: Use the COLLATE clause to set the collation explicitly."
    );
    // **The same expression over the column builds**, which is what makes the refusal about the
    // derivation rather than about `upper`.
    assert_eq!(
        node.answer("ALTER TABLE cf ADD COLUMN g text GENERATED ALWAYS AS (upper(t)) STORED")
            .to_string(),
        "(a command, no result set)"
    );
    assert_eq!(
        node.rows("SELECT d, g FROM cf WHERE id = 1"),
        vec![vec!["A", "X"]],
        "the default is unchanged and the generated column computes from the row"
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

/// **Which operations need a collation, all 43 at once** — [ADR 0096](../../../docs/adr/0096-a-collation-is-derived-from-a-column-or-from-nothing.md)'s
/// third family, and the census `tests/captures/pg19_collation_operations.txt` holds.
///
/// The corpus beside this file measured the family one placement at a time and reached three
/// operation names. Asking 43 shapes in one session found **six**, and the three it did not have
/// are the ones a reader would not guess: `initcap() function` has a name of its own, `LIKE` and
/// `ILIKE` are two names for one node, and a regular-expression match is `regular expression`.
///
/// **`string comparison` is much wider than `<`.** A function that *compares* needs a collation
/// and one that only *cuts* does not — `replace`, `split_part`, `strpos`, `string_to_array`,
/// `greatest`, `least`, `nullif` and `array_position` do; `substr`, `substring`, `btrim`, `ltrim`,
/// `rtrim`, `reverse`, `ascii`, `length` and `||` do not. `COALESCE` picks rather than compares
/// and does not, where `CASE` does, through the comparison in its `WHEN`.
///
/// **The type decides, not the operator**: `(1 = 2)` and `(1 < 2)` are accepted and `('a' = 'b')`
/// is not.
///
/// **41 of the 43 agree with 19beta1 exactly.** The two that do not are `md5('a')` and
/// `initcap('a')`, and neither is about collation: this node does not have either function, so it
/// answers `0A000` where a real server answers (`md5`) or raises `42P22` (`initcap`). `md5` is
/// `debts-v1.1.md` #26 and is already declared in the corpus; they are left out of the table
/// below rather than pinned under a name that would be wrong about why.
#[allow(
    clippy::too_many_lines,
    reason = "one line per measured shape; the census is the test, and splitting it would put \
              half the table in a function named after nothing"
)]
#[test]
fn which_operations_need_a_collation() {
    /// Accepted: a DDL statement answers with no result set.
    const OK: &str = "OK";
    /// Refused for a different reason, and the census records which — not every refusal here is
    /// this family's, and one that reported them together would put two immutability rows in a
    /// collation list.
    const IMMUT: &str = "IMMUT";
    // **The operation name, or `OK`/`IMMUT`.** Written as the *name* rather than the whole
    // sentence so the table reads as the census it is; the sentence is assembled once below and
    // is PostgreSQL's own, `HINT` included.
    let cases: &[(&str, &str)] = &[
        (
            "ALTER TABLE g ADD COLUMN c0 text GENERATED ALWAYS AS (lower('a')) STORED",
            "lower() function",
        ),
        (
            "ALTER TABLE g ADD COLUMN c1 text GENERATED ALWAYS AS (upper('a')) STORED",
            "upper() function",
        ),
        (
            "ALTER TABLE g ADD COLUMN c2 text GENERATED ALWAYS AS (reverse('a')) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c3 int GENERATED ALWAYS AS (ascii('a')) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c4 int GENERATED ALWAYS AS (length('a')) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c5 int GENERATED ALWAYS AS (octet_length('a')) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c6 int GENERATED ALWAYS AS (abs(1)) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c7 text GENERATED ALWAYS AS (('a' || 'b')) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c8 text GENERATED ALWAYS AS (concat('a','b')) STORED",
            "IMMUT",
        ),
        (
            "ALTER TABLE g ADD COLUMN c9 text GENERATED ALWAYS AS (split_part('a,b', ',', 1)) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c10 text[] GENERATED ALWAYS AS (string_to_array('a,b', ',')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c11 int GENERATED ALWAYS AS (strpos('abc','b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c12 text GENERATED ALWAYS AS (btrim(' a ')) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c13 text GENERATED ALWAYS AS (ltrim(' a ')) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c14 text GENERATED ALWAYS AS (rtrim('a ')) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c15 text GENERATED ALWAYS AS (greatest('a','b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c16 text GENERATED ALWAYS AS (least('a','b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c17 text GENERATED ALWAYS AS (nullif('a','b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c18 text GENERATED ALWAYS AS (substr('abc',1,2)) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c19 text GENERATED ALWAYS AS (substring('abc' from 1 for 2)) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c20 text GENERATED ALWAYS AS (replace('abc','b','x')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c21 int GENERATED ALWAYS AS (array_position(ARRAY['a'],'a')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c22 tsvector GENERATED ALWAYS AS (to_tsvector('a')) STORED",
            "IMMUT",
        ),
        (
            "ALTER TABLE g ADD COLUMN c23 boolean GENERATED ALWAYS AS ((to_tsvector('a') @@ to_tsquery('b'))) STORED",
            "IMMUT",
        ),
        (
            "ALTER TABLE g ADD COLUMN c24 boolean GENERATED ALWAYS AS (('a' < 'b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c25 boolean GENERATED ALWAYS AS (('a' = 'b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c26 boolean GENERATED ALWAYS AS (('a' <> 'b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c27 boolean GENERATED ALWAYS AS (('a' >= 'b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c28 boolean GENERATED ALWAYS AS (('ab' LIKE 'a%')) STORED",
            "LIKE",
        ),
        (
            "ALTER TABLE g ADD COLUMN c29 boolean GENERATED ALWAYS AS (('ab' ILIKE 'a%')) STORED",
            "ILIKE",
        ),
        (
            "ALTER TABLE g ADD COLUMN c30 boolean GENERATED ALWAYS AS (('a' ~ 'b')) STORED",
            "regular expression",
        ),
        (
            "ALTER TABLE g ADD COLUMN c31 boolean GENERATED ALWAYS AS (('a' ~* 'b')) STORED",
            "regular expression",
        ),
        (
            "ALTER TABLE g ADD COLUMN c32 boolean GENERATED ALWAYS AS (('a' IN ('b'))) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c33 boolean GENERATED ALWAYS AS (('a' BETWEEN 'a' AND 'b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c34 text GENERATED ALWAYS AS ((CASE WHEN 'a' = 'b' THEN 'x' ELSE 'y' END)) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c35 text GENERATED ALWAYS AS (COALESCE('a','b')) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c36 boolean GENERATED ALWAYS AS ((1 = 2)) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c37 boolean GENERATED ALWAYS AS ((1 < 2)) STORED",
            "OK",
        ),
        (
            "ALTER TABLE g ADD COLUMN c38 boolean GENERATED ALWAYS AS ((ARRAY['a'] = ARRAY['b'])) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c39 boolean GENERATED ALWAYS AS ((('a')::varchar < 'b')) STORED",
            "string comparison",
        ),
        (
            "ALTER TABLE g ADD COLUMN c40 boolean GENERATED ALWAYS AS ((('a')::char(1) < 'b')) STORED",
            "string comparison",
        ),
    ];
    for (statement, want) in cases {
        let expected = match *want {
            OK => "(a command, no result set)".to_owned(),
            IMMUT => "!42P17 generation expression is not immutable".to_owned(),
            operation => format!(
                "!42P22 could not determine which collation to use for {operation} \
                 HINT: Use the COLLATE clause to set the collation explicitly."
            ),
        };
        let mut node = parity::Node::new(&["CREATE TABLE g (t text, n integer)"]);
        assert_eq!(node.answer(statement).to_string(), expected, "{statement}");
    }
}

/// **And a column settles every one of them**, which is the lower bound the table above needs.
///
/// The same operations over a **column** are all accepted, on both servers: the refusal is about
/// having nowhere to derive an ordering from, not about the operation. Measured — `upper(t)`,
/// `(t < 'b')`, `(t LIKE 'a%')` and `replace(t, 'b', 'x')` all build.
///
/// **And a cast of a non-collatable column counts**: `upper(n::text)` over an `integer` column is
/// accepted where `upper(1::text)` is not, which is what says the rule is "a column somewhere in
/// the operand" and not "a collatable column".
#[test]
fn a_column_anywhere_under_the_operation_settles_it() {
    for (n, expression) in [
        "upper(t)",
        "(t < 'b')",
        "(t LIKE 'a%')",
        "replace(t, 'b', 'x')",
        "upper(n::text)",
        "upper(COALESCE(t, 'a'))",
        "(CASE WHEN t = 'a' THEN 'x' ELSE 'y' END)",
    ]
    .into_iter()
    .enumerate()
    {
        let mut node = parity::Node::new(&["CREATE TABLE g (t text, n integer)"]);
        let statement = format!(
            "ALTER TABLE g ADD COLUMN k{n} text GENERATED ALWAYS AS (({expression})::text) STORED"
        );
        assert_eq!(
            node.answer(&statement).to_string(),
            "(a command, no result set)",
            "{statement}"
        );
    }
}

/// **And the other four stored contexts do not ask**, which is the half the corpus header had
/// backwards for a day.
///
/// `upper('a')` is accepted in a `DEFAULT`, in an index key, in an index **predicate** and in a
/// `CHECK` on 19beta1 — re-measured 2026-09-10, the index and the constraint were built — and
/// refused only in a generated column. A rule written at the wrong place would have refused four
/// statements a real server answers.
#[test]
fn only_a_generated_column_asks() {
    let mut node = parity::Node::new(&["CREATE TABLE g (t text, n integer)"]);
    for statement in [
        "ALTER TABLE g ADD COLUMN d text DEFAULT upper('a')",
        "CREATE INDEX g_ix1 ON g ((upper('a')))",
        "CREATE INDEX g_ix2 ON g (n) WHERE upper('a') = 'A'",
        "ALTER TABLE g ADD CONSTRAINT g_ck CHECK (upper('a') = 'A')",
    ] {
        assert_eq!(
            node.answer(statement).to_string(),
            "(a command, no result set)",
            "{statement}"
        );
    }
    // And a query answers it, which is where this family started.
    assert_eq!(node.rows("SELECT upper('a')"), vec![vec!["A"]]);
}
