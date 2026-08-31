//! A deterministic, realistic batch of rows, shared by the golden and compression tests.
//!
//! Included with `#[path]` rather than published as a module, because it is test scaffolding
//! rather than crate surface. Everything here is a pure function of a seed, so a golden file
//! rebuilt on another machine, in another year, is byte for byte the same file.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use esker_base::rng::Pcg32;
use esker_columnar::{ColumnDef, ColumnType, Schema, Value};

/// Seed of the generator behind every golden file. Changing it invalidates them.
pub(crate) const SEED: u64 = 0xC01D_C011;

/// The kinds a `kind` column draws from: few and repeated, so a dictionary is chosen.
const KINDS: [&str; 5] = ["deposit", "withdrawal", "transfer", "fee", "adjustment"];

/// An event table with one column of every type this format stores.
pub(crate) fn schema() -> Schema {
    Schema::new(vec![
        ColumnDef::new("id", ColumnType::Int8),
        ColumnDef::new("at", ColumnType::TimestampTz),
        ColumnDef::new("kind", ColumnType::Text),
        ColumnDef::new("settled", ColumnType::Bool),
        ColumnDef::new("amount", ColumnType::Double),
        ColumnDef::new("payload", ColumnType::Bytea),
    ])
    .unwrap()
}

/// `count` rows of it, shaped like a ledger: ascending ids and timestamps, a low-cardinality
/// label, a boolean that runs, amounts in a narrow band, and a payload that is sometimes absent.
pub(crate) fn rows(count: usize) -> Vec<Vec<Value>> {
    let mut rng = Pcg32::from_seed(SEED);
    let mut at = 757_382_400_000_000i64;
    let mut settled = false;
    let mut run = 0u32;

    (0..count)
        .map(|index| {
            at += i64::from(rng.below(4_000)) + 100;
            if run == 0 {
                settled = rng.chance(0.7);
                run = rng.below(40) + 1;
            }
            run -= 1;

            let kind = KINDS[rng.below(KINDS.len() as u32) as usize];
            let amount = f64::from(rng.below(1_000_000)) / 100.0;
            let payload_len = if rng.chance(0.15) {
                0
            } else {
                rng.below(24) as usize
            };
            let mut payload = vec![0u8; payload_len];
            rng.fill_bytes(&mut payload);

            vec![
                Value::Int8(index as i64),
                Value::TimestampTz(at),
                if rng.chance(0.05) {
                    Value::Null
                } else {
                    Value::Text(kind.to_owned())
                },
                Value::Bool(settled),
                if rng.chance(0.02) {
                    Value::Null
                } else {
                    Value::Double(amount)
                },
                if payload_len == 0 {
                    Value::Null
                } else {
                    Value::Bytea(payload)
                },
            ]
        })
        .collect()
}
