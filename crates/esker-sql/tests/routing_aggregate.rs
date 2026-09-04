//! The finishing aggregate: what happens when many regions each answer part of one question.
//!
//! `docs/plans/phase-10-routing.md` U4. This is where a routed query can differ from a row one
//! *silently* — a `min` over an empty partial, a NULL group merged with a real one, an `avg`
//! finished from averages — so the test is a property rather than a table of cases:
//!
//! > **Any split of the rows into fragments folds to the same answer as one fragment**, and to the
//! > same answer the row engine gives.
//!
//! # The source here is a second evaluator, not a script
//!
//! `tests/routing.rs` answers with canned partials, which is right for testing *routing*. It would
//! be worthless here: a fold checked against numbers the test wrote down is a test of the test.
//! So [`Fragments`] holds the rows, **decodes the fragment the planner actually built**, and
//! evaluates it — grouping, folding in row order, PostgreSQL's empty-input rules — sharing nothing
//! with `crate::exec::fragment` but `pg_cmp`, which is the specification. That is the shape
//! `esker-columnar`'s own differential takes and for the same reason (`docs/DESIGN.md` §16.3).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use esker_client::wire::{Epoch, Peer, PeerRole};
use esker_columnar::fragment::{Aggregate as ColAggregate, Output as ColOutput};
use esker_proto::fragment::ScanStats;
use esker_proto::fragment::result::{
    AggregateKind, Body, Group, Partial, Value as WireValue, ValueType,
};
use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::fragment::{Answer, FragmentSource, Shard};
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};
use esker_sql::value::{Datum, PgDatum};
use proptest::prelude::*;

/// The query every case in this file asks, over every aggregate a fragment can carry plus the one
/// it cannot.
///
/// `avg` is there deliberately: it is the only aggregate with no partial that combines, so it goes
/// down as a `sum` and a `count` and is divided here. A test that left it out would miss the one
/// finishing rule that is arithmetic rather than folding.
const QUERY: &str = "SELECT g, count(*), count(v), sum(v), min(v), max(v), avg(d) \
                     FROM t GROUP BY g ORDER BY g";

/// One row of the fixture table.
#[derive(Debug, Clone)]
struct Row {
    id: i64,
    /// The grouping key, or NULL — which is **one group of its own**, not a missing row.
    g: Option<String>,
    /// The value the four foldable aggregates read, or NULL — which `count(v)` skips and
    /// `count(*)` does not.
    v: Option<i64>,
    /// `avg`'s column. Whole numbers, so the addition is exact and a difference in the last bits
    /// would be the defect rather than the arithmetic (see the module note on associativity).
    d: Option<f64>,
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// **The property.** Rows split any way at all fold to the answer one fragment gives, and to
    /// the answer the row engine gives at the same snapshot.
    ///
    /// The second half is the one that catches a wrong fold; the first is the one that catches a
    /// fold that is wrong only when it is distributed, which is the failure this whole milestone
    /// can have silently.
    #[test]
    fn any_split_into_fragments_folds_to_the_same_answer(
        rows in rows(), splits in prop::collection::vec(0usize..12, 0..4)
    ) {
        let one = routed(&rows, &[]);
        let many = routed(&rows, &splits);
        let by_rows = on_rows(&rows);

        prop_assert_eq!(&many, &one, "a split changed the answer\nrows: {:?}", rows);
        prop_assert_eq!(&many, &by_rows, "the fragments disagree with the row engine\nrows: {:?}", rows);
    }
}

/// **Every** split of a fixed row set, enumerated rather than sampled.
///
/// The proptest above samples splits; this one is exhaustive over the interesting space — six rows
/// have thirty-two ways to be cut into regions, and all thirty-two must fold to the same answer.
/// A property that only ever generated one region would pass while proving nothing, and a pass
/// rate whose denominator nobody looked at is the thing `docs/plans/phase-9-rails.md` §Unit 6 is
/// about; here the denominator is written down and is thirty-two.
#[test]
fn every_split_of_six_rows_folds_the_same() {
    let rows: Vec<Row> = (0..6)
        .map(|at| Row {
            id: at,
            g: (at % 3 != 2).then(|| if at % 2 == 0 { "a" } else { "b" }.to_owned()),
            v: (at % 4 != 3).then_some(at * 3 - 4),
            d: (at % 5 != 4).then_some(f64::from(i32::try_from(at).unwrap_or(0))),
        })
        .collect();
    let expected = on_rows(&rows);
    let mut splits_tried = 0;
    // Every subset of the five interior cut points.
    for mask in 0u32..32 {
        let splits: Vec<usize> = (1..6).filter(|at| mask >> (at - 1) & 1 == 1).collect();
        splits_tried += 1;
        assert_eq!(
            routed(&rows, &splits),
            expected,
            "split {splits:?} changed the answer"
        );
    }
    assert_eq!(splits_tried, 32, "the enumeration did not run");
}

/// A table of no rows: an **ungrouped** aggregate is one row and a **grouped** one is none.
///
/// Two rules, not one, and a fragment that matched nothing returns no groups either way — so the
/// distinction cannot come from the answer and has to be made where the answer is finished.
#[test]
fn the_empty_input_rules_are_two_rules() {
    let mut node = ready(&[]);
    assert_eq!(
        answer(&mut node, "SELECT count(*), sum(v), min(v), max(v) FROM t"),
        vec![vec![
            "0".to_owned(),
            "NULL".to_owned(),
            "NULL".to_owned(),
            "NULL".to_owned()
        ]],
        "an ungrouped aggregate over no rows is one row: count 0, everything else NULL"
    );
    assert!(
        answer(&mut node, "SELECT g, count(*) FROM t GROUP BY g").is_empty(),
        "a grouped aggregate over no rows is no rows"
    );
}

/// A NULL grouping key is one group, and it is a group the fragments must merge with each other's.
///
/// `pg_cmp` equality, which is what `GROUP BY` uses everywhere else in this crate. A key compared
/// bitwise would split a group the row engine keeps together, which is exactly the shape of
/// disagreement this milestone can produce and not notice.
#[test]
fn a_null_key_is_one_group_across_regions() {
    let rows = vec![
        Row {
            id: 1,
            g: None,
            v: Some(1),
            d: Some(1.0),
        },
        Row {
            id: 2,
            g: Some("a".to_owned()),
            v: Some(2),
            d: Some(2.0),
        },
        Row {
            id: 3,
            g: None,
            v: Some(3),
            d: Some(3.0),
        },
    ];
    // Split so the two NULL-keyed rows land in different regions.
    assert_eq!(routed(&rows, &[2]), on_rows(&rows));
    assert_eq!(routed(&rows, &[2]), routed(&rows, &[]));
}

/// `sum` over a group whose every value is NULL is **NULL, not zero** — through the fold as well
/// as through one fragment.
///
/// The fold is where this is easy to lose: a running sum initialised to zero would turn "no rows
/// contributed" into a number, and a number is indistinguishable from data.
#[test]
fn a_sum_over_only_nulls_stays_null_through_the_fold() {
    let rows = vec![
        Row {
            id: 1,
            g: Some("a".to_owned()),
            v: None,
            d: None,
        },
        Row {
            id: 2,
            g: Some("a".to_owned()),
            v: None,
            d: None,
        },
        Row {
            id: 3,
            g: Some("a".to_owned()),
            v: None,
            d: None,
        },
    ];
    let finished = routed(&rows, &[1, 2]);
    assert_eq!(finished.len(), 1);
    // g, count(*), count(v), sum(v), min(v), max(v), avg(d)
    assert_eq!(
        finished[0],
        vec!["a", "3", "0", "NULL", "NULL", "NULL", "NULL"]
    );
    assert_eq!(finished, on_rows(&rows));
}

/// `count(*)` counts a row whose every column is NULL; `count(v)` does not.
#[test]
fn count_star_and_count_of_a_column_differ_across_the_fold() {
    let rows = vec![
        Row {
            id: 1,
            g: Some("a".to_owned()),
            v: Some(7),
            d: None,
        },
        Row {
            id: 2,
            g: Some("a".to_owned()),
            v: None,
            d: None,
        },
    ];
    let finished = routed(&rows, &[1]);
    assert_eq!(finished[0][1], "2", "count(*) skipped a NULL row");
    assert_eq!(finished[0][2], "1", "count(v) counted a NULL");
    assert_eq!(finished, on_rows(&rows));
}

// ---------------------------------------------------------------------------------------------
// The two ways to answer the query
// ---------------------------------------------------------------------------------------------

/// The answer through the fragments, with the rows split at `splits` (row indexes).
fn routed(rows: &[Row], splits: &[usize]) -> Vec<Vec<String>> {
    let source = Arc::new(Fragments {
        rows: rows.to_vec(),
        splits: splits.to_vec(),
        asked: Mutex::new(0),
    });
    let asked = Arc::clone(&source);
    let mut node = ready_with(rows, Some(source as Arc<dyn FragmentSource>));
    // The ratio does not favour a query reading three of four columns, so the estimate is told to
    // stand aside. The override moves the threshold and nothing else, which is what makes it usable
    // here without weakening the thing under test.
    run(&mut node, "SET esker.engine = 'columnar'").unwrap();
    let out = answer(&mut node, QUERY);

    // **The denominator.** A fragment going out is not enough: one that went out and was refused
    // sends the query back to the rows, and then every assertion below compares the row engine
    // with itself and passes for the wrong reason. `EXPLAIN ANALYZE` re-runs the same query and
    // says how many regions *answered*, which is the number that makes the comparison mean
    // something.
    let ran = analyze(&mut node, QUERY);
    let regions = source_regions(rows, splits);
    assert!(
        ran.contains(&format!("Fragments: {regions} asked, {regions} answered")),
        "the answer did not come from {regions} fragments:\n{ran}"
    );
    assert!(
        *asked.asked.lock().unwrap() > 0,
        "nothing was routed, so this comparison proves nothing"
    );
    out
}

/// How many regions a split makes — the same arithmetic [`Fragments::regions`] does, written out
/// so the assertion above has a number of its own rather than asking the thing under test.
fn source_regions(rows: &[Row], splits: &[usize]) -> usize {
    let mut cuts: Vec<usize> = splits.iter().map(|at| (*at).min(rows.len())).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut bounds = vec![0];
    bounds.extend(cuts);
    bounds.push(rows.len());
    bounds.dedup();
    bounds.len().saturating_sub(1).max(1)
}

/// The same query on the row engine, at the same fixture.
fn on_rows(rows: &[Row]) -> Vec<Vec<String>> {
    let mut node = ready_with(rows, None);
    answer(&mut node, QUERY)
}

// ---------------------------------------------------------------------------------------------
// A second evaluator, sharing only `pg_cmp` with the one under test
// ---------------------------------------------------------------------------------------------

/// A fragment source that holds the rows and answers whatever fragment it is given.
#[derive(Debug)]
struct Fragments {
    rows: Vec<Row>,
    /// Row indexes the regions are cut at, in any order and possibly out of range — a split is a
    /// property input, not a shape the test curates.
    splits: Vec<usize>,
    asked: Mutex<usize>,
}

impl Fragments {
    /// The rows of each region, in region order.
    fn regions(&self) -> Vec<&[Row]> {
        let mut cuts: Vec<usize> = self
            .splits
            .iter()
            .map(|at| (*at).min(self.rows.len()))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut bounds = vec![0];
        bounds.extend(cuts);
        bounds.push(self.rows.len());
        bounds.dedup();
        bounds
            .windows(2)
            .map(|pair| &self.rows[pair[0]..pair[1]])
            .collect()
    }
}

impl FragmentSource for Fragments {
    fn shards(&self, _start: &[u8], _end: &[u8]) -> esker_sql::Result<Vec<Shard>> {
        Ok((0..self.regions().len().max(1))
            .map(|at| Shard {
                region_id: at as u64 + 1,
                epoch: Epoch::INITIAL,
                start: bytes::Bytes::new(),
                end: bytes::Bytes::new(),
                columnar: Some(Peer {
                    store_id: 9,
                    peer_id: 9,
                    role: PeerRole::ColumnarLearner,
                }),
            })
            .collect())
    }

    fn evaluate(
        &self,
        shard: &Shard,
        fragment: &[u8],
        _ts: u64,
        _min_apply_index: u64,
    ) -> esker_sql::Result<Answer> {
        *self.asked.lock().unwrap() += 1;
        let fragment = esker_columnar::fragment::decode(fragment).expect("the planner's fragment");
        let regions = self.regions();
        let mine: &[Row] = regions
            .get(usize::try_from(shard.region_id - 1).unwrap_or(0))
            .copied()
            .unwrap_or(&[]);
        Ok(Answer::Answered {
            result: bytes::Bytes::from(
                esker_proto::fragment::result::encode(&evaluate(&fragment, mine)).unwrap(),
            ),
            stats: ScanStats::default(),
        })
    }
}

/// Evaluates one fragment over one region's rows, in row order.
///
/// PostgreSQL's aggregate semantics, written from `docs/DESIGN.md` §16.2 rather than from the code
/// under test: `count(*)` counts every row and `count(col)` skips NULLs, `sum`/`min`/`max` over
/// nothing are NULL and not zero, extremes order by `pg_cmp`, and a NULL forms one `GROUP BY`
/// group of its own.
fn evaluate(fragment: &esker_columnar::Fragment, rows: &[Row]) -> Body {
    let ColOutput::Aggregates {
        group_by,
        aggregates,
    } = &fragment.output
    else {
        panic!("this file's query is an aggregate");
    };

    // Grouped in first-seen order, then keyed by `pg_cmp` — the ordering is the answer's, and a
    // group's identity is not its bytes.
    let mut groups: BTreeMap<Key, Vec<Partial>> = BTreeMap::new();
    for row in rows {
        let slots = project(fragment, row);
        let key = Key(group_by
            .iter()
            .map(|slot| slots[*slot as usize].clone())
            .collect());
        let running = groups
            .entry(key)
            .or_insert_with(|| aggregates.iter().copied().map(empty).collect());
        for (partial, ask) in running.iter_mut().zip(aggregates) {
            fold(partial, *ask, &slots);
        }
    }

    Body::Groups {
        key_types: group_by
            .iter()
            .map(|slot| declared_type(fragment, *slot))
            .collect(),
        aggregates: aggregates
            .iter()
            .map(|ask| declare(*ask, fragment))
            .collect(),
        groups: groups
            .into_iter()
            .map(|(key, partials)| Group {
                key: key.0,
                partials,
            })
            .collect(),
    }
}

/// A row, projected onto the fragment's slots.
fn project(fragment: &esker_columnar::Fragment, row: &Row) -> Vec<WireValue> {
    fragment
        .projection
        .iter()
        .map(|column| match column {
            0 => WireValue::Int8(row.id),
            1 => row.g.clone().map_or(WireValue::Null, WireValue::Text),
            2 => row.v.map_or(WireValue::Null, WireValue::Int8),
            3 => row.d.map_or(WireValue::Null, WireValue::Double),
            other => panic!("the fixture has no column {other}"),
        })
        .collect()
}

fn empty(ask: ColAggregate) -> Partial {
    match ask {
        ColAggregate::CountStar | ColAggregate::Count(_) => Partial::Count(0),
        ColAggregate::Sum(_) => Partial::Sum(None),
        ColAggregate::Min(_) => Partial::Min(None),
        ColAggregate::Max(_) => Partial::Max(None),
    }
}

/// Folds one row into one running partial.
fn fold(partial: &mut Partial, ask: ColAggregate, slots: &[WireValue]) {
    let value = ask.slot().map(|slot| slots[slot as usize].clone());
    match (partial, ask) {
        (Partial::Count(count), ColAggregate::CountStar) => *count += 1,
        (Partial::Count(count), ColAggregate::Count(_)) => {
            if !matches!(value, Some(WireValue::Null) | None) {
                *count += 1;
            }
        }
        (Partial::Sum(running), ColAggregate::Sum(_)) => {
            *running = add(running.as_ref(), value.as_ref());
        }
        (Partial::Min(running), ColAggregate::Min(_)) => {
            *running = pick(running.as_ref(), value.as_ref(), std::cmp::Ordering::Less);
        }
        (Partial::Max(running), ColAggregate::Max(_)) => {
            *running = pick(
                running.as_ref(),
                value.as_ref(),
                std::cmp::Ordering::Greater,
            );
        }
        (partial, ask) => panic!("{partial:?} cannot fold {ask:?}"),
    }
}

fn add(running: Option<&WireValue>, value: Option<&WireValue>) -> Option<WireValue> {
    match (running, value) {
        (running, None | Some(WireValue::Null)) => running.cloned(),
        (None, Some(value)) => Some(value.clone()),
        (Some(WireValue::Int8(a)), Some(WireValue::Int8(b))) => {
            Some(WireValue::Int8(a.wrapping_add(*b)))
        }
        (Some(WireValue::Double(a)), Some(WireValue::Double(b))) => Some(WireValue::Double(a + b)),
        (running, value) => panic!("cannot add {value:?} to {running:?}"),
    }
}

fn pick(
    running: Option<&WireValue>,
    value: Option<&WireValue>,
    want: std::cmp::Ordering,
) -> Option<WireValue> {
    match (running, value) {
        (running, None | Some(WireValue::Null)) => running.cloned(),
        (None, Some(value)) => Some(value.clone()),
        (Some(running), Some(value)) => {
            let (a, b) = (datum(running), datum(value));
            Some(if b.pg_cmp(&a) == want {
                value.clone()
            } else {
                running.clone()
            })
        }
    }
}

fn declare(
    ask: ColAggregate,
    fragment: &esker_columnar::Fragment,
) -> (AggregateKind, Option<ValueType>) {
    match ask {
        // One arm: a `count`'s partial is a `u64` and not a column value, so neither form
        // declares a type. That is the wire's rule, not a coincidence between two arms.
        ColAggregate::CountStar | ColAggregate::Count(_) => (AggregateKind::Count, None),
        ColAggregate::Sum(slot) => (AggregateKind::Sum, Some(declared_type(fragment, slot))),
        ColAggregate::Min(slot) => (AggregateKind::Min, Some(declared_type(fragment, slot))),
        ColAggregate::Max(slot) => (AggregateKind::Max, Some(declared_type(fragment, slot))),
    }
}

/// The type of a slot, from the fixture's own schema.
fn declared_type(fragment: &esker_columnar::Fragment, slot: u32) -> ValueType {
    match fragment.projection[slot as usize] {
        0 | 2 => ValueType::Int8,
        1 => ValueType::Text,
        3 => ValueType::Double,
        other => panic!("the fixture has no column {other}"),
    }
}

fn datum(value: &WireValue) -> Datum {
    match value {
        WireValue::Null => Datum::Null,
        WireValue::Int8(int) => Datum::Int8(*int),
        WireValue::Text(text) => Datum::Text(text.clone()),
        WireValue::Double(double) => Datum::Double(*double),
        other => panic!("the fixture has no {other:?}"),
    }
}

/// A group key ordered by `pg_cmp`, which is what makes a NULL one group and `-0.0` and `0.0` one
/// value — the same rule the code under test uses, and the *only* thing the two share.
/// `Eq` by `pg_cmp` and not by the wire value's own `PartialEq`, which is bitwise for a float and
/// therefore says two `NaN`s are different groups. A `BTreeMap` needs the two to agree.
#[derive(Debug, Clone)]
struct Key(Vec<WireValue>);

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for Key {}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        for (left, right) in self.0.iter().zip(&other.0) {
            match datum(left).pg_cmp(&datum(right)) {
                std::cmp::Ordering::Equal => {}
                other => return other,
            }
        }
        self.0.len().cmp(&other.0.len())
    }
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

fn rows() -> impl Strategy<Value = Vec<Row>> {
    prop::collection::vec(
        (
            prop::option::of("[a-c]"),
            prop::option::of(-20i64..20),
            prop::option::of(-20i64..20),
        ),
        0..12,
    )
    .prop_map(|rows| {
        rows.into_iter()
            .enumerate()
            .map(|(at, (g, v, d))| Row {
                id: i64::try_from(at).unwrap_or(0),
                g,
                v,
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "whole numbers in a small range, exactly representable — the point \
                              is that a difference in the last bits would be the defect and not \
                              the arithmetic"
                )]
                d: d.map(|value| value as f64),
            })
            .collect()
    })
}

fn ready(rows: &[Row]) -> Executor {
    ready_with(rows, None)
}

/// The fixture table, filled, with a fragment source or without one.
fn ready_with(rows: &[Row], source: Option<Arc<dyn FragmentSource>>) -> Executor {
    let mut node = Executor::new(
        Arc::new(MemoryBackend::new()) as Arc<dyn Backend>,
        Arc::new(Catalog::new()),
        1,
    );
    if let Some(source) = source {
        node = node.asking_fragments_of(source);
    }
    run(
        &mut node,
        "CREATE TABLE t (id int8 PRIMARY KEY, g text, v int8, d double precision)",
    )
    .unwrap();
    run(&mut node, "ALTER TABLE t SET (columnar_replicas = 1)").unwrap();
    for row in rows {
        run(
            &mut node,
            &format!(
                "INSERT INTO t VALUES ({}, {}, {}, {})",
                row.id,
                row.g
                    .as_ref()
                    .map_or_else(|| "NULL".to_owned(), |g| format!("'{g}'")),
                row.v.map_or_else(|| "NULL".to_owned(), |v| v.to_string()),
                row.d
                    .map_or_else(|| "NULL".to_owned(), |d| format!("{d:?}")),
            ),
        )
        .unwrap();
    }
    node
}

fn run(node: &mut Executor, sql: &str) -> esker_sql::Result<()> {
    for parsed in parse_statements(sql)? {
        match parsed.class() {
            StatementClass::Begin => {
                node.begin(parsed.begins_read_only())?;
                if let Some(level) = parsed.begins_isolation() {
                    node.set_isolation(level)?;
                }
            }
            StatementClass::Commit => node.commit()?,
            StatementClass::Rollback => node.rollback()?,
            _ => {
                let _: Outcome = node.execute(&parsed, &Params::NONE)?;
            }
        }
    }
    Ok(())
}

/// The `EXPLAIN ANALYZE` of a query, as one string. Runs it.
fn analyze(node: &mut Executor, sql: &str) -> String {
    answer(node, &format!("EXPLAIN ANALYZE {sql}"))
        .into_iter()
        .map(|row| row.join(""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn answer(node: &mut Executor, sql: &str) -> Vec<Vec<String>> {
    let parsed = parse_statements(sql).unwrap().pop().expect("one statement");
    match node.execute(&parsed, &Params::NONE).unwrap() {
        Outcome::Rows { rows, .. } => rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| {
                        cell.map_or_else(
                            || "NULL".to_owned(),
                            |bytes| String::from_utf8_lossy(&bytes).into_owned(),
                        )
                    })
                    .collect()
            })
            .collect(),
        other @ Outcome::Done { .. } => panic!("`{sql}` answered {other:?}"),
    }
}
