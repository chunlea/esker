//! **`pg_get_constraintdef(oid, true)` parenthesises a `CHECK` expression once, not twice.**
//!
//! Five tests in `check_constraint_test.rb` read a check constraint back and get one pair of
//! parentheses too many. `ActiveRecord` asks for the *pretty* form and this node ignored the flag:
//!
//! ```text
//! test_check_constraints        Expected: "price > discounted_price"  Actual: "(price > discounted_price)"
//! test_add_check_constraint     Expected: "quantity > 0"              Actual: "(quantity > 0)"
//! test_remove_check_constraint  Expected: "price > 0"                 Actual: "(price > 0)"
//! test_schema_dumping_with_validate_false   add_check_constraint "trades", "(quantity > 0)", … validate: false
//! test_schema_dumping_with_validate_true    t.check_constraint "(quantity > 0)", name: "quantity_check"
//! ```
//!
//! All five are one mechanism, and the last two are the first three reaching the schema dump —
//! `SchemaDumper` writes whatever `check_constraints` returns.
//!
//! # Measured on 19beta1
//!
//! ```text
//! conname       | pg_get_constraintdef(oid)  == (oid, false) | pg_get_constraintdef(oid, true)
//! g1cp_simple   | CHECK ((quantity > 0))                     | CHECK (quantity > 0)
//! g1cp_two_cols | CHECK ((price > discounted_price))         | CHECK (price > discounted_price)
//! g1cp_notvalid | CHECK ((price > 100)) NOT VALID            | CHECK (price > 100) NOT VALID
//! g1cc_p_pkey   | PRIMARY KEY (id)                           | PRIMARY KEY (id)
//! g1cc_u        | UNIQUE (a, "position")                     | UNIQUE (a, "position")
//! g1cc_f_…_fkey | FOREIGN KEY (pid) REFERENCES …             | FOREIGN KEY (pid) REFERENCES …
//! …_id_not_null | NOT NULL id                                | NOT NULL id
//! ```
//!
//! Three things the capture settled that a guess would have got wrong:
//!
//! * **The default is the non-pretty form.** `pg_get_constraintdef(oid)` equals
//!   `pg_get_constraintdef(oid, false)` exactly, so the doubled parentheses this node already
//!   printed were right for the spelling every other reader uses — `unique_constraints` and
//!   `foreign_keys` both send the one-argument form. Only `check_constraints` asks for pretty.
//! * **`pretty` changes nothing for the other five contypes.** Measured over `p`, `u`, `f`, `x`
//!   and `n`: byte-identical. So the flag is a `CHECK`-only concern and threading it cannot
//!   disturb the readers that do not pass it.
//! * **It does not wrap.** The doc comment this file replaces claimed the flag "re-wraps a long
//!   `CHECK` expression on a real server". It does not: `strpos(…, chr(10))` is `0` for a
//!   42-character predicate, a 153-character nested one and a 223-character conjunction of eight.
//!   `PRETTYFLAG_INDENT` reaches `pg_get_indexdef`'s column lists, not this. **That false premise
//!   is why the flag was ignored** — the belief was that honouring it meant reflowing text.
//!
//! # What is still divergent, deliberately
//!
//! PostgreSQL's pretty form is a **deparse of the stored tree** and this node prints the text the
//! user wrote (`catalog::CheckDef`), so the two agree only where the written text is already
//! canonical — which is the case for every predicate `ActiveRecord` writes, and not for
//! `status IN ('a','b')`, which a real server prints back as
//! `status = ANY (ARRAY['a'::text, 'b'::text])`. That trade is declared in `check_constraint.rs`
//! and is unchanged here; this file is about the parentheses the *printer* adds, which are the
//! node's own either way.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] = &[
    "CREATE TABLE g1cp_products (price integer, discounted_price integer, \
     CONSTRAINT products_price_check CHECK (price > discounted_price))",
    "CREATE TABLE g1cp_trades (id integer PRIMARY KEY, quantity integer, price integer, \
     CONSTRAINT quantity_check CHECK (quantity > 0))",
    "ALTER TABLE g1cp_trades ADD CONSTRAINT price_check CHECK (price > 100) NOT VALID",
    "CREATE TABLE g1cp_chain (a integer, b integer, \
     CONSTRAINT chain_check CHECK (a > 0 AND b > 0))",
];

/// `pg_get_constraintdef(oid, <flag>)` for one constraint by name.
fn def(node: &mut parity::Node, conname: &str, flag: &str) -> String {
    node.rows(&format!(
        "SELECT pg_get_constraintdef(oid{flag}) FROM pg_constraint WHERE conname = '{conname}'"
    ))
    .first()
    .and_then(|row| row.first().cloned())
    .unwrap_or_else(|| panic!("no pg_constraint row named {conname}"))
}

/// **The pretty form, which is the one `ActiveRecord` asks for.**
#[test]
fn the_pretty_form_parenthesises_the_expression_once() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        def(&mut node, "quantity_check", ", true"),
        "CHECK (quantity > 0)"
    );
    assert_eq!(
        def(&mut node, "products_price_check", ", true"),
        "CHECK (price > discounted_price)"
    );
}

/// **The one-argument form is unchanged**, because it is what `unique_constraints` and
/// `foreign_keys` send and what the corpus captures pin.
#[test]
fn the_default_and_the_false_form_keep_the_doubled_parentheses() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        def(&mut node, "quantity_check", ""),
        "CHECK ((quantity > 0))"
    );
    assert_eq!(
        def(&mut node, "quantity_check", ", false"),
        "CHECK ((quantity > 0))"
    );
}

/// `NOT VALID` is outside the parentheses in both forms — the half `validate: false` is read from.
#[test]
fn not_valid_prints_after_the_closing_parenthesis_in_both_forms() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        def(&mut node, "price_check", ", true"),
        "CHECK (price > 100) NOT VALID"
    );
    assert_eq!(
        def(&mut node, "price_check", ""),
        "CHECK ((price > 100)) NOT VALID"
    );
}

/// Measured: for every contype but `c` the flag changes nothing, so passing it cannot move a
/// primary key, a unique constraint or a `NOT NULL` that some other reader depends on.
#[test]
fn the_flag_changes_nothing_for_the_other_kinds() {
    let mut node = parity::Node::new(FIXTURE);
    let both = node.rows(
        "SELECT conname, pg_get_constraintdef(oid), pg_get_constraintdef(oid, true) \
         FROM pg_constraint WHERE conrelid = 'g1cp_trades'::regclass AND contype <> 'c' \
         ORDER BY conname",
    );
    assert!(!both.is_empty(), "no non-check constraints to compare");
    for row in &both {
        assert_eq!(row[1], row[2], "the flag moved {}", row[0]);
    }
}

/// Strict in the flag: `pg_get_constraintdef(oid, NULL)` is NULL, measured — the same rule
/// `pg_get_indexdef` already follows here.
#[test]
fn a_null_flag_is_a_null_answer() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(def(&mut node, "quantity_check", ", NULL"), "\\N");
}

/// **`ActiveRecord#check_constraints`, both halves**: the query verbatim and the Ruby regex that
/// reduces its answer. This is the assertion the five Rails tests make.
#[test]
fn activerecord_check_constraints_reads_the_expression_without_the_extra_parentheses() {
    let mut node = parity::Node::new(FIXTURE);
    // `row["constraintdef"][/CHECK \((.+)\)/m, 1]` — greedy, so it ends at the **last** `)`,
    // which is what leaves `NOT VALID` outside the capture.
    fn expression(definition: &str) -> String {
        let rest = &definition[definition.find("CHECK (").expect("no CHECK (") + "CHECK (".len()..];
        rest[..rest.rfind(')').expect("no closing parenthesis")].to_owned()
    }

    let rows = node.rows(
        "SELECT conname, pg_get_constraintdef(c.oid, true) AS constraintdef, \
         c.convalidated AS valid FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
         JOIN pg_namespace n ON n.oid = c.connamespace WHERE c.contype = 'c' \
         AND t.relname = 'g1cp_products' AND n.nspname = ANY (current_schemas(false))",
    );
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0][0], "products_price_check");
    assert_eq!(expression(&rows[0][1]), "price > discounted_price");
    assert_eq!(rows[0][2], "t");

    // The unvalidated one, which is where `validate: false` comes from and where the greedy
    // regex has to stop early.
    let rows = node.rows(
        "SELECT conname, pg_get_constraintdef(c.oid, true) AS constraintdef, \
         c.convalidated AS valid FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
         JOIN pg_namespace n ON n.oid = c.connamespace WHERE c.contype = 'c' \
         AND t.relname = 'g1cp_trades' AND n.nspname = ANY (current_schemas(false)) \
         AND c.conname = 'price_check'",
    );
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(expression(&rows[0][1]), "price > 100");
    assert_eq!(rows[0][2], "f");
}

/// **A boolean chain is where the pretty form is the *easier* one to get right**, and it is worth
/// saying why. PostgreSQL deparses, so its two spellings of `a > 0 AND b > 0` are
///
/// ```text
/// pretty      CHECK (a > 0 AND b > 0)
/// non-pretty  CHECK (((a > 0) AND (b > 0)))
/// ```
///
/// — the second parenthesising every operand. This node prints the text it stored, so the pretty
/// form agrees exactly and the non-pretty one is `CHECK ((a > 0 AND b > 0))`, which is a
/// divergence **measured and left open**: closing it means `catalog::parenthesised_operands`,
/// the helper the index and exclusion predicates already use, and that helper wraps an operand
/// the user had already parenthesised a second time — trading one divergence for another on a
/// shape no capture here covers. Nothing in the Rails suite reads a chain through the
/// one-argument form. Only the agreeing half is asserted.
#[test]
fn a_boolean_chain_agrees_in_the_pretty_form() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        def(&mut node, "chain_check", ", true"),
        "CHECK (a > 0 AND b > 0)"
    );
}
