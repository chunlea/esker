//! The manifest's on-disk shapes: `VersionEdit` bytes, and what recovery makes of them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_engine::dbformat::{EntryKind, internal_key};
use esker_engine::version::{FileMeta, VersionEdit};
use proptest::prelude::*;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// The edit of `docs/DESIGN.md` §4.6, field by field, against a file written by a separate
/// encoder.
#[test]
fn golden_version_edit() {
    let golden = include_str!("golden/version-edit.hex");
    let fields: Vec<&str> = golden
        .lines()
        .filter_map(|line| line.strip_prefix("field "))
        .collect();
    assert_eq!(fields.len(), 9, "the golden file lost a field");

    let mut edit = VersionEdit::new();
    edit.comparator = Some("esker.BytewiseComparator".into());
    edit.log_number = Some(11);
    edit.next_file_number = Some(12);
    edit.last_seqno = Some((1 << 56) - 1);
    edit.cf_added.push((0, "default".into()));
    edit.cf_added.push((7, "write".into()));
    edit.cf_dropped.push(3);
    edit.delete_file(0, 1, 5);
    edit.add_file(
        0,
        0,
        FileMeta {
            number: 8,
            size: 32_768,
            smallest: internal_key(b"apple", 10, EntryKind::Put),
            largest: internal_key(b"pear", 20, EntryKind::Put),
            smallest_seqno: 10,
            largest_seqno: 20,
        },
    );

    assert_eq!(
        hex(&edit.encode()),
        fields.concat(),
        "the encoded edit is its fields, in the order encode() writes them"
    );
    assert_eq!(VersionEdit::decode(&edit.encode()).unwrap(), edit);
}

fn key() -> impl Strategy<Value = Vec<u8>> {
    (prop::collection::vec(any::<u8>(), 0..16), 0u64..1_000_000)
        .prop_map(|(user, seq)| internal_key(&user, seq, EntryKind::Put))
}

fn file_meta() -> impl Strategy<Value = FileMeta> {
    (
        any::<u64>(),
        any::<u64>(),
        key(),
        key(),
        0u64..(1 << 56),
        0u64..(1 << 56),
    )
        .prop_map(|(number, size, smallest, largest, a, b)| FileMeta {
            number,
            size,
            smallest,
            largest,
            smallest_seqno: a.min(b),
            largest_seqno: a.max(b),
        })
}

proptest! {
    #[test]
    fn edits_round_trip(
        comparator in prop::option::of("[a-zA-Z.]{1,32}"),
        log_number in prop::option::of(any::<u64>()),
        next_file_number in prop::option::of(any::<u64>()),
        last_seqno in prop::option::of(0u64..(1 << 56)),
        cf_added in prop::collection::vec((any::<u32>(), "[a-z]{1,12}"), 0..4),
        cf_dropped in prop::collection::vec(any::<u32>(), 0..4),
        deleted in prop::collection::vec((any::<u32>(), 0u32..8, any::<u64>()), 0..6),
        added in prop::collection::vec((any::<u32>(), 0u32..8, file_meta()), 0..4),
    ) {
        let mut edit = VersionEdit::new();
        edit.comparator = comparator;
        edit.log_number = log_number;
        edit.next_file_number = next_file_number;
        edit.last_seqno = last_seqno;
        edit.cf_added = cf_added;
        edit.cf_dropped = cf_dropped;
        edit.deleted_files = deleted;
        edit.added_files = added;

        prop_assert_eq!(VersionEdit::decode(&edit.encode())?, edit);
    }

    /// Manifest bytes come off a disk that was being written to when the machine died.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..128)) {
        if let Ok(edit) = VersionEdit::decode(&bytes) {
            // Anything that decodes must re-encode to the same bytes: there is exactly one
            // encoding of an edit, so a second one would mean the decoder invented something.
            prop_assert_eq!(edit.encode(), bytes);
        }
    }
}
