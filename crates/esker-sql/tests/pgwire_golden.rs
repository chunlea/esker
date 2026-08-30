//! The wire protocol, checked against bytes a real client and a real server exchanged.
//!
//! `golden/pgwire.hex` was recorded off a socket between `psql` 18.6 and PostgreSQL 19beta1
//! through a proxy that logged both directions. Nothing in it was written from the specification,
//! which matters because the specification is where a plausible-but-wrong reading comes from: the
//! length that does or does not count itself, the NULL that is -1 rather than 0, the command tag
//! for a `COMMIT` that PostgreSQL decides was really a `ROLLBACK`.
//!
//! Two directions are asserted. Our **decoder** must read what `psql` sends, and our **encoder**
//! must produce, byte for byte, what PostgreSQL sends.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_sql::error::SqlError;
use esker_sql::pgwire::message::{
    Backend, ErrorField, FieldDescription, Frontend, Startup, Target, TransactionStatus, decode,
    decode_startup,
};
use esker_sql::pgwire::{Negotiation, error_fields, negotiation};

const GOLDEN: &str = include_str!("golden/pgwire.hex");

/// The named byte string from the golden file.
fn golden(name: &str) -> Vec<u8> {
    for line in GOLDEN.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, hex)) = line.split_once('\t') else {
            panic!("golden line is not name<tab>hex: {line}");
        };
        if key.trim() == name {
            return (0..hex.trim().len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex.trim()[i..i + 2], 16).expect("hex"))
                .collect();
        }
    }
    panic!("no golden named {name}");
}

/// Splits a captured message into its tag and body, the way a framing reader would.
fn framed(name: &str) -> (u8, Vec<u8>) {
    let bytes = golden(name);
    let length = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    assert_eq!(
        length + 1,
        bytes.len(),
        "{name}: the length prefix counts itself and excludes the tag"
    );
    (bytes[0], bytes[5..].to_vec())
}

// --- startup ----------------------------------------------------------------------------------

#[test]
fn the_startup_packet_psql_sends_is_understood() {
    let startup = decode_startup(&golden("f_startup_30")).unwrap();
    let Startup::Parameters {
        major,
        minor,
        parameters,
    } = startup
    else {
        panic!("expected a startup packet");
    };
    assert_eq!((major, minor), (3, 0), "psql 18.6 asks for 3.0 by default");
    assert_eq!(
        parameters,
        vec![
            ("user".to_owned(), "esker".to_owned()),
            ("database".to_owned(), "esker".to_owned()),
            ("application_name".to_owned(), "psql".to_owned()),
        ]
    );
}

/// The packet a current `libpq` sends when told `max_protocol_version=latest`. A server that
/// cannot read this one, or that answers it with an error, is unreachable by that client.
#[test]
fn the_startup_packet_asking_for_3_2_is_understood_and_downgraded() {
    let startup = decode_startup(&golden("f_startup_32")).unwrap();
    let Startup::Parameters { major, minor, .. } = &startup else {
        panic!("expected a startup packet");
    };
    assert_eq!((*major, *minor), (3, 2));
    assert_eq!(
        negotiation(&startup),
        Negotiation::Downgrade {
            unsupported_options: Vec::new()
        },
        "3.2 must be downgraded to 3.0, never refused"
    );
}

#[test]
fn an_ssl_request_is_recognised_and_is_not_a_startup_packet() {
    assert_eq!(
        decode_startup(&golden("f_ssl_request")).unwrap(),
        Startup::SslRequest
    );
    // The refusal is a single bare byte, not a framed message.
    assert_eq!(golden("b_ssl_refused"), b"N");
}

/// Byte-for-byte against a `NegotiateProtocolVersion` that PostgreSQL 19beta1 actually sent, in
/// answer to a startup packet asking for minor version 9.
#[test]
fn negotiate_protocol_version_matches_postgresqls_own_bytes() {
    let no_options: [String; 0] = [];
    let ours = Backend::NegotiateProtocolVersion {
        // PostgreSQL 19 answered with its own newest, 3.2. We say 3.0, so the golden is compared
        // against a message built with the same newest value it reported.
        newest_minor: 2,
        unsupported_options: &no_options,
    }
    .to_bytes();
    assert_eq!(ours, golden("b_negotiate_no_options"));

    let one = ["_pq_.made_up_thing".to_owned()];
    let ours = Backend::NegotiateProtocolVersion {
        newest_minor: 2,
        unsupported_options: &one,
    }
    .to_bytes();
    assert_eq!(
        ours,
        golden("b_negotiate_one_option"),
        "unknown _pq_ options are listed by name, as PostgreSQL 19 lists them"
    );
}

#[test]
fn the_startup_replies_match_postgresqls_bytes() {
    assert_eq!(Backend::AuthenticationOk.to_bytes(), golden("b_auth_ok"));
    assert_eq!(
        Backend::BackendKeyData {
            pid: 0x0000_0050,
            key: 0x351b_d1dd,
        }
        .to_bytes(),
        golden("b_backend_key_30"),
        "in protocol 3.0 the cancel key is exactly four bytes"
    );
    assert_eq!(
        Backend::ReadyForQuery(TransactionStatus::Idle).to_bytes(),
        golden("b_ready_idle")
    );
    assert_eq!(
        Backend::ReadyForQuery(TransactionStatus::InTransaction).to_bytes(),
        golden("b_ready_in_transaction")
    );
    assert_eq!(
        Backend::ReadyForQuery(TransactionStatus::Failed).to_bytes(),
        golden("b_ready_failed")
    );
}

// --- simple query -------------------------------------------------------------------------------

#[test]
fn the_query_and_terminate_psql_sends_are_understood() {
    let (tag, body) = framed("f_query_select");
    assert_eq!(
        decode(tag, &body).unwrap(),
        Frontend::Query("SELECT a, b, c FROM g ORDER BY a".to_owned())
    );
    let (tag, body) = framed("f_terminate");
    assert_eq!(decode(tag, &body).unwrap(), Frontend::Terminate);
}

/// The reply to `SELECT a int8, b text, c bool`, as PostgreSQL described it.
#[test]
fn row_description_matches_postgresqls_bytes() {
    let fields = [
        FieldDescription {
            name: "a".to_owned(),
            table_oid: 16385,
            column_id: 1,
            type_oid: 20, // int8
            type_size: 8,
            type_modifier: -1,
            format: 0,
        },
        FieldDescription {
            name: "b".to_owned(),
            table_oid: 16385,
            column_id: 2,
            type_oid: 25, // text
            type_size: -1,
            type_modifier: -1,
            format: 0,
        },
        FieldDescription {
            name: "c".to_owned(),
            table_oid: 16385,
            column_id: 3,
            type_oid: 16, // bool
            type_size: 1,
            type_modifier: -1,
            format: 0,
        },
    ];
    assert_eq!(
        Backend::RowDescription(&fields).to_bytes(),
        golden("b_row_description")
    );
}

/// Contract C3, in bytes rather than in prose: `INT8` is decimal text, `BOOL` is `t`/`f`, and NULL
/// is a length of -1 — not an empty string, which is a different value and a different row.
#[test]
fn data_rows_match_postgresqls_text_formats_including_null() {
    let row = [
        Some(b"1".to_vec()),
        Some(b"x".to_vec()),
        Some(b"t".to_vec()),
    ];
    assert_eq!(Backend::DataRow(&row).to_bytes(), golden("b_data_row_1"));

    let with_null = [Some(b"2".to_vec()), None, Some(b"f".to_vec())];
    assert_eq!(
        Backend::DataRow(&with_null).to_bytes(),
        golden("b_data_row_2_with_null")
    );
}

#[test]
fn command_tags_match_postgresqls_bytes() {
    assert_eq!(
        Backend::CommandComplete("SELECT 2").to_bytes(),
        golden("b_command_complete_select_2")
    );
    assert_eq!(
        Backend::CommandComplete("BEGIN").to_bytes(),
        golden("b_command_complete_begin")
    );
    // Committing a transaction that has already failed is reported as ROLLBACK. Captured, not
    // guessed -- this is the kind of detail that would otherwise be wrong for years.
    assert_eq!(
        Backend::CommandComplete("ROLLBACK").to_bytes(),
        golden("b_command_complete_rollback")
    );
}

// --- errors -------------------------------------------------------------------------------------

/// Splits a captured `ErrorResponse` into its fields.
fn error_field_map(name: &str) -> Vec<(u8, String)> {
    let (tag, body) = framed(name);
    assert_eq!(tag, b'E');
    let mut fields = Vec::new();
    let mut rest = &body[..];
    while let Some((&code, tail)) = rest.split_first() {
        if code == 0 {
            break;
        }
        let end = tail.iter().position(|b| *b == 0).expect("terminated");
        fields.push((code, String::from_utf8(tail[..end].to_vec()).expect("utf8")));
        rest = &tail[end + 1..];
    }
    fields
}

/// Our error must carry the same severity, code and message PostgreSQL sends for the same
/// condition. The `F`/`L`/`R` fields it also sends are its own source file, line and function, and
/// are deliberately not reproduced — they would be a lie about where the error came from.
#[test]
fn our_errors_carry_the_same_fields_postgresql_sends() {
    for (golden_name, ours) in [
        (
            "b_error_undefined_table",
            SqlError::UndefinedTable("nope".into()),
        ),
        (
            "b_error_in_failed_transaction",
            SqlError::InFailedTransaction,
        ),
    ] {
        let theirs = error_field_map(golden_name);
        let mine = error_fields(&ours);
        for (field, label) in [
            (ErrorField::SEVERITY, "severity"),
            (ErrorField::SEVERITY_UNLOCALIZED, "unlocalised severity"),
            (ErrorField::CODE, "SQLSTATE"),
            (ErrorField::MESSAGE, "message"),
        ] {
            let expected = theirs
                .iter()
                .find(|(code, _)| *code == field.0)
                .unwrap_or_else(|| panic!("{golden_name} has no {label} field"));
            let actual = mine
                .iter()
                .find(|(code, _)| *code == field)
                .unwrap_or_else(|| panic!("we send no {label}"));
            assert_eq!(
                actual.1, expected.1,
                "{golden_name}: our {label} differs from PostgreSQL's"
            );
        }
    }
}

// --- extended query -----------------------------------------------------------------------------

#[test]
fn the_extended_protocol_messages_psql_sends_are_understood() {
    let (tag, body) = framed("f_parse");
    assert_eq!(
        decode(tag, &body).unwrap(),
        Frontend::Parse {
            statement: String::new(),
            sql: "SELECT a, b FROM g WHERE a > $1".to_owned(),
            param_types: Vec::new(),
        }
    );

    let (tag, body) = framed("f_bind");
    assert_eq!(
        decode(tag, &body).unwrap(),
        Frontend::Bind {
            portal: String::new(),
            statement: String::new(),
            param_formats: Vec::new(),
            params: vec![Some(b"0".to_vec())],
            result_formats: vec![0],
        }
    );

    let (tag, body) = framed("f_describe_portal");
    assert_eq!(
        decode(tag, &body).unwrap(),
        Frontend::Describe {
            target: Target::Portal,
            name: String::new(),
        }
    );

    let (tag, body) = framed("f_execute");
    assert_eq!(
        decode(tag, &body).unwrap(),
        Frontend::Execute {
            portal: String::new(),
            max_rows: 0,
        }
    );

    let (tag, body) = framed("f_sync");
    assert_eq!(decode(tag, &body).unwrap(), Frontend::Sync);
}

// --- invariant 9: nothing a socket delivers may panic ---------------------------------------

// Every length, count and offset in a frontend message is chosen by whoever opened the socket.
// A `Bind` can claim eighty thousand parameters in a twelve-byte body, a string can run off the
// end without its NUL, a length can be negative. All of it must come back as a typed error.
proptest::proptest! {
    #[test]
    fn decoding_arbitrary_bytes_never_panics(tag: u8, body: Vec<u8>) {
        let _ = decode(tag, &body);
    }

    #[test]
    fn decoding_an_arbitrary_startup_packet_never_panics(packet: Vec<u8>) {
        let _ = decode_startup(&packet);
    }

    /// Bytes shaped like real messages find the paths that random noise never reaches: a body
    /// whose declared counts are plausible is what actually exercises the loops.
    #[test]
    fn decoding_plausible_messages_never_panics(
        tag in proptest::sample::select(vec![
            b'Q', b'P', b'B', b'D', b'E', b'C', b'S', b'H', b'X', b'p', b'?',
        ]),
        body in proptest::collection::vec(
            proptest::sample::select(vec![0u8, 1, 2, 0xff, b'S', b'P', b'x']),
            0..64,
        ),
    ) {
        let _ = decode(tag, &body);
    }
}

/// The specific hostile shapes, named, because a property test finds them by luck and a
/// regression needs them by name.
#[test]
fn hostile_lengths_are_errors_and_not_allocations() {
    // A Bind claiming 0x7fff parameters in a body far too small to hold them.
    let mut body = Vec::new();
    body.push(0); // portal ""
    body.push(0); // statement ""
    body.extend_from_slice(&0i16.to_be_bytes()); // no format codes
    body.extend_from_slice(&0x7fffi16.to_be_bytes()); // 32767 parameters
    assert!(
        decode(b'B', &body).is_err(),
        "an impossible count must be refused"
    );

    // A parameter length that is negative but is not the -1 that means NULL.
    let mut body = vec![0, 0];
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&1i16.to_be_bytes());
    body.extend_from_slice(&(-7i32).to_be_bytes());
    assert!(decode(b'B', &body).is_err(), "-7 is not a NULL");

    // A string with no terminator.
    assert!(
        decode(b'Q', b"SELECT 1").is_err(),
        "an unterminated string must be refused"
    );

    // A startup packet whose declared length disagrees with what arrived.
    let mut packet = 999u32.to_be_bytes().to_vec();
    packet.extend_from_slice(&196_608u32.to_be_bytes());
    assert!(decode_startup(&packet).is_err());

    // A startup packet whose parameter list never ends.
    let mut body = 196_608u32.to_be_bytes().to_vec();
    body.extend_from_slice(b"user\x00esker");
    let mut packet = u32::try_from(body.len() + 4)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    packet.extend_from_slice(&body);
    assert!(decode_startup(&packet).is_err());
}

/// An unknown message type is kept rather than dropped, so the session can refuse it precisely.
/// Dropping the connection instead would turn "I do not implement `FunctionCall`" into a network
/// error the client cannot diagnose.
#[test]
fn an_unimplemented_message_type_survives_decoding() {
    let decoded = decode(b'F', b"whatever").unwrap();
    assert_eq!(
        decoded,
        Frontend::Unknown {
            tag: b'F',
            body: b"whatever".to_vec()
        }
    );
}
