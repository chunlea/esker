//! **Assignment casts** — what a value may become on its way into a column, which is a wider set
//! than what it may become in an expression.
//!
//! `UpdateAllTest#test_update_counters_with_joins` is the test that found this. `update_counters`
//! builds `COALESCE(col, 0) + $1` (`relation.rb#_increment_attribute`) and sends:
//!
//! ```text
//! UPDATE "pets" SET "integer" = COALESCE("pets"."integer", 0) + $1
//!   FROM "toys" WHERE "toys"."pet_id" = "pets"."id" AND "toys"."name" = 'Bone'
//! ```
//!
//! and this node answered `42804 column "integer" is of type integer but expression is of type
//! bigint`. The column really is called `integer` and really is an `integer`; the `bigint` is
//! ours.
//!
//! # Measured on 19beta1 before a line was written
//!
//! **PostgreSQL types the expression `integer` in the first place** — an integer constant is
//! `integer`, not `bigint`, and `int4 + int4` stays `int4`:
//!
//! ```text
//! pg_typeof(1)                       integer
//! pg_typeof(COALESCE(int4, 0))       integer
//! pg_typeof(COALESCE(int4, 0) + 1)   integer
//! ```
//!
//! **And where the expression really is wider, assignment is still allowed** — the cast happens,
//! and only the *value* can fail. This is the half that matters, because it is a whole family and
//! not one arm:
//!
//! ```text
//! UPDATE t SET i4 = i8                        allowed; 22003 integer out of range if it overflows
//! UPDATE t SET i4 = (SELECT count(*) FROM …)  allowed — count() is bigint
//! UPDATE t SET i2 = i4, i4 = i2               allowed
//! UPDATE t SET i4 = n                         allowed, and it ROUNDS: numeric 7.6 lands as 8
//! UPDATE t SET n  = i4                        allowed
//! UPDATE t SET a4 = a8                        allowed — bigint[] into an integer[] column
//! UPDATE t SET txt = i4                       allowed — every type has an assignment cast to text
//! INSERT INTO t (i4) SELECT i8 FROM t         allowed, same rule
//! ```
//!
//! **The table is asymmetric, and that is what keeps this from being "cast everything".** To text
//! is an assignment cast; *from* text is explicit only:
//!
//! ```text
//! UPDATE t SET i4 = txt   42804 column "i4" is of type integer but expression is of type text
//!                         HINT:  You will need to rewrite or cast the expression.
//! ```
//!
//! and in an **expression** context nothing is implicit either way — `WHERE 'x' = 1` is
//! `22P02 invalid input syntax for type integer: "x"`, not a mismatch and not a cast.
//!
//! # Two comments in this repository state the opposite, and they are corrected in this change
//!
//! `plan::expr`'s `Literal::assign` says of the array arm: "A `Literal` is a constant in the
//! statement, never a column reference, so this settles a *literal's* type and does not widen
//! assignment between two columns: `int8[]` into an `integer[]` column is still `42804`, from
//! `exec::assign`." Measured above: a real server allows exactly that. The rule the comment
//! describes is real — a literal's type *is* settled by the column — but the conclusion drawn from
//! it about column-to-column assignment is not what PostgreSQL does.
//!
//! # The rule this node already had, in one place instead of two
//!
//! `exec::dml::assign_default` carries PostgreSQL's numeric assignment cast — "**An integer into a
//! narrower integer**, which is PostgreSQL's numeric assignment cast and has the *short* `22003`"
//! — added when a `DEFAULT` needed it. Nothing else reached it: `plan::expr::Literal::assign`'s
//! `Typed` arm asks `value.fits(ty)` and falls to a mismatch, so the same value narrowed in a
//! `DEFAULT` and was refused in a `SET`. Two paths, one rule, and the disagreement is the argument
//! for lifting it out rather than writing it twice.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

/// `counter` rather than `"integer"` for the mechanism tests, so that a quoting problem could not
/// be mistaken for a cast problem. One test below uses `ActiveRecord`'s real column name.
const FIXTURE: &[&str] = &[
    "CREATE TABLE g1_pets (id bigserial primary key, counter integer, name text)",
    "CREATE TABLE g1_toys (id bigserial primary key, pet_id bigint, name text)",
    "INSERT INTO g1_pets (name) VALUES ('parrot')",
    "INSERT INTO g1_toys (pet_id, name) VALUES (1, 'Bone')",
];

const WIDTHS: &[&str] = &[
    "CREATE TABLE g1_w (id bigserial primary key, i4 integer, i8 bigint, i2 smallint, \
     a4 integer[], a8 bigint[], n numeric, t text)",
    "INSERT INTO g1_w (i4, i8, i2, a4, a8, n, t) \
     VALUES (1, 5000000000, 1, '{1,2}', '{3,4}', 7.6, 'x')",
];

/// **The join question, and it is asked first on purpose.** `UPDATE … FROM` belongs to another
/// lane; if the bare form narrows correctly then nothing here is about the join.
#[test]
fn the_bare_update_narrows_without_a_join() {
    let mut node = parity::Node::new(FIXTURE);
    node.run("UPDATE g1_pets SET counter = COALESCE(counter, 0) + 1")
        .unwrap();
    assert_eq!(node.rows("SELECT counter FROM g1_pets"), [["1".to_owned()]]);
}

/// The statement `update_counters` sends, with `ActiveRecord`'s own column name — which is the
/// word `integer`, quoted.
#[test]
fn the_statement_update_counters_sends() {
    let mut node = parity::Node::new(&[
        "CREATE TABLE g1_pets (id bigserial primary key, \"integer\" integer, name text)",
        "CREATE TABLE g1_toys (id bigserial primary key, pet_id bigint, name text)",
        "INSERT INTO g1_pets (name) VALUES ('parrot')",
        "INSERT INTO g1_toys (pet_id, name) VALUES (1, 'Bone')",
    ]);
    node.run(
        "UPDATE g1_pets SET \"integer\" = COALESCE(g1_pets.\"integer\", 0) + 1 \
         FROM g1_toys WHERE g1_toys.pet_id = g1_pets.id AND g1_toys.name = 'Bone'",
    )
    .unwrap();
    assert_eq!(
        node.rows("SELECT \"integer\" FROM g1_pets"),
        [["1".to_owned()]]
    );
}

/// **A wider integer column into a narrower one is a cast, not a mismatch** — and only the value
/// can fail, with the short `22003`.
#[test]
fn a_bigint_column_assigns_into_an_integer_column() {
    let mut node = parity::Node::new(WIDTHS);
    assert_eq!(
        node.answer("UPDATE g1_w SET i4 = i8").to_string(),
        "!22003 integer out of range",
        "5000000000 does not fit, and that is a value error rather than a type error"
    );
    node.run("UPDATE g1_w SET i8 = 42").unwrap();
    node.run("UPDATE g1_w SET i4 = i8").unwrap();
    node.run("UPDATE g1_w SET i2 = i4").unwrap();
    node.run("UPDATE g1_w SET i4 = i2").unwrap();
    assert_eq!(
        node.rows("SELECT i4, i8, i2 FROM g1_w"),
        [["42".to_owned(), "42".to_owned(), "42".to_owned()]]
    );
}

/// A `numeric` into an integer column **rounds**, which is the same assignment cast one type over.
#[test]
fn numeric_into_an_integer_column_rounds() {
    let mut node = parity::Node::new(WIDTHS);
    node.run("UPDATE g1_w SET i4 = n").unwrap();
    assert_eq!(
        node.rows("SELECT i4 FROM g1_w"),
        [["8".to_owned()]],
        "7.6 rounds to 8 rather than truncating to 7"
    );
    node.run("UPDATE g1_w SET n = i4").unwrap();
    assert_eq!(node.rows("SELECT n FROM g1_w"), [["8".to_owned()]]);
}

/// **An array is the same rule one level down.** The comment this corrects said this was `42804`.
#[test]
fn a_bigint_array_assigns_into_an_integer_array_column() {
    let mut node = parity::Node::new(WIDTHS);
    node.run("UPDATE g1_w SET a4 = a8").unwrap();
    assert_eq!(node.rows("SELECT a4 FROM g1_w"), [["{3,4}".to_owned()]]);
}

/// Every type has an assignment cast **to** text.
#[test]
fn an_integer_assigns_into_a_text_column() {
    let mut node = parity::Node::new(WIDTHS);
    node.run("UPDATE g1_w SET t = i4").unwrap();
    assert_eq!(node.rows("SELECT t FROM g1_w"), [["1".to_owned()]]);
}

/// **And nothing has one *from* text**, which is what keeps the table from being "cast anything to
/// anything". The `HINT` is the server's.
#[test]
fn text_into_an_integer_column_is_still_refused() {
    let mut node = parity::Node::new(WIDTHS);
    assert_eq!(
        node.answer("UPDATE g1_w SET i4 = t").to_string(),
        "!42804 column \"i4\" is of type integer but expression is of type text \
         HINT: You will need to rewrite or cast the expression."
    );
}

/// **The same rule on the statement that also assigns.** An `INSERT`'s `VALUES` tuple asks
/// `exec::assign` the same question a `SET` does.
///
/// Written with an explicit cast rather than `INSERT … SELECT`, which this node refused outright
/// when this file was written (`0A000 INSERT ... SELECT is not supported`) — a separate and much
/// larger gap that the first draft of this file accidentally tested instead of the cast rule.
#[test]
fn insert_takes_the_same_casts() {
    let mut node = parity::Node::new(WIDTHS);
    node.run("INSERT INTO g1_w (i4) VALUES (7::bigint)")
        .unwrap();
    node.run("INSERT INTO g1_w (i4) VALUES (7.6::numeric)")
        .unwrap();
    node.run("INSERT INTO g1_w (t) VALUES (7::integer)")
        .unwrap();
    assert_eq!(
        node.rows("SELECT i4, t FROM g1_w ORDER BY id"),
        [
            ["1".to_owned(), "x".to_owned()],
            ["7".to_owned(), "\\N".to_owned()],
            ["8".to_owned(), "\\N".to_owned()],
            ["\\N".to_owned(), "7".to_owned()],
        ]
    );
    assert_eq!(
        node.answer("INSERT INTO g1_w (i4) VALUES (5000000000::bigint)")
            .to_string(),
        "!22003 integer out of range"
    );
}

// ---------------------------------------------------------------------------------------------
// The bind parameter, which is what the reconstruction above was missing.
//
// `_increment_attribute` builds `COALESCE(col, 0) + bind`, and the bind reaches the wire as `$1`
// with **no declared OID** — the client says "you decide". Measured on 19beta1 with `PREPARE` and
// `pg_prepared_statements.parameter_types`, PostgreSQL decides **integer** in all three shapes:
//
// ```text
// UPDATE t SET counter = COALESCE(t.counter, 0) + $1                      {integer}
// UPDATE t SET counter = COALESCE(t.counter, 0) + $1 WHERE id IN (…join…) {integer}
// UPDATE t SET counter = $1                                               {integer}
// ```
//
// The `IN (SELECT … INNER JOIN …)` is the shape `Arel#compile_update` actually emits for
// `update_all` with a join — not `UPDATE … FROM`, which is what a first reading of the statement
// suggests.
// ---------------------------------------------------------------------------------------------

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::value::{ColumnType, PgType};

struct Bound {
    executor: Executor,
}

impl Bound {
    fn loaded() -> Self {
        let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let mut node = Bound {
            executor: Executor::new(
                backend,
                Arc::new(Catalog::new()),
                1,
                esker_sql::session::register(),
            ),
        };
        for statement in FIXTURE {
            node.plain(statement).unwrap();
        }
        node
    }

    fn plain(&mut self, sql: &str) -> esker_sql::Result<Outcome> {
        let mut last = Outcome::done("");
        for parsed in parse_statements(sql)? {
            last = self.executor.execute(&parsed, &Params::NONE)?;
        }
        Ok(last)
    }

    /// One statement with one text-format bind and **no declared OID**, which is what
    /// `ActiveRecord` sends.
    fn bind_one(&mut self, sql: &str, value: &str) -> esker_sql::Result<Outcome> {
        let values = [Some(value.as_bytes().to_vec())];
        let parsed = parse_statements(sql)?;
        self.executor.execute(
            &parsed[0],
            &Params {
                values: &values,
                formats: &[],
                declared: &[],
                bound: true,
            },
        )
    }

    /// The OID this node infers for each parameter, which is what `Describe` tells a client.
    fn inferred(&mut self, sql: &str) -> Vec<u32> {
        let parsed = parse_statements(sql).unwrap();
        self.executor.describe(&parsed[0], &[]).unwrap().parameters
    }

    fn column(&mut self, sql: &str) -> Vec<String> {
        match self.plain(sql).unwrap() {
            Outcome::Rows { rows, .. } => rows
                .into_iter()
                .map(|row| {
                    row[0].clone().map_or_else(
                        || "NULL".to_owned(),
                        |bytes| String::from_utf8(bytes).unwrap(),
                    )
                })
                .collect(),
            Outcome::Done { tag } => panic!("not rows: {tag}"),
        }
    }
}

/// **The parameter's inferred type**, which is where the `bigint` comes from if it comes from
/// anywhere. PostgreSQL says `integer` for all three.
#[test]
fn the_increment_parameter_is_inferred_integer() {
    let mut node = Bound::loaded();
    let int4 = ColumnType::Int4.oid();
    for sql in [
        "UPDATE g1_pets SET counter = COALESCE(g1_pets.counter, 0) + $1",
        "UPDATE g1_pets SET counter = $1",
    ] {
        assert_eq!(node.inferred(sql), vec![int4], "for {sql}");
    }
}

/// The statement `update_counters` really sends, bind and subquery included.
#[test]
fn the_bound_increment_runs() {
    let mut node = Bound::loaded();
    node.bind_one(
        "UPDATE g1_pets SET counter = COALESCE(g1_pets.counter, 0) + $1",
        "1",
    )
    .unwrap();
    assert_eq!(node.column("SELECT counter FROM g1_pets"), ["1"]);

    node.bind_one(
        "UPDATE g1_pets SET counter = COALESCE(g1_pets.counter, 0) + $1 \
         WHERE g1_pets.id IN (SELECT g1_pets.id FROM g1_pets \
         INNER JOIN g1_toys ON g1_toys.pet_id = g1_pets.id WHERE g1_toys.name = 'Bone')",
        "1",
    )
    .unwrap();
    assert_eq!(node.column("SELECT counter FROM g1_pets"), ["2"]);
}
