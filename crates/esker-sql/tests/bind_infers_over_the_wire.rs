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

    /// The bytes a message produced, for the two tests that read the **wire** rather than a
    /// lossy rendering of it: a `RowDescription`'s type OID is four bytes in a frame and a
    /// `contains("…")` over the whole stream cannot tell it from a value that happens to say the
    /// same thing.
    fn send_raw(&mut self, message: &Frontend) -> Vec<u8> {
        let mut out = Vec::new();
        self.session
            .handle(message, &mut self.node.executor, &mut out);
        out
    }

    /// `Parse`/`Describe`/`Bind`/`Execute` with **no declared type**, answering the first column's
    /// wire OID and its text — or the `SQLSTATE` of whichever step refused.
    fn ask_row(&mut self, sql: &str, value: &str) -> Result<(u32, Option<String>), String> {
        let mut stream = self.send_raw(&Frontend::Parse {
            statement: "r".to_owned(),
            sql: sql.to_owned(),
            param_types: Vec::new(),
        });
        stream.extend(self.send_raw(&Frontend::Describe {
            target: Target::Statement,
            name: "r".to_owned(),
        }));
        stream.extend(self.send_raw(&Frontend::Bind {
            portal: "q".to_owned(),
            statement: "r".to_owned(),
            param_formats: Vec::new(),
            params: vec![Some(value.as_bytes().to_vec())],
            result_formats: Vec::new(),
        }));
        stream.extend(self.send_raw(&Frontend::Execute {
            portal: "q".to_owned(),
            max_rows: 0,
        }));
        self.send(&Frontend::Sync);
        first_row(&stream)
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
        let answer = self.ask_unsynced(sql, param_types, value);
        // **Every ask ends with its `Sync`, and it has to.** After a failure a session discards
        // every message up to the next `Sync` (`pgwire::session`'s `skipping_until_sync`, captured
        // from a real server), so a second ask on the same client would be dropped in silence and
        // come back as the empty string — which reads exactly like "no error". That is how the
        // refusal test below spent a round pinning a fact about this node that was really a fact
        // about this harness.
        self.send(&Frontend::Sync);
        answer
    }

    fn ask_unsynced(&mut self, sql: &str, param_types: Vec<u32>, value: &str) -> String {
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
/// Both `json` and `xml` refuse, on both `Parse` forms, and so does 19beta1 — measured today,
/// `esker-coord/wire-v3-probes.txt`'s `where_eq` rows and r1's census on `ba932e4e` agree, which
/// is what corrected the claim this test used to carry.
///
/// **What it used to say, and why it was wrong.** It asserted that `xml` was *answered* here where
/// PostgreSQL refuses, and called that a pre-existing divergence for the wire v3 comparison family
/// to close. That was never a measurement of this node: the `json` ask above it fails at
/// `Describe`, the harness sent no `Sync`, and a session discards everything up to the next `Sync`
/// after a failure — so the `xml` ask's four messages were dropped without a byte and the empty
/// answer read as "no error". Both asks now end with their `Sync` and both refuse. A prediction of
/// two new DIVERGE rows went out on the strength of the old assertion; the census measured MATCH
/// and was right.
///
/// **The refusals are not word for word, and the difference is where the parameter is resolved.**
/// This node infers `$1` from the column first and then looks for the operator, so it names both
/// sides — `operator does not exist: xml = xml`. PostgreSQL leaves the parameter `unknown` and
/// fails the same lookup — `operator does not exist: xml = unknown`. The census judges a refusal on
/// direction and sqlstate, so the row is a MATCH; the sentence is written down here because it is
/// the visible half a client reads.
#[test]
fn a_parameter_over_a_type_with_no_equality_still_refuses() {
    let mut client = Client::new();
    let ask = |client: &mut Client, column: &str, types: Vec<u32>, value: &str| {
        client.ask(
            &format!("SELECT id FROM h WHERE {column} = $1"),
            types,
            value,
        )
    };
    // **Both `Parse` forms**, because the two are different frames and a pin on one can go green
    // while the other answers (r1's frame tap, `wire-v3-format.md`).
    for types in [vec![0], Vec::new()] {
        let json = ask(&mut client, "d", types.clone(), r#"{"a": 1}"#);
        assert!(
            json.contains("operator does not exist: json = json"),
            "json has no equality on either server, param_types={types:?}: {json}"
        );
        let xml = ask(&mut client, "x", types.clone(), "<a/>");
        assert!(
            xml.contains("operator does not exist: xml = xml"),
            "xml has no equality on either server either, param_types={types:?}: {xml}"
        );
    }
}

/// **The counterfactual for the harness bug above**, which is the only thing that can prove the
/// `Sync` is load-bearing: without it the second ask on a failed session answers nothing at all,
/// and an assertion that reads "no error" passes over a message stream the server never saw.
#[test]
fn an_ask_after_a_failure_is_discarded_until_sync() {
    let mut client = Client::new();
    let failed = client.ask_unsynced("SELECT id FROM h WHERE d = $1", vec![0], "{}");
    assert!(
        failed.contains("operator does not exist"),
        "the first ask has to fail for this to measure anything: {failed}"
    );
    let swallowed = client.ask_unsynced("SELECT id FROM h WHERE x = $1", vec![0], "<a/>");
    assert_eq!(
        swallowed, "",
        "a session discards every message up to the next `Sync` after a failure, so this ask \
         produced no bytes — and an assertion of the form `!answer.contains(\"ERROR\")` would call \
         that a pass"
    );
    client
        .session
        .handle(&Frontend::Sync, &mut client.node.executor, &mut Vec::new());
    let asked = client.ask("SELECT id FROM h WHERE x = $1", vec![0], "<a/>");
    assert!(
        asked.contains("operator does not exist: xml = xml"),
        "and after the `Sync` the same ask reaches the executor and refuses: {asked}"
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
    let answer = portal_described_unsynced(client, sql, param_types, value);
    client.send(&Frontend::Sync);
    answer
}

fn portal_described_unsynced(
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

/// The first `RowDescription`'s first type OID and the first `DataRow`'s first column, read out of
/// the backend stream — or the `SQLSTATE` of the first `ErrorResponse` in it.
///
/// A backend message is `tag(1) · length(4, big endian, counting itself) · body`, and the three
/// bodies this reads are in `pgwire::message`'s own encoder one screen apart.
fn first_row(stream: &[u8]) -> Result<(u32, Option<String>), String> {
    let mut at = 0;
    let mut oid = None;
    while at + 5 <= stream.len() {
        let tag = stream[at];
        let len = u32::from_be_bytes([
            stream[at + 1],
            stream[at + 2],
            stream[at + 3],
            stream[at + 4],
        ]) as usize;
        let body = &stream[at + 5..(at + 1 + len).min(stream.len())];
        at += 1 + len;
        match tag {
            // `E`: fields until a NUL, each `type · text · NUL`; `C` is the SQLSTATE.
            b'E' => {
                let mut field = body;
                while let Some(end) = field.iter().position(|byte| *byte == 0) {
                    if end == 0 {
                        break;
                    }
                    let (kind, text) = (field[0], String::from_utf8_lossy(&field[1..end]));
                    if kind == b'C' {
                        return Err(text.into_owned());
                    }
                    field = &field[end + 1..];
                }
                return Err("an ErrorResponse with no code".to_owned());
            }
            // `T`: count, then per field `name · NUL · table · column · type oid · …`.
            b'T' => {
                let name_end = 2 + body[2..].iter().position(|byte| *byte == 0).unwrap_or(0);
                let ty = name_end + 1 + 6;
                oid = Some(u32::from_be_bytes([
                    body[ty],
                    body[ty + 1],
                    body[ty + 2],
                    body[ty + 3],
                ]));
            }
            // `D`: count, then per column `length(4, -1 for NULL) · bytes`.
            b'D' => {
                let size = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                let value = usize::try_from(size)
                    .ok()
                    .map(|size| String::from_utf8_lossy(&body[6..6 + size]).into_owned());
                return Ok((oid.unwrap_or(0), value));
            }
            _ => {}
        }
    }
    Err("no DataRow and no ErrorResponse".to_owned())
}

/// **A vector cast to an array, through a bound parameter** — the eleven rows of the cast matrix
/// where a literal answers and a bind refuses.
///
/// `$1::oidvector::integer[]` was `22P02 malformed array literal: "1 2"` while
/// `'1 2'::oidvector::integer[]` answered, and `int2vector` answered in **both** spellings: the
/// two vectors differ only in that `oidvector` reaches the planner as a `CatalogFunc::OidVector`
/// (`sqlparser` has no `DataType` for it) where `int2vector` reaches `lower_type`. The evaluator's
/// vector arm asks the *operand's declared type*, and `declared_type_of` did not read that call.
///
/// Measured on 19beta1, 2026-09-11, through `PREPARE`/`EXECUTE` so the parameter is bound:
///
/// ```text
/// $1::oidvector::integer[]   [0:1]={1,2}      $1::oidvector::text[]      [0:1]={1,2}
/// $1::int2vector::integer[]  [0:1]={1,2}      parameter type inferred    oidvector
/// ```
///
/// The **zero** lower bound is what makes this a vector rather than an array, and it survives the
/// bind exactly as it survives the literal.
#[test]
fn a_vector_cast_through_a_bound_parameter_is_the_element_cast() {
    let mut client = Client::new();
    for (sql, oid) in [
        ("SELECT $1::oidvector::integer[]", 1007_u32),
        ("SELECT $1::oidvector::text[]", 1009),
        ("SELECT $1::oidvector::bigint[]", 1016),
        ("SELECT $1::int2vector::integer[]", 1007),
        ("SELECT $1::int2vector::text[]", 1009),
    ] {
        assert_eq!(
            client.ask_row(sql, "1 2"),
            Ok((oid, Some("[0:1]={1,2}".to_owned()))),
            "{sql}"
        );
    }
    // The vector itself, which was right in both modes and says the road only forks at the array.
    assert_eq!(
        client.ask_row("SELECT $1::oidvector", "1 2"),
        Ok((30, Some("1 2".to_owned())))
    );
}

/// **A `regclass[]` through a bound parameter** — the other eleven, and a different mechanism.
///
/// `$1::regclass[]` was `0A000 a relation name read as a regclass without a catalog`, a sentence
/// from `crate::value`, which by invariant 7 has no catalog: `bind::substitute` reads the bound
/// text with `Datum::from_text` and a `regclass`'s input function is a **relation lookup**. The
/// scalar `$1::regclass` answers because its parameter is typed `text` and the lookup happens one
/// pass later, in `resolve_regclass`, where the catalog is.
///
/// Measured on 19beta1 through `PREPARE`/`EXECUTE`: `$1::regclass[]` is `{pg_class}`, the
/// parameter inferred as `regclass[]` — which this node infers too, so the inference was never the
/// defect.
#[test]
fn a_regclass_array_through_a_bound_parameter_resolves_its_names() {
    let mut client = Client::new();
    assert_eq!(
        client.ask_row("SELECT $1::regclass[]", "{\"pg_class\"}"),
        Ok((2210, Some("{pg_class}".to_owned())))
    );
    for (sql, oid) in [
        ("SELECT $1::regclass[]::text[]", 1009_u32),
        ("SELECT $1::regclass[]::name[]", 1003),
    ] {
        assert_eq!(
            client.ask_row(sql, "{\"pg_class\"}"),
            Ok((oid, Some("{pg_class}".to_owned()))),
            "{sql}"
        );
    }
    // The scalar, which answered all along — the two now take the same road.
    assert_eq!(
        client.ask_row("SELECT $1::regclass", "pg_class"),
        Ok((2205, Some("pg_class".to_owned())))
    );
    // **A name nothing answers to is still `42P01`**, which is the half the catalog road must
    // keep: resolving through `value` could only ever have been a refusal, and resolving through
    // the catalog has to be able to refuse too.
    assert_eq!(
        client.ask_row("SELECT $1::regclass[]", "{nosuchrelation}"),
        Err("42P01".to_owned())
    );
}
