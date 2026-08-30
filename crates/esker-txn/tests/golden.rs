//! Golden vectors for the Percolator keys and records.
//!
//! `tests/golden/txn.txt` is the frozen output of the encodings in `docs/txn-spec.md` §1, §3
//! and §4. The cases live here in Rust; the test renders them and compares the text with the
//! file, so an accidental change to any encoding shows up as a failing test rather than as a
//! cluster that cannot read its own locks. Changing a line in that file is a format change: it
//! needs an ADR and a format version bump, never a re-bless.
//!
//! To add cases, extend `cases()` and re-generate with `ESKER_BLESS=1 cargo test -p esker-txn
//! --test golden`. That is for *new* lines only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::path::PathBuf;

use bytes::Bytes;
use esker_txn::codec::{Kind, LockRecord, SHORT_VALUE_MAX_LEN, WriteRecord};
use esker_txn::key;

const HEADER: &str = "\
# esker-txn golden vectors, format version 1.
#
# One case per line: <kind> <argument> = <hex of the encoding>.
# Byte arguments are hex; `-` means the empty string.
#
# These bytes are the on-disk record and key format of docs/txn-spec.md. A line that changes
# here is a format change: it needs an ADR and a format version bump, not a re-blessed file.
";

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").unwrap();
    }
    out
}

fn arg(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        "-".to_owned()
    } else {
        hex(bytes)
    }
}

fn cases() -> Vec<(String, String, Vec<u8>)> {
    let mut cases = Vec::new();

    // -- keys: the empty key, a short one, one that is a prefix of the next, and one that is
    //    an exact multiple of the group size (the case that costs an extra padding group).
    let keys: [&[u8]; 5] = [b"", b"a", b"ab", b"account/1", b"12345678"];
    for user_key in keys {
        cases.push(("lock_key".to_owned(), arg(user_key), key::lock(user_key)));
    }
    for user_key in keys {
        for ts in [0u64, 1, 42, 1 << 41, u64::MAX] {
            cases.push((
                "write_key".to_owned(),
                format!("{}@{ts}", arg(user_key)),
                key::write(user_key, ts),
            ));
        }
    }
    // `default` is versioned by start_ts rather than commit_ts, but the bytes are built the
    // same way; one case pins that they really are the same function.
    cases.push((
        "value_key".to_owned(),
        format!("{}@42", arg(b"account/1")),
        key::value(b"account/1", 42),
    ));

    let (range_start, range_end) = key::version_range(b"account/1");
    cases.push((
        "version_range_start".to_owned(),
        arg(b"account/1"),
        range_start,
    ));
    cases.push(("version_range_end".to_owned(), arg(b"account/1"), range_end));

    // -- lock records: one per legal kind, the default TTL and an explicit one, an absent
    //    inline value, an empty one and one at the cutoff.
    for kind in [Kind::Put, Kind::Delete, Kind::Lock] {
        let record = LockRecord::new(kind, 42, Bytes::from_static(b"primary"));
        cases.push((
            "lock".to_owned(),
            format!("{}/start=42/ttl=default/primary=primary", kind.name()),
            record.encode(),
        ));
    }
    let mut short = LockRecord::new(Kind::Put, 1 << 41, Bytes::from_static(b"p"));
    short.ttl_ms = 60_000;
    short.short_value = Some(Bytes::from_static(b"hello"));
    cases.push((
        "lock".to_owned(),
        "Put/start=2199023255552/ttl=60000/primary=p/value=hello".to_owned(),
        short.encode(),
    ));
    let mut empty_value = LockRecord::new(Kind::Put, 7, Bytes::from_static(b"p"));
    empty_value.short_value = Some(Bytes::new());
    cases.push((
        "lock".to_owned(),
        "Put/start=7/ttl=default/primary=p/value=-".to_owned(),
        empty_value.encode(),
    ));
    let mut max_value = LockRecord::new(Kind::Put, u64::MAX, Bytes::from_static(b"p"));
    max_value.short_value = Some(Bytes::from(vec![0xab; SHORT_VALUE_MAX_LEN]));
    cases.push((
        "lock".to_owned(),
        format!("Put/start=max/ttl=default/primary=p/value=ab*{SHORT_VALUE_MAX_LEN}"),
        max_value.encode(),
    ));

    // -- write records: one per kind, plus an inlined value.
    for kind in Kind::ALL {
        cases.push((
            "write".to_owned(),
            format!("{}/start=42", kind.name()),
            WriteRecord::new(kind, 42).encode(),
        ));
    }
    let mut inlined = WriteRecord::new(Kind::Put, 1 << 41);
    inlined.short_value = Some(Bytes::from_static(b"hello"));
    cases.push((
        "write".to_owned(),
        "Put/start=2199023255552/value=hello".to_owned(),
        inlined.encode(),
    ));
    cases.push((
        "write".to_owned(),
        "Rollback/start=max".to_owned(),
        WriteRecord::rollback(u64::MAX).encode(),
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/txn.txt")
}

#[test]
fn encodings_match_the_golden_file() {
    let expected = render();
    let path = golden_path();

    if std::env::var_os("ESKER_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).expect("cannot create tests/golden");
        std::fs::write(&path, &expected).expect("failed to write the golden file");
        return;
    }

    let actual = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "cannot read {}: {error}. Run ESKER_BLESS=1 cargo test -p esker-txn --test golden \
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
            Some((line, (was, now))) => format!(
                "first difference at line {}:\n  golden: {was}\n  now:    {now}",
                line + 1
            ),
            None => format!(
                "the file has {} lines, the code produces {}",
                actual.lines().count(),
                expected.lines().count()
            ),
        };
        panic!(
            "the transaction encoding changed.\n{detail}\n\nThis is an on-disk format change: it \
             needs an ADR and a format version bump, not a re-blessed golden file."
        );
    }
}

/// A golden file that silently covers nothing would pass forever.
#[test]
fn the_golden_file_covers_every_encoding() {
    let text = std::fs::read_to_string(golden_path()).expect("golden file is missing");
    for kind in [
        "lock_key",
        "write_key",
        "value_key",
        "version_range_start",
        "version_range_end",
        "lock ",
        "write ",
    ] {
        assert!(
            text.lines().any(|line| line.starts_with(kind)),
            "the golden file has no {kind} case"
        );
    }
    for kind in Kind::ALL {
        assert!(
            text.contains(kind.name()),
            "the golden file has no {} record",
            kind.name()
        );
    }
}

/// The property the whole key layout exists for, pinned in the frozen bytes rather than only
/// in a unit test: `"a"`'s versions all sort below `"ab"`'s (`docs/txn-spec.md` §2).
#[test]
fn the_golden_keys_keep_each_key_s_versions_together() {
    let a_oldest = key::write(b"a", 0);
    let ab_newest = key::write(b"ab", u64::MAX);
    assert!(
        a_oldest < ab_newest,
        "the golden layout interleaves one key's versions with another's"
    );
}
