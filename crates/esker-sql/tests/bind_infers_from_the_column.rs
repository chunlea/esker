//! **An unknown parameter in a comparison takes the column's type** — r1's param census.
//!
//! `where(col: value)` is what `ActiveRecord` sends for every finder, and for an `hstore` column it
//! arrives as `WHERE data = $1` with the parameter's type left unspecified. This node read that
//! parameter as `text` and answered `42883 operator does not exist: hstore = text`; a real server
//! infers it from the other side of the comparison and answers the row. Measured: `PREPARE` over
//! the same statement reports `parameter_types = {hstore}`.
//!
//! **The node already does this inference for `INSERT` and `UPDATE`**, where the target column
//! says what the value is — r1's census is 31/0 divergences for both. It is the comparison that
//! was left out, which is the shape this repository keeps meeting: a rule that reached the caller
//! it was written for and not the next one.
//!
//! `hstore_test.rb`'s `where` is what walks into it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "bind_harness/mod.rs"]
mod bind;

use std::sync::Arc;

use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::parse::parse_statements;
use esker_sql::pgwire::session::{Execute, Params};

/// One bound parameter, in the shape `answer` takes them.
fn one(text: &str) -> [Option<Vec<u8>>; 1] {
    [Some(text.as_bytes().to_vec())]
}

fn node() -> bind::Node {
    let mut node = bind::Node::new();
    node.bound("CREATE EXTENSION IF NOT EXISTS hstore", &[])
        .unwrap();
    node.bound("CREATE TABLE h (id bigint, data hstore)", &[])
        .unwrap();
    node.bound("INSERT INTO h VALUES (1, 'a=>1'), (2, 'b=>2')", &[])
        .unwrap();
    node
}

/// **The comparison infers from the column**, which is the half that was missing.
#[test]
fn a_bound_comparison_takes_the_columns_type() {
    let mut node = node();
    assert_eq!(
        node.answer("SELECT id FROM h WHERE data = $1", &one("a=>1")),
        bind::Answer::Rows {
            types: vec!["bigint".to_owned()],
            rows: vec![vec!["1".to_owned()]],
        }
    );
}

/// **The two that already worked**, kept beside it so a fix cannot trade one for the other — this
/// is the shape a lane in this repository has traded before.
#[test]
fn an_insert_and_an_update_still_infer_from_the_column() {
    let mut node = node();
    assert_eq!(
        node.answer("INSERT INTO h VALUES (3, $1)", &one("c=>3")),
        bind::Answer::Done
    );
    assert_eq!(
        node.answer("UPDATE h SET data = $1 WHERE id = 3", &one("d=>4")),
        bind::Answer::Done
    );
    assert_eq!(
        node.answer("SELECT data FROM h WHERE id = 3", &[]),
        bind::Answer::Rows {
            types: vec!["hstore".to_owned()],
            rows: vec![vec!["\"d\"=>\"4\"".to_owned()]],
        }
    );
}

/// **And what happens when the driver *declares* the parameter `text`** — which is the
/// configuration r1's census hit, and where PostgreSQL itself refuses.
///
/// Measured on 19beta1, both ways round:
///
/// ```text
/// PREPARE p (text) AS SELECT id FROM h WHERE data = $1   ERROR: operator does not exist: hstore = text
/// PREPARE p        AS SELECT id FROM h WHERE data = $1   1
/// ```
///
/// So the sentence in the census report is **PostgreSQL's own answer** for a declared-`text`
/// parameter, not a divergence — what decides is whether the driver names a type in `Parse`, and
/// a zero there means "you decide". This pins both configurations so neither can drift into the
/// other: undeclared infers the column's type, declared `text` refuses exactly as a real server
/// does.
#[test]
fn a_parameter_declared_text_refuses_the_way_postgresql_does() {
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let mut executor = Executor::new(
        backend,
        Arc::new(Catalog::new()),
        1,
        esker_sql::session::register(),
    );
    let mut run = |sql: &str, values: &[Option<Vec<u8>>], declared: &[u32]| {
        let parsed = parse_statements(sql)?;
        let mut last = None;
        for one in &parsed {
            last = Some(executor.execute(
                one,
                &Params {
                    values,
                    formats: &[],
                    declared,
                    bound: !values.is_empty(),
                },
            )?);
        }
        Ok::<_, esker_sql::SqlError>(last.unwrap())
    };
    run("CREATE EXTENSION IF NOT EXISTS hstore", &[], &[]).unwrap();
    run("CREATE TABLE h (id bigint, data hstore)", &[], &[]).unwrap();
    run("INSERT INTO h VALUES (1, 'a=>1')", &[], &[]).unwrap();

    // 25 is `text`. PostgreSQL refuses this; so must this node, with the same sentence.
    let refused = run("SELECT id FROM h WHERE data = $1", &one("a=>1"), &[25]).unwrap_err();
    assert_eq!(
        refused.to_string(),
        "operator does not exist: hstore = text"
    );
    assert_eq!(refused.sqlstate(), esker_sql::sqlstate::UNDEFINED_FUNCTION);
}

/// **An explicitly zero OID is not the same as no OID at all** — which is what a driver sends.
///
/// The wire's `Parse` carries one type OID per parameter and **zero means "you decide"**. A client
/// that names no types at all sends an empty list; `ActiveRecord`'s driver sends a list of zeros.
/// Those are the same request and this node must answer them the same way.
#[test]
fn a_zero_oid_infers_the_same_as_no_oid_at_all() {
    let backend: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
    let mut executor = Executor::new(
        backend,
        Arc::new(Catalog::new()),
        1,
        esker_sql::session::register(),
    );
    let mut run = |sql: &str, values: &[Option<Vec<u8>>], declared: &[u32]| {
        let parsed = parse_statements(sql)?;
        let mut last = None;
        for one in &parsed {
            last = Some(executor.execute(
                one,
                &Params {
                    values,
                    formats: &[],
                    declared,
                    bound: !values.is_empty(),
                },
            )?);
        }
        Ok::<_, esker_sql::SqlError>(last.unwrap())
    };
    run("CREATE EXTENSION IF NOT EXISTS hstore", &[], &[]).unwrap();
    run("CREATE TABLE h (id bigint, data hstore)", &[], &[]).unwrap();
    run("INSERT INTO h VALUES (1, 'a=>1')", &[], &[]).unwrap();

    let answered = run("SELECT id FROM h WHERE data = $1", &one("a=>1"), &[0])
        .expect("a zero OID means the node decides, and the column says hstore");
    let esker_sql::pgwire::session::Outcome::Rows { rows, .. } = answered else {
        panic!("no rows");
    };
    assert_eq!(rows.len(), 1);
}
