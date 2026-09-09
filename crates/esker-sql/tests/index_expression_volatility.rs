//! **An index expression must be `IMMUTABLE`, and PostgreSQL decides that from `provolatile`.**
//!
//! Run 88's largest row was `functions in index expression must be marked IMMUTABLE` — 50 tests
//! across three files, and the three send four different expressions between them:
//!
//! | file | expression |
//! |---|---|
//! | `schema_test.rb` (48) | `(to_tsvector('english', coalesce(things.name, '')))` |
//! | `postgresql_adapter_test.rb` (1) | `mod(id, 10), abs(number)` |
//! | `invertible_migration_test.rb` (1) | `remind_at, place_id` — plain columns, no function at all |
//!
//! So the census is the point: fixing the one function the headline names would have left the
//! other two files exactly where they were.
//!
//! # The volatility, taken from the oracle
//!
//! `pg_proc.provolatile` on 19beta1, and the rule is `= 'i'` and nothing else:
//!
//! ```text
//! abs mod lower upper md5 left right length         i
//! to_tsvector(regconfig, text)                      i     to_tsvector(text)              s
//! date_trunc(text, timestamp)                       i     date_trunc(text, timestamptz)  s
//! concat                                            s
//! now                                               s     random                         v
//! ```
//!
//! **`concat` is the one reasoning gets wrong.** It looks like string arithmetic and it is
//! `STABLE`, because it formats by the session's settings. `date_trunc` is the other trap: the
//! same name is immutable or stable depending on which *argument type* it took.
//!
//! Every refusal below is one sentence and one class, measured in a rolled-back transaction:
//! `42P17 functions in index expression must be marked IMMUTABLE`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

const FIXTURE: &[&str] =
    &["CREATE TABLE ex (id int8, number int4, data text, remind_at timestamp, place_id int8)"];

/// The three files' own expressions, which is what 50 tests are waiting on.
#[test]
fn the_expressions_the_suite_indexes_over_are_accepted() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        // `schema_test.rb`'s, which is 48 of the 50.
        "CREATE INDEX i_ts ON ex USING gin ((to_tsvector('english', coalesce(ex.data, ''))))",
        // `postgresql_adapter_test.rb`, verbatim from `expr = "mod(id, 10), abs(number)"`.
        "CREATE INDEX i_abs ON ex ((abs(number)))",
        "CREATE INDEX i_modonly ON ex ((mod(id, 10)))",
        "CREATE INDEX i_mod ON ex ((mod(id, 10)), (abs(number)))",
        // `invertible_migration_test.rb` — no function in it at all.
        "CREATE INDEX i_cols ON ex (remind_at, place_id)",
        // The plainest immutable call, for the boundary below to mean something.
        "CREATE INDEX i_low ON ex ((lower(data)))",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "(a command, no result set)",
            "{sql}"
        );
    }
}

/// **What a real server refuses, with its own sentence** — and `concat` is why this is measured
/// rather than reasoned.
#[test]
fn a_function_that_is_not_immutable_is_refused_as_postgresql_refuses_it() {
    let mut node = parity::Node::new(FIXTURE);
    for sql in [
        "CREATE INDEX i_now ON ex ((now()))",
        "CREATE INDEX i_rand ON ex ((random()))",
        "CREATE INDEX i_cat ON ex ((concat(data, 'x')))",
    ] {
        assert_eq!(
            node.answer(sql).to_string(),
            "!42P17 functions in index expression must be marked IMMUTABLE",
            "{sql}"
        );
    }
}

/// **What this node refuses that PostgreSQL accepts, and why it is left that way.**
///
/// `date_trunc` is immutable or stable depending on the *argument type* it took —
/// `date_trunc(text, timestamp)` is `i` and `date_trunc(text, timestamptz)` is `s`, because the
/// second reads `TimeZone`. This node's rule is per *function*, with an argument **count** for the
/// few whose configuration is optional, and neither can tell those two apart — so `date_trunc` is
/// outside the immutable list entirely and both spellings are refused. That the zoned one really
/// does read the session is not a guess here any more: `tests/date_trunc.rs` pins a `month`
/// truncation that lands in a different month in New York.
///
/// That is a conservative divergence and not a wrong answer: it refuses an index a real server
/// would build, rather than building one whose key is not a function of the row. Closing it means
/// carrying the resolved argument *types* into the volatility question — the types are available
/// where this is checked, so it is reachable, but nothing in the corpus indexes over `date_trunc`
/// and a rule with no caller is a rule with no test.
#[test]
fn a_type_dependent_volatility_is_refused_conservatively() {
    let mut node = parity::Node::new(FIXTURE);
    assert_eq!(
        node.answer("CREATE INDEX i_dt ON ex ((date_trunc('day', remind_at)))")
            .to_string(),
        "!42P17 functions in index expression must be marked IMMUTABLE"
    );
}

/// **A function this node does not have is now named in an index too**, which it was not.
///
/// This test used to be called `an_unknown_function_in_an_index_blames_volatility` and asserted
/// the mislabelling, with the reasoning for leaving it. The reasoning is kept because it is the
/// thing worth reading:
///
/// > Lowering carries an unresolved name as `CatalogFunc::UserFunc` on purpose, so the *executor*
/// > can look it up in the catalog and give the honest message; `refuse_unless_immutable` runs
/// > earlier, in `index_expression`, which has a `TableDef` and no catalog. Telling "absent" from
/// > "present and volatile" there needs the catalog at that layer — a seam change, not a
/// > one-liner.
///
/// **What changed is that the distinction turned out not to be needed.** The two cases a
/// `UserFunc` can be — a name nobody declared, and a user function the catalog does hold — get the
/// *same* answer from this node, because it cannot evaluate a user function in a stored expression
/// either way (`docs/plans/phase-9-rails.md`'s `my_uuid_generator` row). So the guard can name the
/// function without knowing which case it is, and the message the query path already gives is the
/// one it gives now. `docs/plans/debts-v1.1.md` #26 is the row, and it was found by a corpus about
/// *collations*: `md5('a')` in a generated column answered `42P17` where a real server answers the
/// value, and the reason a reader was handed named the wrong thing.
///
/// **The guard still blames volatility where volatility is the reason** — the assertion above this
/// one, `date_trunc('day', remind_at)`, is a function this node *has* and PostgreSQL calls
/// `STABLE`, and it is still `42P17`. That pair is the point: one refusal per cause.
///
/// Still open, and not this unit's: a user function declared `IMMUTABLE` should eventually be
/// **accepted** here rather than refused, by inlining it the way a query does. A refusal that
/// names the function is a smaller thing to change later than one that names the wrong cause.
#[test]
fn an_unknown_function_in_an_index_is_named_rather_than_blamed() {
    let mut node = parity::Node::new(FIXTURE);
    let expected = "!0A000 the function no_such_fn is not supported";
    assert_eq!(
        node.answer("SELECT no_such_fn(data) FROM ex").to_string(),
        expected
    );
    assert_eq!(
        node.answer("CREATE INDEX i_nf ON ex ((no_such_fn(data)))")
            .to_string(),
        expected,
        "the same name, one statement apart, now gets one answer"
    );
    // And a generated column, which is where the corpus found it.
    assert_eq!(
        node.answer(
            "ALTER TABLE ex ADD COLUMN nf text GENERATED ALWAYS AS (no_such_fn(data)) STORED"
        )
        .to_string(),
        expected
    );
}
