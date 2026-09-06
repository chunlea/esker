//! **32767 works and 32768 kills the connection** — a signed read of an unsigned wire field.
//!
//! `ActiveRecord`'s `where(id: […])` over a large array sends one `Parse` with a parameter per
//! element. r1 pinned the boundary exactly on run 102's node: 32767 answers and 32768 does not,
//! and the client sees
//!
//! ```text
//! PQconsumeInput() FATAL:  negative the parameter type count
//! server closed the connection unexpectedly
//! ```
//!
//! 32768 is `i16::MAX + 1`, and read as a signed 16-bit it is `-32768`. **The protocol's counts are
//! Int16 and PostgreSQL reads them unsigned** — `pq_getmsgint` widens a `uint16` — so the range is
//! `0..=65535` and a count above 32767 is ordinary. Verified from the other side by r1: PG19
//! answers the identical five frames, byte for byte, at 32768.
//!
//! The reply half is the same mistake pointing outwards: `ParameterDescription` wrote its count
//! through `i16::try_from(…).unwrap_or(i16::MAX)`, so a statement with 32768 parameters would have
//! announced **32767** of them and left the client reading the next message from the middle of
//! this one. That one is quieter than a dropped connection and worse.
//!
//! What is *not* fixed here, because it is not broken: `Bind`'s value count and the format-code
//! arrays go through the same reader and are now unsigned with it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "parity_harness/mod.rs"]
mod parity;

use esker_sql::pgwire::message::{Backend, Frontend, Target, decode};
use esker_sql::pgwire::session::Session;

/// A `Parse` frame body carrying `count` declared parameter types, all `0` ("you decide").
fn parse_body(sql: &str, count: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(0); // the unnamed statement
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    body.extend_from_slice(&count.to_be_bytes());
    for _ in 0..count {
        body.extend_from_slice(&0u32.to_be_bytes());
    }
    body
}

#[test]
fn a_parse_may_declare_more_than_i16_max_parameter_types() {
    // 32767 was never the problem; it is here so the boundary is a boundary and not a guess.
    for count in [0u16, 1, 32767, 32768, 65535] {
        let body = parse_body("SELECT 1", count);
        match decode(b'P', &body) {
            Ok(Frontend::Parse { param_types, .. }) => {
                assert_eq!(param_types.len(), usize::from(count), "at {count}");
            }
            other => panic!("at {count}: {other:?}"),
        }
    }
}

#[test]
fn a_parameter_description_announces_every_type_it_carries() {
    let types: Vec<u32> = vec![23; 32768];
    let mut out = Vec::new();
    Backend::ParameterDescription(&types).encode(&mut out);
    // Tag, four length bytes, then the count: `32768` as an unsigned 16-bit is `0x8000`.
    assert_eq!(out[0], b't');
    assert_eq!(&out[5..7], &[0x80, 0x00]);
    // And the frame is as long as the count it announced, which is what a client reads by.
    let length = u32::from_be_bytes([out[1], out[2], out[3], out[4]]) as usize;
    assert_eq!(length, 4 + 2 + 32768 * 4);
}

#[test]
fn a_statement_with_more_than_i16_max_parameters_answers() {
    // **Through the wire, because that is where the count is read.** The five frames the `pg` gem
    // sends for `exec_params`, with the statement the suite actually sends — `COUNT(*)` over an
    // `IN` list one parameter past the old boundary.
    let mut node = parity::Node::new(&[
        "CREATE TABLE topics (id int PRIMARY KEY)",
        "INSERT INTO topics VALUES (1)",
    ]);
    let mut session = Session::new();
    let count: usize = 32768;
    let placeholders: Vec<String> = (1..=count).map(|n| format!("${n}")).collect();
    let sql = format!(
        "SELECT COUNT(*) FROM topics WHERE id IN ({})",
        placeholders.join(", ")
    );

    let mut out = Vec::new();
    session.handle(
        &Frontend::Parse {
            statement: String::new(),
            sql,
            param_types: vec![0; count],
        },
        &mut node.executor,
        &mut out,
    );
    assert!(
        out.starts_with(b"1"),
        "Parse was refused: {}",
        String::from_utf8_lossy(&out)
    );

    out.clear();
    session.handle(
        &Frontend::Bind {
            portal: String::new(),
            statement: String::new(),
            param_formats: Vec::new(),
            params: (1..=count)
                .map(|n| Some(n.to_string().into_bytes()))
                .collect(),
            result_formats: Vec::new(),
        },
        &mut node.executor,
        &mut out,
    );
    assert!(
        out.starts_with(b"2"),
        "Bind was refused: {}",
        String::from_utf8_lossy(&out)
    );

    out.clear();
    session.handle(
        &Frontend::Describe {
            target: Target::Portal,
            name: String::new(),
        },
        &mut node.executor,
        &mut out,
    );
    out.clear();
    session.handle(
        &Frontend::Execute {
            portal: String::new(),
            max_rows: 0,
        },
        &mut node.executor,
        &mut out,
    );
    let answer = String::from_utf8_lossy(&out);
    assert!(
        answer.contains('1') && !answer.contains("ERROR"),
        "the statement did not answer: {answer}"
    );
}
