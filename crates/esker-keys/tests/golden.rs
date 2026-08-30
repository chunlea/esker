//! Golden vectors for every encoding in `esker-keys`.
//!
//! `tests/golden/keys.txt` is the frozen output of the codec. The cases live here in Rust;
//! the test renders them and compares the text with the file, so an accidental change to any
//! encoding shows up as a failing test rather than as a cluster that cannot read its own
//! keys. Changing a line in that file is a format change: it needs an ADR and a format
//! version bump, never a re-bless.
//!
//! To add cases, extend `cases()` and re-generate with `ESKER_BLESS=1 cargo test -p esker-keys
//! --test golden`. That is for *new* lines only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::path::PathBuf;

use esker_keys::codec::{Value, enc_ts, encode_bytes, encode_i64, encode_tuple, encode_u64};
use esker_keys::prefix;

const HEADER: &str = "\
# esker-keys golden vectors, format version 1.
#
# One case per line: <kind> <argument> = <hex of the encoding>.
# Byte arguments are hex; `-` means the empty string.
#
# These bytes are the on-disk key format. A line that changes here is a format change:
# it needs an ADR and a format version bump, not a re-blessed file.
";

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").unwrap();
    }
    out
}

fn cases() -> Vec<(String, String, Vec<u8>)> {
    let mut cases = Vec::new();

    for value in [0u64, 1, 255, 256, 65_535, 65_536, 1 << 32, u64::MAX] {
        let mut encoded = Vec::new();
        encode_u64(value, &mut encoded);
        cases.push(("u64".to_owned(), value.to_string(), encoded));
    }

    for value in [i64::MIN, -1_000_000, -1, 0, 1, 1_000_000, i64::MAX] {
        let mut encoded = Vec::new();
        encode_i64(value, &mut encoded);
        cases.push(("i64".to_owned(), value.to_string(), encoded));
    }

    // Length classes that matter: empty, short, exactly one group, one over a group, two
    // groups, and bytes that look like markers.
    let byte_cases: [&[u8]; 11] = [
        b"",
        b"a",
        b"ab",
        b"abcdefg",
        b"abcdefgh",
        b"abcdefghi",
        b"abcdefghijklmnop",
        b"abcdefghijklmnopq",
        b"\x00",
        b"\xff\xff\xff\xff\xff\xff\xff\xff",
        b"hello world",
    ];
    for value in byte_cases {
        let mut encoded = Vec::new();
        encode_bytes(value, &mut encoded);
        let argument = if value.is_empty() {
            "-".to_owned()
        } else {
            hex(value)
        };
        cases.push(("bytes".to_owned(), argument, encoded));
    }

    for ts in [0u64, 1, 42, 1 << 41, u64::MAX - 1, u64::MAX] {
        cases.push(("ts".to_owned(), ts.to_string(), enc_ts(ts).to_vec()));
    }

    let tuples = [
        (
            "u64:7,bytes:region,i64:-3",
            vec![
                Value::U64(7),
                Value::Bytes(b"region".to_vec()),
                Value::I64(-3),
            ],
        ),
        (
            "bytes:,u64:0",
            vec![Value::Bytes(Vec::new()), Value::U64(0)],
        ),
    ];
    for (name, values) in tuples {
        let mut encoded = Vec::new();
        encode_tuple(&values, &mut encoded);
        cases.push(("tuple".to_owned(), name.to_owned(), encoded));
    }

    cases.push(("raw_key".to_owned(), hex(b"k1"), prefix::raw_key(b"k1")));
    cases.push((
        "txn_key".to_owned(),
        format!("{}@42", hex(b"k1")),
        prefix::txn_key(b"k1", 42),
    ));
    cases.push((
        "meta_key".to_owned(),
        hex(b"cluster"),
        prefix::meta_key(b"cluster"),
    ));
    cases.push((
        "table_row_prefix".to_owned(),
        "tenant=1,table=7".to_owned(),
        prefix::table_row_prefix(1, 7),
    ));
    cases.push((
        "table_index_prefix".to_owned(),
        "tenant=1,table=7,index=2".to_owned(),
        prefix::table_index_prefix(1, 7, 2),
    ));

    cases
}

fn render() -> String {
    let mut out = String::from(HEADER);
    for (kind, argument, encoded) in cases() {
        writeln!(out, "{kind} {argument} = {}", hex(&encoded)).unwrap();
    }
    out
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/keys.txt")
}

#[test]
fn encodings_match_the_golden_file() {
    let expected = render();
    let path = golden_path();

    if std::env::var_os("ESKER_BLESS").is_some() {
        std::fs::write(&path, &expected).expect("failed to write the golden file");
        return;
    }

    let actual = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}. Run ESKER_BLESS=1 cargo test -p esker-keys --test golden \
             only when adding new cases.",
            path.display()
        )
    });

    if actual != expected {
        let mismatch = actual
            .lines()
            .zip(expected.lines())
            .enumerate()
            .find(|(_, (a, b))| a != b);
        let detail = match mismatch {
            Some((line, (was, now))) => {
                format!(
                    "first difference at line {}:\n  golden: {was}\n  now:    {now}",
                    line + 1
                )
            }
            None => format!(
                "the file has {} lines, the code produces {}",
                actual.lines().count(),
                expected.lines().count()
            ),
        };
        panic!(
            "the key encoding changed.\n{detail}\n\nThis is an on-disk format change: it needs an \
             ADR and a format version bump, not a re-blessed golden file."
        );
    }
}

/// A golden file that silently covers nothing would pass forever.
#[test]
fn the_golden_file_covers_every_encoding() {
    let text = std::fs::read_to_string(golden_path()).expect("golden file is missing");
    for kind in [
        "u64",
        "i64",
        "bytes",
        "ts",
        "tuple",
        "raw_key",
        "txn_key",
        "meta_key",
        "table_row_prefix",
        "table_index_prefix",
    ] {
        assert!(
            text.lines()
                .any(|line| line.starts_with(&format!("{kind} "))),
            "the golden file has no {kind} case"
        );
    }
    let case_lines = text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .count();
    assert!(case_lines >= 35, "only {case_lines} golden cases");
}
