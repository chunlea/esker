//! **An untyped parameter takes the column's type — over the extended protocol.**
//!
//! r1's param census reports `WHERE hstore_col = $1` answering `42883 operator does not exist:
//! hstore = text` where a real server answers the row. Driving the executor directly does *not*
//! reproduce it: with no declared types, with an explicit zero OID, and with `PREPARE`, this node
//! already infers `hstore` from the other side (`tests/bind_infers_from_the_column.rs`).
//!
//! **So the two paths are not one path.** `Parse`/`Bind`/`Execute` resolve the statement's shape
//! at `Parse`, before any value has arrived, and that is the door `ActiveRecord` comes through —
//! `executor.execute` with a `Params` is the *simple* path and answers correctly. A test that
//! drives only the second one cannot see this, which is why the first version of this unit
//! reported "not a defect" and was wrong about the half that matters.
//!
//! Measured on 19beta1: `PREPARE p AS SELECT id FROM h WHERE data = $1` reports
//! `parameter_types = {hstore}`, and executing it answers the row.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::Frontend;
use esker_sql::pgwire::session::Session;

struct Client {
    node: parity::Node,
    session: Session,
}

impl Client {
    fn new() -> Self {
        Client {
            node: parity::Node::new(&[
                "CREATE EXTENSION IF NOT EXISTS hstore",
                "CREATE TABLE h (id bigint, data hstore)",
                "INSERT INTO h VALUES (1, 'a=>1'), (2, 'b=>2')",
            ]),
            session: Session::new(),
        }
    }

    fn send(&mut self, message: &Frontend) -> String {
        let mut out = Vec::new();
        self.session
            .handle(message, &mut self.node.executor, &mut out);
        read(&out)
    }

    /// `Parse` with one parameter whose type the client leaves to the server, then `Bind` the
    /// value as text and `Execute` — the three messages a driver sends for `where(col: value)`.
    fn ask(&mut self, sql: &str, param_types: Vec<u32>, value: &str) -> String {
        let parsed = self.send(&Frontend::Parse {
            statement: "s".to_owned(),
            sql: sql.to_owned(),
            param_types,
        });
        if parsed.starts_with("ERROR") {
            return parsed;
        }
        let bound = self.send(&Frontend::Bind {
            portal: "p".to_owned(),
            statement: "s".to_owned(),
            param_formats: Vec::new(),
            params: vec![Some(value.as_bytes().to_vec())],
            result_formats: Vec::new(),
        });
        if bound.starts_with("ERROR") {
            return bound;
        }
        self.send(&Frontend::Execute {
            portal: "p".to_owned(),
            max_rows: 0,
        })
    }
}

fn read(out: &[u8]) -> String {
    let text = String::from_utf8_lossy(out);
    let parts: Vec<&str> = text.split('\u{0}').collect();
    if parts
        .iter()
        .any(|part| *part == "SERROR" || *part == "VERROR")
    {
        let message = parts
            .iter()
            .find(|part| part.starts_with('M'))
            .map_or("", |part| &part[1..]);
        return format!("ERROR {message}");
    }
    text.to_string()
}

/// **No declared types at all** — the client names nothing.
#[test]
fn an_undeclared_parameter_takes_the_columns_type_over_the_wire() {
    let mut client = Client::new();
    let answer = client.ask("SELECT id FROM h WHERE data = $1", Vec::new(), "a=>1");
    assert!(
        !answer.starts_with("ERROR"),
        "the column says `hstore`, so the parameter is one: {answer}"
    );
}

/// **A declared zero**, which is what a driver actually puts in `Parse`. Zero means "you decide".
#[test]
fn a_zero_declared_type_takes_the_columns_type_over_the_wire() {
    let mut client = Client::new();
    let answer = client.ask("SELECT id FROM h WHERE data = $1", vec![0], "a=>1");
    assert!(
        !answer.starts_with("ERROR"),
        "a zero OID is not a declaration of `text`: {answer}"
    );
}

/// **And `text` really declared still refuses**, exactly as a real server does — so a fix for the
/// two above must not reach this one.
#[test]
fn a_parameter_declared_text_still_refuses_over_the_wire() {
    let mut client = Client::new();
    let answer = client.ask("SELECT id FROM h WHERE data = $1", vec![25], "a=>1");
    assert_eq!(answer, "ERROR operator does not exist: hstore = text");
}

/// **The parameter on the left**, which is the asymmetry an inference written for one side has.
#[test]
fn a_parameter_on_the_left_takes_the_columns_type_too() {
    let mut client = Client::new();
    let answer = client.ask("SELECT id FROM h WHERE $1 = data", vec![0], "a=>1");
    assert!(
        !answer.starts_with("ERROR"),
        "inference must not depend on which side the column is: {answer}"
    );
}
