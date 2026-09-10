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

use esker_sql::pgwire::message::{Frontend, Target};
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
                "CREATE TABLE h (id bigint, t text, data hstore, j jsonb, d json, x xml)",
                "INSERT INTO h VALUES (1, 'x', 'a=>1', '{\"a\":1}', '{\"a\":1}', '<a/>'), \
                 (2, 'y', 'b=>2', '{\"b\":2}', '{\"b\":2}', '<b/>')",
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
        // A driver asks the statement's shape before binding. This is the message the first
        // version of the test left out.
        let described = self.send(&Frontend::Describe {
            target: Target::Statement,
            name: "s".to_owned(),
        });
        if described.starts_with("ERROR") {
            return format!("AT-DESCRIBE {described}");
        }
        let bound = self.send(&Frontend::Bind {
            portal: "p".to_owned(),
            statement: "s".to_owned(),
            param_formats: Vec::new(),
            params: vec![Some(value.as_bytes().to_vec())],
            result_formats: Vec::new(),
        });
        if bound.starts_with("ERROR") {
            return format!("AT-BIND {bound}");
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
        !answer.contains("ERROR"),
        "the column says `hstore`, so the parameter is one: {answer}"
    );
}

/// **A declared zero**, which is what a driver actually puts in `Parse`. Zero means "you decide".
#[test]
fn a_zero_declared_type_takes_the_columns_type_over_the_wire() {
    let mut client = Client::new();
    let answer = client.ask("SELECT id FROM h WHERE data = $1", vec![0], "a=>1");
    assert!(
        !answer.contains("ERROR"),
        "a zero OID is not a declaration of `text`: {answer}"
    );
}

/// **And `text` really declared still refuses**, exactly as a real server does — so a fix for the
/// two above must not reach this one.
#[test]
fn a_parameter_declared_text_still_refuses_over_the_wire() {
    let mut client = Client::new();
    let answer = client.ask("SELECT id FROM h WHERE data = $1", vec![25], "a=>1");
    assert!(
        answer.ends_with("ERROR operator does not exist: hstore = text"),
        "{answer}"
    );
}

/// **The parameter on the left**, which is the asymmetry an inference written for one side has.
#[test]
fn a_parameter_on_the_left_takes_the_columns_type_too() {
    let mut client = Client::new();
    let answer = client.ask("SELECT id FROM h WHERE $1 = data", vec![0], "a=>1");
    assert!(
        !answer.contains("ERROR"),
        "inference must not depend on which side the column is: {answer}"
    );
}

/// **Which column types survive `Describe`** — the isolation that says how wide this is.
#[test]
fn describe_infers_a_parameter_from_the_column_for_every_type() {
    let mut client = Client::new();
    for (column, value) in [
        ("id", "1"),
        ("t", "x"),
        ("data", "a=>1"),
        // **`jsonb` is the one run 116 found**, and it is the same mechanism `hstore` was: a
        // parameter compared against the column takes the column's type, and a placeholder that
        // borrows `text`'s representation reports `text` instead — so the comparison resolves as
        // `jsonb = text` and refuses. `query_cache_test#test_query_cache_handles_mutated_binds`.
        ("j", r#"{"a": 1}"#),
    ] {
        let answer = client.ask(
            &format!("SELECT id FROM h WHERE {column} = $1"),
            vec![0],
            value,
        );
        assert!(!answer.contains("ERROR"), "{column}: {answer}");
    }
}

/// **The same for `jsonb` on the prepared shape**, because run 116's red is reachable from both
/// `Parse` forms and a pin on one of them can go green while the other refuses.
///
/// `query_cache_test#test_query_cache_handles_mutated_binds` is the failure; the mechanism is that
/// a bound value carries a *representation* and the parameter has a **type**, and `jsonb` shares
/// `text`'s representation. `8dd0b4a8` taught the `Describe` placeholder to carry the type and
/// left the bound value behind.
#[test]
fn a_jsonb_parameter_is_inferred_on_both_parse_forms() {
    let mut client = Client::new();
    for param_types in [vec![0], vec![]] {
        let answer = ask_portal_described(
            &mut client,
            "SELECT id FROM h WHERE j = $1",
            param_types.clone(),
            r#"{"a": 1}"#,
        );
        assert!(
            !answer.contains("ERROR"),
            "param_types={param_types:?}: {answer}"
        );
        assert!(answer.contains('1'), "the row is the answer: {answer}");
    }
}

/// **The lower bound the fix above needs**: a type with no equality operator must still refuse,
/// or the inference has bought its green by comparing everything as text.
///
/// `json` does. **`xml` does not, and that is a pre-existing divergence this test found rather
/// than caused** — measured on 19beta1, `SELECT 1 FROM t WHERE x = $1` over an `xml` column is
/// `42883 operator does not exist: xml = unknown`, and this node answers no rows. It answered
/// before the parameter fix too, checked by reverting it, so it belongs to the wire v3 comparison
/// family (`esker-coord/b4-wire-v3-families.md`) and not here. Pinned as it is so the day that
/// family lands, this line goes red and says where to look.
#[test]
fn a_parameter_over_a_type_with_no_equality_still_refuses() {
    let mut client = Client::new();
    let ask = |client: &mut Client, column: &str, value: &str| {
        client.ask(
            &format!("SELECT id FROM h WHERE {column} = $1"),
            vec![0],
            value,
        )
    };
    let json = ask(&mut client, "d", r#"{"a": 1}"#);
    assert!(
        json.contains("operator does not exist"),
        "json has no equality on either server: {json}"
    );
    // **Today's answer, and PostgreSQL's is a refusal.** Not asserted as correct — asserted so it
    // cannot change silently.
    let xml = ask(&mut client, "x", "<a/>");
    assert!(
        !xml.contains("operator does not exist"),
        "if this now refuses, the wire v3 comparison family has landed and this pin is the record \
         of what it fixed: {xml}"
    );
}

/// **The shape libpq actually sends**: `Parse` naming *no* types, `Bind`, then a `Describe` of the
/// **portal**, then `Execute`.
///
/// r1's frame tap caught two things this file had wrong. The `Describe` is of the portal and not
/// the statement — `PQsendQueryGuts` has no other shape, which `pgwire::session`'s own comment
/// already said — and the defect is reachable from *both* `Parse` forms: one type OID of zero, and
/// zero type OIDs at all. A pin that only covers `param_types=[0]` can go green while the empty
/// form still refuses, so both are here.
fn ask_portal_described(
    client: &mut Client,
    sql: &str,
    param_types: Vec<u32>,
    value: &str,
) -> String {
    let parsed = client.send(&Frontend::Parse {
        statement: "s1".to_owned(),
        sql: sql.to_owned(),
        param_types,
    });
    if parsed.starts_with("ERROR") {
        return format!("AT-PARSE {parsed}");
    }
    let bound = client.send(&Frontend::Bind {
        portal: String::new(),
        statement: "s1".to_owned(),
        param_formats: Vec::new(),
        params: vec![Some(value.as_bytes().to_vec())],
        result_formats: Vec::new(),
    });
    if bound.starts_with("ERROR") {
        return format!("AT-BIND {bound}");
    }
    let described = client.send(&Frontend::Describe {
        target: Target::Portal,
        name: String::new(),
    });
    if described.starts_with("ERROR") {
        return format!("AT-DESCRIBE-PORTAL {described}");
    }
    client.send(&Frontend::Execute {
        portal: String::new(),
        max_rows: 0,
    })
}

/// **Zero declared types, portal described** — the prepared path r1 captured.
#[test]
fn no_declared_types_with_the_portal_described() {
    let mut client = Client::new();
    let answer = ask_portal_described(
        &mut client,
        "SELECT id FROM h WHERE data = $1",
        Vec::new(),
        "a=>1",
    );
    assert!(!answer.contains("ERROR"), "{answer}");
}

/// **One zero OID, portal described** — the other form, which must not be the only one covered.
#[test]
fn a_zero_declared_type_with_the_portal_described() {
    let mut client = Client::new();
    let answer = ask_portal_described(
        &mut client,
        "SELECT id FROM h WHERE data = $1",
        vec![0],
        "a=>1",
    );
    assert!(!answer.contains("ERROR"), "{answer}");
}

/// **And `text` really declared still refuses**, through the portal shape too.
#[test]
fn a_declared_text_refuses_through_the_portal_shape() {
    let mut client = Client::new();
    let answer = ask_portal_described(
        &mut client,
        "SELECT id FROM h WHERE data = $1",
        vec![25],
        "a=>1",
    );
    assert!(
        answer.ends_with("ERROR operator does not exist: hstore = text"),
        "{answer}"
    );
}
