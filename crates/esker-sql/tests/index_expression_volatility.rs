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
//! # The census, finished
//!
//! The table above is the five names three files send, and for eight months that was the whole
//! measurement: `plan::CatalogFuncCall::is_immutable` then grew a family per defect — the
//! text-search functions, the JSON accessors, `||` over text — and each of those commits could
//! have measured the rest of the table with one more `WHERE`. `docs/plans/debts-v1.1.md` #25's
//! corpus was the fourth to find it, refusing `replace(t, 'a', 'b')` in a generated column for a
//! call whose `provolatile` is `i`.
//!
//! So `tests/captures/pg19_provolatile_census.txt` is `pg_proc.provolatile` for **all 79 names**
//! `plan::CatalogFunc::from_name` and `plan::ScalarFunc::from_name` resolve, in one query.
//! Twenty-two of them were `i` on the oracle and refused here — in *both* readers, index key and
//! generated column, 22 of 22 each — and the two tests at the bottom of this file are that list.
//! The nine `ScalarFunc` names came back `i` to a name (`length(bytea, name)` is the one `s`
//! overload and this node has no two-argument `length`), so the permissive treatment they get from
//! `refuse_unless_immutable` — no arm at all — is correct rather than lucky.
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

/// The table the census below needs, which is one column per argument type in it.
const CENSUS_FIXTURE: &[&str] = &[
    "CREATE EXTENSION hstore",
    "CREATE EXTENSION ltree",
    "CREATE TABLE cs (t text, arr int4[], h hstore, r tsrange, \
     l ltree, q tsquery, p path, ts timestamp, d date)",
];

/// **The other twenty-two, measured in one pass instead of one per defect.**
///
/// `is_immutable` grew a family at a time — the text-search functions, then the JSON accessors,
/// then `||` over text — and every one of those commits could have measured the rest of the table
/// with the same query. `docs/plans/debts-v1.1.md` #25's corpus was refused
/// `replace(t, 'a', 'b')` in a generated column, `42P17`, for a call whose `provolatile` is `i`;
/// the fix was not `replace`. It was
/// `tests/captures/pg19_provolatile_census.txt` — `pg_proc.provolatile` for all 79 names
/// `plan::CatalogFunc::from_name` and `plan::ScalarFunc::from_name` resolve, in one query — and
/// these are the twenty-two rows it answered `i` for that this node was refusing.
#[test]
fn every_function_the_oracle_calls_immutable_may_be_an_index_key() {
    let mut node = parity::Node::new(CENSUS_FIXTURE);
    // **Collected rather than asserted one at a time.** The first shape of this test stopped at
    // `replace`, which is the one name the corpus had already found — so it would have reported
    // one row of a twenty-two row measurement and read like the whole thing was covered.
    let mut refused = Vec::new();
    for (n, expr) in [
        // The string function the corpus found.
        "replace(t, 'a', 'b')",
        // Array introspection.
        "array_length(arr, 1)",
        "array_lower(arr, 1)",
        "array_upper(arr, 1)",
        "array_position(arr, 1)",
        "cardinality(arr)",
        // Range introspection, and the two constructors this node resolves.
        "isempty(r)",
        "lower_inc(r)",
        "lower_inf(r)",
        "upper_inc(r)",
        "upper_inf(r)",
        "daterange(d, d)",
        "tsrange(ts, ts)",
        // The two path predicates.
        "isclosed(p)",
        "isopen(p)",
        // hstore.
        "akeys(h)",
        "avals(h)",
        "hstore(t, t)",
        // ltree, and `numnode` beside it.
        "nlevel(l)",
        "ltree2text(l)",
        "text2ltree(t)",
        "numnode(q)",
    ]
    .iter()
    .enumerate()
    {
        let sql = format!("CREATE INDEX i_cs{n} ON cs (({expr}))");
        let answer = node.answer(&sql).to_string();
        if answer != "(a command, no result set)" {
            refused.push(format!("{expr}\n  {answer}"));
        }
    }
    assert!(
        refused.is_empty(),
        "{} of the 22 the oracle calls immutable are refused as an index key:\n\n{}",
        refused.len(),
        refused.join("\n")
    );
}

/// **The same twenty-two as a generated column**, which is the reader that found them.
///
/// `refuse_unless_immutable` is one function with two callers — `index_expression` and the
/// generated-column path — and a rule that reached only one of them is this repository's most
/// repeated defect. The corpus that found `replace` was reading `pg_get_expr(adbin)` for a
/// `GENERATED ALWAYS AS` column, not an index, so the index assertion above would have passed
/// without the column one ever being true.
#[test]
fn every_function_the_oracle_calls_immutable_may_be_a_generated_column() {
    let mut node = parity::Node::new(CENSUS_FIXTURE);
    let mut refused = Vec::new();
    for (n, expr, ty) in [
        ("replace(t, 'a', 'b')", "text"),
        ("array_length(arr, 1)", "integer"),
        ("array_lower(arr, 1)", "integer"),
        ("array_upper(arr, 1)", "integer"),
        ("array_position(arr, 1)", "integer"),
        ("cardinality(arr)", "integer"),
        ("isempty(r)", "boolean"),
        ("lower_inc(r)", "boolean"),
        ("lower_inf(r)", "boolean"),
        ("upper_inc(r)", "boolean"),
        ("upper_inf(r)", "boolean"),
        ("daterange(d, d)", "daterange"),
        ("tsrange(ts, ts)", "tsrange"),
        ("isclosed(p)", "boolean"),
        ("isopen(p)", "boolean"),
        ("akeys(h)", "text[]"),
        ("avals(h)", "text[]"),
        ("hstore(t, t)", "hstore"),
        ("nlevel(l)", "integer"),
        ("ltree2text(l)", "text"),
        ("text2ltree(t)", "ltree"),
        ("numnode(q)", "integer"),
    ]
    .iter()
    .enumerate()
    .map(|(n, (expr, ty))| (n, *expr, *ty))
    {
        let sql =
            format!("ALTER TABLE cs ADD COLUMN g{n} {ty} GENERATED ALWAYS AS ({expr}) STORED");
        let answer = node.answer(&sql).to_string();
        if answer != "(a command, no result set)" {
            refused.push(format!("{expr}\n  {answer}"));
        }
    }
    assert!(
        refused.is_empty(),
        "{} of the 22 are refused as a generated column:\n\n{}",
        refused.len(),
        refused.join("\n")
    );
    // **And then the values, because accepting the column is not the answer.** A rule that only
    // stops refusing has moved the failure from `ALTER TABLE` to the first `INSERT`, and 22
    // columns that store NULL would pass the assertion above. Measured on the oracle in a
    // rolled-back transaction over exactly this row
    // (`tests/captures/pg19_provolatile_census.txt`'s tail records the query):
    assert_eq!(
        node.answer(
            "INSERT INTO cs VALUES ('abc', '{7,8}', 'k=>v', \
             tsrange('2020-01-01'::timestamp, '2020-02-01'::timestamp), 'x.y.z', \
             'a & b'::tsquery, '((0,0),(1,1),(0,1))'::path, '2020-01-01', '2020-01-01')"
        )
        .to_string(),
        "(a command, no result set)"
    );
    let columns = (0..22)
        .map(|n| format!("g{n}"))
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        node.answer(&format!("SELECT {columns} FROM cs"))
            .to_string(),
        "text,integer,integer,integer,integer,integer,boolean,boolean,boolean,boolean,boolean,\
         daterange,tsrange,boolean,boolean,text[],text[],hstore,integer,text,ltree,integer\
         \tbbc|2|1|2|\\N|2|f|t|f|f|f|empty|empty|t|f|{k}|{v}|\"abc\"=>\"abc\"|3|x.y.z|abc|3"
    );
}
