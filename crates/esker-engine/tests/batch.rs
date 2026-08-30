//! The write batch's byte layout, pinned and fuzzed.
//!
//! The golden file was produced by an encoder written separately from the one under test, so
//! it checks the implementation rather than agreeing with it. Everything else here is about
//! `CLAUDE.md` invariant 9: these bytes come off a disk that was being written to when the
//! machine died, so no arrangement of them may panic.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use esker_engine::batch::{HEADER_SIZE, WriteBatch};
use esker_engine::dbformat::EntryKind;
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

/// The batch of `docs/DESIGN.md` §4.3, byte for byte.
#[test]
fn golden_write_batch() {
    let golden = include_str!("golden/write-batch.hex");
    let field = |name: &str| -> Vec<&str> {
        golden
            .lines()
            .filter_map(|line| line.strip_prefix(&format!("{name} ")))
            .collect()
    };

    let mut batch = WriteBatch::new();
    batch.set_seqno(100);
    batch.put(0, b"alpha", b"one");
    batch.delete(1, b"beta");
    batch.delete_range(2, b"g", b"m");
    batch.put(300, b"", b"");

    let header = field("header");
    let entries = field("entry");
    assert_eq!(header.len(), 1);
    assert_eq!(entries.len(), 4);
    assert_eq!(hex(&batch.as_bytes()[..HEADER_SIZE]), header[0]);
    assert_eq!(
        hex(batch.as_bytes()),
        format!("{}{}", header[0], entries.concat()),
        "the batch is its header followed by its entries, in order"
    );

    // And the golden bytes decode back to what built them.
    let decoded = WriteBatch::from_bytes(batch.as_bytes()).unwrap();
    let kinds: Vec<EntryKind> = decoded.iter().map(|e| e.unwrap().kind).collect();
    assert_eq!(
        kinds,
        [
            EntryKind::Put,
            EntryKind::Delete,
            EntryKind::DeleteRange,
            EntryKind::Put
        ]
    );
    assert_eq!(
        decoded.iter().map(|e| e.unwrap().seqno).collect::<Vec<_>>(),
        [100, 101, 102, 103]
    );
}

/// One operation to apply to a batch, and the model of what it should read back as.
#[derive(Debug, Clone)]
enum Op {
    Put(u32, Vec<u8>, Vec<u8>),
    Delete(u32, Vec<u8>),
    DeleteRange(u32, Vec<u8>, Vec<u8>),
}

fn op() -> impl Strategy<Value = Op> {
    let key = prop::collection::vec(any::<u8>(), 0..24);
    let value = prop::collection::vec(any::<u8>(), 0..64);
    // Column-family ids spread across the varint length boundaries.
    let cf = prop_oneof![
        Just(0u32),
        1..8u32,
        120..200u32,
        16_000..17_000u32,
        3_000_000..3_000_100u32
    ];
    prop_oneof![
        (cf.clone(), key.clone(), value.clone()).prop_map(|(c, k, v)| Op::Put(c, k, v)),
        (cf.clone(), key.clone()).prop_map(|(c, k)| Op::Delete(c, k)),
        (cf, key, value).prop_map(|(c, k, v)| Op::DeleteRange(c, k, v)),
    ]
}

proptest! {
    /// Whatever is put in comes out, in order, with the right kinds and sequence numbers.
    #[test]
    fn batches_round_trip(ops in prop::collection::vec(op(), 0..24), seqno in 0u64..(1 << 56)) {
        let mut batch = WriteBatch::new();
        batch.set_seqno(seqno);
        for op in &ops {
            match op {
                Op::Put(cf, key, value) => batch.put(*cf, key, value),
                Op::Delete(cf, key) => batch.delete(*cf, key),
                Op::DeleteRange(cf, begin, end) => batch.delete_range(*cf, begin, end),
            }
        }
        prop_assert_eq!(batch.count() as usize, ops.len());

        let decoded = WriteBatch::from_bytes(batch.as_bytes())?;
        prop_assert_eq!(decoded.count(), batch.count());
        for (index, (entry, op)) in decoded.iter().zip(&ops).enumerate() {
            let entry = entry?;
            prop_assert_eq!(entry.seqno, seqno + index as u64);
            match op {
                Op::Put(cf, key, value) => {
                    prop_assert_eq!(entry.kind, EntryKind::Put);
                    prop_assert_eq!((entry.cf, entry.key, entry.value), (*cf, &key[..], &value[..]));
                }
                Op::Delete(cf, key) => {
                    prop_assert_eq!(entry.kind, EntryKind::Delete);
                    prop_assert_eq!((entry.cf, entry.key, entry.value), (*cf, &key[..], &[][..]));
                }
                Op::DeleteRange(cf, begin, end) => {
                    prop_assert_eq!(entry.kind, EntryKind::DeleteRange);
                    prop_assert_eq!((entry.cf, entry.key, entry.value), (*cf, &begin[..], &end[..]));
                }
            }
        }
    }

    /// Merging batches is concatenation, renumbered from the leader's base. Group commit is
    /// built on this being true.
    #[test]
    fn appending_is_concatenation(
        left in prop::collection::vec(op(), 0..8),
        right in prop::collection::vec(op(), 0..8),
    ) {
        let build = |ops: &[Op], seqno: u64| {
            let mut batch = WriteBatch::new();
            batch.set_seqno(seqno);
            for op in ops {
                match op {
                    Op::Put(cf, key, value) => batch.put(*cf, key, value),
                    Op::Delete(cf, key) => batch.delete(*cf, key),
                    Op::DeleteRange(cf, begin, end) => batch.delete_range(*cf, begin, end),
                }
            }
            batch
        };
        let mut merged = build(&left, 10);
        merged.append(&build(&right, 9_999));
        let separately = build(&[left.clone(), right.clone()].concat(), 10);

        prop_assert_eq!(merged.count(), separately.count());
        prop_assert_eq!(merged.as_bytes(), separately.as_bytes());
    }

    /// Arbitrary bytes off a damaged disk: decoding may fail, but it may not panic and it may
    /// not report success on something it cannot read back (invariant 9).
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        if let Ok(batch) = WriteBatch::from_bytes(&bytes) {
            let mut seen = 0u32;
            for entry in &batch {
                prop_assert!(entry.is_ok(), "from_bytes accepted a batch it cannot iterate");
                seen += 1;
            }
            prop_assert_eq!(seen, batch.count());
            prop_assert_eq!(batch.as_bytes(), &bytes[..]);
        }
    }

    /// Damaging one byte of a valid batch must never turn it into a panic, and the header is
    /// small enough that most damage to it is caught.
    #[test]
    fn one_damaged_byte_never_panics(index in 0usize..40, mask in 1u8..=255) {
        let mut batch = WriteBatch::new();
        batch.set_seqno(7);
        batch.put(3, b"key", b"value");
        batch.delete(0, b"gone");
        let mut bytes = batch.as_bytes().to_vec();
        if index < bytes.len() {
            bytes[index] ^= mask;
            if let Ok(damaged) = WriteBatch::from_bytes(&bytes) {
                for entry in &damaged {
                    let _ = entry?;
                }
            }
        }
    }
}
