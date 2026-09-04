//! What ADR 0041 claimed, measured.
//!
//! The memtable's public surface, benched at the size a real one reaches: a full scan (the
//! number with the most headroom behind it, because the `crossbeam-skiplist` cursor re-finds
//! its key on every step), a short scan, point reads, and the two fill shapes.
//!
//! This binary measures **one** store — whichever `esker_engine::memtable`'s `Selected` names.
//! The A/B is two builds of it, alternated round by round, because a run of one arm followed by
//! a run of the other measures the machine as much as the code (`docs/plans/phase-11-engine.md`
//! §10). `docs/bench/skiplist.md` records how, and what came out.

#![allow(
    clippy::expect_used,
    reason = "a bench builds a real arena, which refuses only at four gigabytes; an entry it               declined would make the measurement meaningless, so saying so loudly is right"
)]

use std::sync::Arc;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_main};
use esker_engine::MemTable;
use esker_engine::dbformat::{
    BytewiseComparator, EntryKind, InternalKeyComparator, internal_key, lookup_key,
};

/// Entries in the table the read benchmarks run against. Chosen so the list is deep enough for
/// `O(log n)` per step to cost something a scan can see, and small enough to build per batch.
const ENTRIES: u64 = 100_000;

/// A deterministic scatter, so both arms see the same key order without carrying a generator.
/// Knuth's multiplicative constant over a 64-bit space.
fn scattered(i: u64) -> u64 {
    i.wrapping_mul(11_400_714_819_323_198_485) >> 24
}

fn table() -> Arc<MemTable> {
    Arc::new(MemTable::new(Arc::new(InternalKeyComparator::new(
        Arc::new(BytewiseComparator),
    ))))
}

/// `ENTRIES` scattered keys, each with a 64-byte value.
fn filled() -> Arc<MemTable> {
    let table = table();
    for i in 0..ENTRIES {
        let key = format!("key-{:012}", scattered(i));
        table
            .add(i + 1, EntryKind::Put, key.as_bytes(), &[b'v'; 64])
            .expect("the bench's arena is a real one and does not refuse");
    }
    table
}

fn fill(c: &mut Criterion) {
    let mut group = c.benchmark_group("fill");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("seq", |b| {
        b.iter_batched(
            table,
            |table| {
                for i in 0..ENTRIES {
                    let key = format!("key-{i:012}");
                    table
                        .add(i + 1, EntryKind::Put, key.as_bytes(), &[b'v'; 64])
                        .expect("the bench's arena is a real one and does not refuse");
                }
                table
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("random", |b| {
        b.iter_batched(
            table,
            |table| {
                for i in 0..ENTRIES {
                    let key = format!("key-{:012}", scattered(i));
                    table
                        .add(i + 1, EntryKind::Put, key.as_bytes(), &[b'v'; 64])
                        .expect("the bench's arena is a real one and does not refuse");
                }
                table
            },
            BatchSize::LargeInput,
        );
    });

    // Many versions of few keys: an append-only structure stores every one of them, so this is
    // where the arena's growth behaviour under repeated writes shows up.
    group.bench_function("overwrite", |b| {
        b.iter_batched(
            table,
            |table| {
                for i in 0..ENTRIES {
                    let key = format!("key-{:04}", i % 1000);
                    table
                        .add(i + 1, EntryKind::Put, key.as_bytes(), &[b'v'; 64])
                        .expect("the bench's arena is a real one and does not refuse");
                }
                table
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn read(c: &mut Criterion) {
    let table = filled();
    let mut group = c.benchmark_group("read");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(10));

    // The headline. A merge iterator calls `next` once per entry per child, so this is the cost
    // ADR 0041 said was `O(log n)` with an allocation where it should be a pointer hop.
    group.bench_function("scan_full", |b| {
        b.iter(|| {
            let mut iter = table.iter();
            iter.seek_to_first();
            let mut bytes = 0usize;
            while iter.valid() {
                bytes += iter.key().len() + iter.value().len();
                iter.next();
            }
            bytes
        });
    });

    // The `scanrange --batch-size 20` shape: a seek and a short walk, over and over.
    group.bench_function("scan_short", |b| {
        let mut at = 0u64;
        b.iter(|| {
            at = at.wrapping_add(1);
            let key = format!("key-{:012}", scattered(at % ENTRIES));
            let mut iter = table.iter();
            iter.seek(&internal_key(key.as_bytes(), u64::MAX >> 8, EntryKind::Put));
            let mut bytes = 0usize;
            for _ in 0..20 {
                if !iter.valid() {
                    break;
                }
                bytes += iter.key().len() + iter.value().len();
                iter.next();
            }
            bytes
        });
    });

    // A backwards walk stays `O(log n)` per step in both stores — a skiplist has no back
    // pointers — so this is here to show that it did not get *worse*.
    group.bench_function("scan_back", |b| {
        b.iter(|| {
            let mut iter = table.iter();
            iter.seek_to_last();
            let mut bytes = 0usize;
            for _ in 0..1000 {
                if !iter.valid() {
                    break;
                }
                bytes += iter.key().len();
                iter.prev();
            }
            bytes
        });
    });

    group.bench_function("get_hit", |b| {
        let mut at = 0u64;
        b.iter(|| {
            at = at.wrapping_add(1);
            let key = format!("key-{:012}", scattered(at % ENTRIES));
            table.get(key.as_bytes(), u64::MAX >> 8)
        });
    });

    group.bench_function("get_miss", |b| {
        let mut at = 0u64;
        b.iter(|| {
            at = at.wrapping_add(1);
            let key = format!("absent-{at:012}");
            table.get(key.as_bytes(), u64::MAX >> 8)
        });
    });

    // The seek a point read really does, without the `Lookup` allocation on top of it.
    group.bench_function("seek_only", |b| {
        let mut at = 0u64;
        b.iter(|| {
            at = at.wrapping_add(1);
            let key = format!("key-{:012}", scattered(at % ENTRIES));
            let mut iter = table.iter();
            iter.seek(&lookup_key(key.as_bytes(), u64::MAX >> 8));
            iter.valid()
        });
    });

    group.finish();
}

// `criterion_group!` expands to an undocumented public function, and the workspace warns on
// those. The macro is the crate's documented entry point; there is nothing to say about the
// item it generates.
#[allow(missing_docs, reason = "the item is generated by criterion_group!")]
mod harness {
    use criterion::criterion_group;

    criterion_group!(benches, super::fill, super::read);
}

use harness::benches;
criterion_main!(benches);
