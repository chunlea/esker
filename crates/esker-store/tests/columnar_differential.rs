//! The columnar apply target against a reference that resolves versions the obvious way.
//!
//! ADR 0022 names the two engines disagreeing as the worst failure this feature can have,
//! *"because it is silent"*. `esker-columnar`'s own differential defends the evaluator; this
//! defends the layer above it — **the apply target, the sort it produces, the merge, and MVCC
//! visibility over all three** — which is where a wrong answer would come from now.
//!
//! # What is actually independent
//!
//! The reference never opens a run. It resolves versions from the **workload the region was fed**,
//! with its own rule written out longhand: group by key, take the newest version at or below the
//! read, drop it if it is a tombstone. So a disagreement can come from anywhere in the path — the
//! decoder, the seal, the sort, the merge, the stripe boundaries, the resolver, the scan.
//!
//! That the two share the *rule* is the point; sharing a rule is not sharing an implementation. A
//! reference that called [`esker_columnar::scan::visible::Resolver`] would agree with itself and
//! prove nothing, which is the trap `docs/plans/phase-8-learner.md` names under RULED-2.
//!
//! # What this cannot yet cover, stated rather than left to be noticed
//!
//! The corpus does not interleave DDL, because the row codec that would make an `ADD COLUMN` with
//! a non-`NULL` default meaningful is still moving into `esker-keys` (RULED-1). That case — rows
//! written **before** an `ADD COLUMN ... DEFAULT 42` reading `42` row-side and `NULL` here — is the
//! one this harness would otherwise be built blind to, so it is named in the plan's test list and
//! is the first thing to add when the codec lands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use esker_base::rng::Pcg32;
use esker_columnar::scan::visible::Visibility;
use esker_columnar::{
    ColumnDef, ColumnType, Fragment, FragmentOutput, Reader, ScanOptions, Schema, TableRef, Value,
    evaluate_merged,
};
use esker_engine::fs::{FileSystem, LocalFileSystem};
use esker_store::columnar::{ColumnarApply, ColumnarOptions, RowDecoder, compact};
use esker_store::error::Result;

/// One write the region applied: a version of a key, or a tombstone for it.
#[derive(Debug, Clone)]
struct Version {
    key: i64,
    /// `None` is a delete.
    name: Option<String>,
    commit_ts: u64,
}

/// `id:int8, name:text`, keyed on `id`. The apply target adds `__commit_ts` and `__deleted`.
#[derive(Debug)]
struct Decoder {
    schema: Schema,
}

impl Decoder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            schema: Schema::new(vec![
                ColumnDef::new("id", ColumnType::Int8),
                ColumnDef::new("name", ColumnType::Text),
            ])
            .unwrap(),
        })
    }
}

impl RowDecoder for Decoder {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    /// A fixed two-column table: no column postdates a row, so nothing is padded.
    fn missing(&self) -> Vec<Value> {
        vec![Value::Null; self.schema.len()]
    }

    fn decode(&self, key: &[u8], value: Option<&[u8]>) -> Result<Vec<Value>> {
        let mut id = [0u8; 8];
        id.copy_from_slice(&key[..8]);
        Ok(match value {
            // A tombstone's data columns are NULL; its identity is the `__key` column the apply
            // target adds from the key's own bytes.
            None => vec![Value::Null, Value::Null],
            Some(bytes) => vec![
                Value::Int8(i64::from_be_bytes(id)),
                Value::Text(String::from_utf8_lossy(bytes).into_owned()),
            ],
        })
    }
}

fn versioned_key(id: i64, ts: u64) -> Vec<u8> {
    let mut key = id.to_be_bytes().to_vec();
    key.extend_from_slice(&esker_keys::prefix::txn_key(&[], ts)[1..]);
    key
}

/// **The reference.** Written longhand, from the workload, never from a file.
///
/// For each key: the versions at or below `at`, newest first, take the first, and report it only
/// if it is not a tombstone. That is the rule in one sentence and this is it in one loop.
fn reference(workload: &[Version], at: u64) -> BTreeMap<i64, String> {
    let mut by_key: BTreeMap<i64, Vec<&Version>> = BTreeMap::new();
    for version in workload {
        by_key.entry(version.key).or_default().push(version);
    }
    let mut visible = BTreeMap::new();
    for (key, mut versions) in by_key {
        versions.retain(|version| version.commit_ts <= at);
        versions.sort_by_key(|version| std::cmp::Reverse(version.commit_ts));
        if let Some(newest) = versions.first()
            && let Some(name) = &newest.name
        {
            visible.insert(key, name.clone());
        }
    }
    visible
}

/// What the columnar copy says, read at `at` across every live run.
fn columnar(apply: &ColumnarApply, at: u64) -> BTreeMap<i64, String> {
    let fs = LocalFileSystem::new();
    let runs = apply.runs();
    // **Every live run at once.** Resolving per run answers "the newest version in this run",
    // which is a different question — and the one this harness caught the first time it ran.
    let readers: Vec<Reader> = runs
        .live()
        .iter()
        .map(|number| Reader::open(&fs, &runs.path_of(*number)).unwrap())
        .collect();
    if readers.is_empty() {
        return BTreeMap::new();
    }
    let fragment = Fragment::scan(
        TableRef {
            tenant: 1,
            table_id: 1,
        },
        vec![0, 1],
    );
    let result = evaluate_merged(
        &readers,
        &fragment,
        &ScanOptions {
            prune: true,
            widening: None,
            visibility: Some(Visibility {
                key_columns: vec![2],
                ts_column: 3,
                deleted_column: 4,
                ts: i64::try_from(at).unwrap(),
            }),
        },
    )
    .unwrap();
    let FragmentOutput::Rows(rows) = result.output else {
        panic!("a scan fragment returned groups");
    };
    let mut seen = BTreeMap::new();
    for row in rows {
        match (&row[0], &row[1]) {
            (Value::Int8(id), Value::Text(name)) => {
                seen.insert(*id, name.clone());
            }
            other => panic!("unexpected row: {other:?}"),
        }
    }
    seen
}

/// Mixed puts, rewrites and deletes over a small key space, so keys collect several versions and
/// tombstones land in the middle of them rather than only at the end.
fn workload(count: usize, keys: i64, seed: u64) -> Vec<Version> {
    let mut rng = Pcg32::from_seed(seed);
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let key = i64::from(rng.below(u32::try_from(keys).unwrap()));
        let commit_ts = (index as u64 + 1) * 10;
        // One write in four is a delete, so a key is repeatedly killed and rewritten.
        let name = if rng.below(4) == 0 {
            None
        } else {
            Some(format!("k{key}@{commit_ts}"))
        };
        out.push(Version {
            key,
            name,
            commit_ts,
        });
    }
    out
}

/// Applies `workload`, sealing every `seal_rows` so the corpus spans several runs.
///
/// The apply target owns its own [`RunSet`], so the numbers a seal takes and the numbers the
/// manifest names come from one sequence. The read below goes through that manifest rather than a
/// directory listing, which is the property `runs.rs` exists to provide.
fn apply_all(dir: &std::path::Path, workload: &[Version], seal_rows: usize) -> ColumnarApply {
    let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new());
    let mut apply = ColumnarApply::open(
        fs,
        dir,
        Decoder::new(),
        ColumnarOptions {
            seal_rows,
            ..ColumnarOptions::default()
        },
    )
    .unwrap();
    for version in workload {
        apply
            .apply(
                &versioned_key(version.key, version.commit_ts),
                version.name.as_ref().map(String::as_bytes),
            )
            .unwrap();
    }
    apply.seal().unwrap();
    apply
}

/// The whole claim, over a generated corpus at every interesting instant.
#[test]
fn the_columnar_copy_agrees_with_the_reference_at_every_timestamp() {
    for seed in 0..8u64 {
        let dir = tempfile::tempdir().unwrap();
        let load = workload(200, 12, 900 + seed);
        let apply = apply_all(dir.path(), &load, 37);

        // Before anything, between every pair of writes, and after everything.
        let mut instants = vec![0u64, u64::from(u32::MAX)];
        instants.extend(load.iter().map(|version| version.commit_ts));
        instants.extend(load.iter().map(|version| version.commit_ts + 5));
        for at in instants {
            assert_eq!(
                columnar(&apply, at),
                reference(&load, at),
                "seed {seed} disagreed at ts {at}"
            );
        }
    }
}

/// And it still agrees after a merge, which is the operation most able to lose or duplicate a
/// version while looking like it worked.
#[test]
fn a_merge_changes_no_answer() {
    let dir = tempfile::tempdir().unwrap();
    let load = workload(300, 9, 4242);
    let mut apply = apply_all(dir.path(), &load, 29);
    assert!(
        apply.runs().live().len() >= 3,
        "the corpus did not span runs"
    );

    let before: Vec<BTreeMap<i64, String>> = load
        .iter()
        .map(|version| columnar(&apply, version.commit_ts))
        .collect();

    let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new());
    let schema = apply.schema().clone();
    let inputs = apply.runs().live().to_vec();
    compact::merge(
        &fs,
        apply.runs_mut(),
        &schema,
        &[2],
        3,
        &inputs,
        &esker_columnar::WriterOptions::default(),
    )
    .unwrap()
    .expect("the merge produced a run");
    assert_eq!(
        apply.runs().live().len(),
        1,
        "the merge did not collapse the runs"
    );

    for (index, version) in load.iter().enumerate() {
        assert_eq!(
            columnar(&apply, version.commit_ts),
            before[index],
            "the merge changed the answer at ts {}",
            version.commit_ts
        );
        assert_eq!(
            columnar(&apply, version.commit_ts),
            reference(&load, version.commit_ts)
        );
    }
}

/// **The regression this harness was built to catch, named for it.**
///
/// A tombstone in a *later* run must delete a row from an *earlier* one. The first version of the
/// fragment path resolved visibility per run, which answers "the newest version of this key **in
/// this run**" — a different question. A run whose only word on a key is a tombstone resolves it
/// to *nothing*, so nothing removed the older, live-looking row an earlier run had every right to
/// return. The result was a deleted row served as live.
///
/// Asserted under **both** output shapes on purpose. `Rows` is where it was found; `Aggregates` is
/// where it could never have been repaired after the fact, because combining per-run partials
/// cannot express "this run's candidate was overruled by another run's" — a `count(*)` would have
/// counted the dead row for ever with no way to notice.
#[test]
fn a_tombstone_in_a_later_run_deletes_a_row_from_an_earlier_one() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new());
    let mut apply = ColumnarApply::open(
        Arc::clone(&fs),
        dir.path(),
        Decoder::new(),
        ColumnarOptions {
            // One row per run, so the write and its tombstone cannot share a file.
            seal_rows: 1,
            ..ColumnarOptions::default()
        },
    )
    .unwrap();

    apply.apply(&versioned_key(1, 10), Some(b"alive")).unwrap();
    apply.apply(&versioned_key(2, 10), Some(b"other")).unwrap();
    apply.apply(&versioned_key(1, 20), None).unwrap();
    apply.seal().unwrap();
    assert!(
        apply.runs().live().len() >= 3,
        "the versions shared a run, so this test proves nothing"
    );

    // Rows: key 1 is gone after its delete, and was there before it.
    assert_eq!(
        columnar(&apply, 15),
        BTreeMap::from([(1, "alive".to_string()), (2, "other".to_string())]),
        "a read before the delete lost the row"
    );
    assert_eq!(
        columnar(&apply, 25),
        BTreeMap::from([(2, "other".to_string())]),
        "a tombstone in a later run did not delete the row in the earlier one"
    );

    // Aggregates: the same claim where no after-the-fact repair could have reached.
    assert_eq!(count_at(&apply, 15), 2);
    assert_eq!(
        count_at(&apply, 25),
        1,
        "count(*) counted a row whose tombstone was in another run"
    );
}

/// `count(*)` over the visible rows at `at`.
fn count_at(apply: &ColumnarApply, at: u64) -> u64 {
    let fs = LocalFileSystem::new();
    let runs = apply.runs();
    let readers: Vec<Reader> = runs
        .live()
        .iter()
        .map(|number| Reader::open(&fs, &runs.path_of(*number)).unwrap())
        .collect();
    let fragment = Fragment::aggregate(
        TableRef {
            tenant: 1,
            table_id: 1,
        },
        vec![0, 1],
        Vec::new(),
        vec![esker_columnar::Aggregate::CountStar],
    );
    let result = evaluate_merged(
        &readers,
        &fragment,
        &ScanOptions {
            prune: true,
            widening: None,
            visibility: Some(Visibility {
                key_columns: vec![2],
                ts_column: 3,
                deleted_column: 4,
                ts: i64::try_from(at).unwrap(),
            }),
        },
    )
    .unwrap();
    match result.output {
        FragmentOutput::Groups(groups) => match groups.first().map(|group| &group.aggregates[0]) {
            Some(esker_columnar::Partial::Count(count)) => *count,
            other => panic!("not a count: {other:?}"),
        },
        FragmentOutput::Rows(rows) => panic!("not groups: {rows:?}"),
    }
}

/// A decoder of `id, name` plus `count` columns of `int8`, the last padding `missing`.
#[derive(Debug)]
struct WideningDecoder {
    schema: Schema,
    row: esker_keys::row::RowSchema,
}

impl WideningDecoder {
    /// `version` 1 is `(id, name)`; version 2 adds `c int8 NOT NULL DEFAULT 42`.
    fn at(version: u64) -> Arc<Self> {
        let mut columns = vec![
            ColumnDef::new("id", ColumnType::Int8),
            ColumnDef::new("name", ColumnType::Text),
        ];
        let mut types = vec![
            esker_keys::value::ColumnType::Int8,
            esker_keys::value::ColumnType::Text,
        ];
        let mut missing: Vec<Option<esker_keys::value::Datum>> = vec![None, None];
        if version >= 2 {
            columns.push(ColumnDef::new("c", ColumnType::Int8));
            types.push(esker_keys::value::ColumnType::Int8);
            missing.push(Some(esker_keys::value::Datum::Int8(42)));
        }
        Arc::new(Self {
            schema: Schema::new(columns).unwrap(),
            row: esker_keys::row::RowSchema::new(types, missing),
        })
    }
}

impl RowDecoder for WideningDecoder {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    /// `c`'s `DEFAULT 42`, which is what a row written at version 1 must read for it.
    fn missing(&self) -> Vec<Value> {
        let mut missing = vec![Value::Null, Value::Null];
        if self.schema.len() > 2 {
            missing.push(Value::Int8(42));
        }
        missing
    }

    fn decode(&self, _key: &[u8], value: Option<&[u8]>) -> Result<Vec<Value>> {
        let Some(bytes) = value else {
            return Ok(vec![Value::Null; self.schema.len()]);
        };
        let row = esker_keys::row::decode_row(&self.row, bytes)
            .map_err(|error| esker_store::error::StoreError::Bootstrap(error.to_string()))?;
        Ok(row
            .iter()
            .map(|datum| match datum {
                esker_keys::value::Datum::Null => Value::Null,
                esker_keys::value::Datum::Int8(int) => Value::Int8(*int),
                esker_keys::value::Datum::Text(text) => Value::Text(text.clone()),
                other => panic!("this table has no {other:?} columns"),
            })
            .collect())
    }
}

/// Encodes a row of the given width, as the SQL layer would have written it at that version.
fn row_at(version: u64, id: i64, name: &str) -> Vec<u8> {
    let mut types = vec![
        esker_keys::value::ColumnType::Int8,
        esker_keys::value::ColumnType::Text,
    ];
    let mut values = vec![
        esker_keys::value::Datum::Int8(id),
        esker_keys::value::Datum::Text(name.to_string()),
    ];
    if version >= 2 {
        types.push(esker_keys::value::ColumnType::Int8);
        values.push(esker_keys::value::Datum::Int8(id * 100));
    }
    esker_keys::row::encode_row(&types, &values).unwrap()
}

/// **A schema widening in the middle of a workload, through the whole path.**
///
/// Rows written under version 1 are two columns wide; version 2 adds `c int8 NOT NULL DEFAULT 42`.
/// A row predating the `ALTER` must read **42**, because that is what the row store reads for it
/// (PostgreSQL 11's `attmissingval`, ADR 0019). The decoder's own regression proves it decodes;
/// this proves it survives the *seal boundary*, the run-width difference and the merged read —
/// which is where a columnar copy could still answer NULL while the decoder was perfectly correct.
#[test]
fn a_column_added_mid_workload_reads_its_default_for_the_rows_that_predate_it() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(LocalFileSystem::new());
    let mut apply = ColumnarApply::open(
        Arc::clone(&fs),
        dir.path(),
        WideningDecoder::at(1),
        ColumnarOptions::default(),
    )
    .unwrap();

    // Version 1: two-column rows.
    for id in 0..4i64 {
        apply
            .apply(&versioned_key(id, 10), Some(&row_at(1, id, "old")))
            .unwrap();
    }

    // The ALTER lands: seal the v1 run, widen, keep applying.
    apply.install_schema(WideningDecoder::at(2), 2).unwrap();
    assert_eq!(apply.runs().live().len(), 1, "the widening did not seal");

    for id in 4..8i64 {
        apply
            .apply(&versioned_key(id, 20), Some(&row_at(2, id, "new")))
            .unwrap();
    }
    apply.seal().unwrap();
    assert_eq!(apply.runs().live().len(), 2, "two widths, two runs");

    // Read every row at a timestamp after both.
    let fs = LocalFileSystem::new();
    let runs = apply.runs();
    let readers: Vec<Reader> = runs
        .live()
        .iter()
        .map(|number| Reader::open(&fs, &runs.path_of(*number)).unwrap())
        .collect();
    let (key_slot, ts_slot, deleted_slot) = apply.visibility_slots();
    let fragment = Fragment::scan(
        TableRef {
            tenant: 1,
            table_id: 1,
        },
        vec![0, 2],
    );
    // The schema every run is read as having, and what the older, narrower one pads with. `42`
    // for `c` is the column's `missing` value — the whole point of the test.
    //
    // **Taken from the apply target, not written out here.** It was a literal until the joint
    // gate found that the production read path passed `widening: None` and so never used any of
    // this: a test that builds the right answer by hand proves the mechanism and says nothing
    // about the caller. `ColumnarApply::missing` is what `Store::serve_fragment` passes, so this
    // now exercises the same value the cluster does.
    let widening = esker_columnar::Widening {
        schema: apply.schema().clone(),
        missing: apply.missing(),
    };
    assert_eq!(
        widening.missing,
        vec![
            Value::Null,
            Value::Null,
            Value::Int8(42),
            Value::Null,
            Value::Null,
            Value::Null,
        ],
        "the apply target's missing values are not the catalog's",
    );
    let result = evaluate_merged(
        &readers,
        &fragment,
        &ScanOptions {
            prune: true,
            widening: Some(widening),
            visibility: Some(Visibility {
                key_columns: vec![key_slot],
                ts_column: ts_slot,
                deleted_column: deleted_slot,
                ts: 30,
            }),
        },
    )
    .unwrap();
    let FragmentOutput::Rows(rows) = result.output else {
        panic!("not rows");
    };

    let mut seen: BTreeMap<i64, Value> = BTreeMap::new();
    for row in rows {
        let Value::Int8(id) = row[0] else {
            panic!("not an id")
        };
        seen.insert(id, row[1].clone());
    }
    for id in 0..4i64 {
        assert_eq!(
            seen.get(&id),
            Some(&Value::Int8(42)),
            "row {id} predates the ALTER and must read the column's default"
        );
    }
    for id in 4..8i64 {
        assert_eq!(seen.get(&id), Some(&Value::Int8(id * 100)));
    }
}
