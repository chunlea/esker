//! The schema, the rows and the four queries.
//!
//! # Why this shape
//!
//! The routing rule is a **ratio** — a query is planned on columns when it projects at most half
//! the table's columns ([ADR 0040](../../../../docs/adr/0040-the-engine-a-query-runs-on.md),
//! `esker_sql::plan::routing::RATIO`) — so the fact table has seven columns and every measured
//! query reads two of them. That is not a trick to reach the columnar path: it is the shape ADR
//! 0022 exists for, and a table narrow enough to fail the ratio would have nothing to measure.
//!
//! The grouping columns are `id % k`, not random. Two properties follow and both matter:
//!
//! * the cardinality is **exactly** `k`, so a report can say "100,000 groups" rather than
//!   "about that";
//! * every region's rows cover **every** residue, so each region ships nearly the whole group set
//!   and the SQL node merges `regions × groups` partials down to `groups`. That is the worst case
//!   for a two-level aggregate and precisely the case MPP exchange exists to remove — a grouping
//!   key correlated with the primary key would have each region ship a disjoint slice, which the
//!   SQL node concatenates for free and which would make this measurement say the opposite thing.
//!
//! `amount` is seeded pseudo-random (`esker_base::Pcg32`, the one generator `CLAUDE.md` allows) so
//! the sums are not answerable from the statistics, and `payload` is never projected — it is the
//! width that makes the ratio real and the bytes a columnar scan gets to skip.

use std::fmt::Write as _;

use esker_base::rng::Pcg32;

use super::pg::Pg;

/// The fact table's name.
pub(crate) const FACT: &str = "ledger";

/// The dimension table's name.
pub(crate) const DIM: &str = "dim";

/// How the data is generated and how much of it there is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    /// Rows in [`FACT`].
    pub(crate) rows: u64,
    /// Distinct values of `ghigh`, and rows in [`DIM`].
    pub(crate) groups_high: u64,
    /// Rows per `INSERT`.
    pub(crate) batch: u64,
    /// The seed for `amount`.
    pub(crate) seed: u64,
}

impl Shape {
    /// How many rows [`DIM`] holds: one per high-cardinality group, capped at [`DIM_ROWS`].
    pub(crate) fn dim_rows(self) -> u64 {
        self.groups_high.min(DIM_ROWS)
    }
}

/// Distinct values of `g32` — the low-cardinality key, small enough that the finish is free.
pub(crate) const GROUPS_LOW: u64 = 32;

/// Distinct values of `day`, which the control filters on.
const DAYS: u64 = 365;

/// The most rows [`DIM`] holds.
///
/// A join materialises its inner side and refuses past `esker_sql::exec::cursor::SORT_LIMIT`
/// (1,000,000 rows), so an unbounded dimension would turn the join query from a measurement into
/// an error at whatever `--groups-high` first crossed the line. Well below it, because the point
/// of the join here is the *outer* side's size.
const DIM_ROWS: u64 = 200_000;

/// How many buckets [`DIM`] rows are spread over, and which one the join selects.
///
/// **Named rather than written twice.** The loader assigns `k % DIM_BUCKETS` and the join filters
/// on one of them; while those were the literals `8` and `3` in separate places, nothing connected
/// the data to the query and nothing could compute how many keys the join's inner side has.
/// [`join_inner_keys`] can, which is what lets the expectation be derived instead of asserted.
const DIM_BUCKETS: u64 = 8;

/// The bucket the join's `WHERE` keeps. See [`DIM_BUCKETS`].
const JOIN_BUCKET: u64 = 3;

/// How many keys the join's inner side yields — the size the semi-join push-down is bounded by.
///
/// `dim` holds one row per high-cardinality group (capped at [`DIM_ROWS`]) and the join keeps one
/// bucket of them, so this counts `k` in `0..dim_rows` with `k % DIM_BUCKETS == JOIN_BUCKET`.
/// Against `esker_columnar::fragment::MAX_IN_VALUES` it says whether the rewrite can express this
/// join at all: 500 keys at `--groups-high 4000` (it can), 12,500 at the default 100,000 (it
/// cannot, and the planner refuses in as many words).
pub(crate) fn join_inner_keys(shape: Shape) -> u64 {
    shape
        .dim_rows()
        .saturating_sub(JOIN_BUCKET)
        .div_ceil(DIM_BUCKETS)
}

/// The day the control's filter keeps rows below, which is about four fifths of them.
///
/// A filter that kept everything would be pruned away by nothing and would measure a bare scan;
/// one that kept almost nothing would measure the stripe statistics. Four fifths is a filter that
/// runs.
pub(crate) const CONTROL_DAY: u64 = 300;

/// Creates both tables, empty.
pub(crate) fn create(pg: &mut Pg) -> Result<(), String> {
    pg.run(&format!(
        "CREATE TABLE {FACT} (
             id      int8 PRIMARY KEY,
             day     int8 NOT NULL,
             g32     int8 NOT NULL,
             ghigh   int8 NOT NULL,
             amount  int8 NOT NULL,
             label   text NOT NULL,
             payload text NOT NULL
         )"
    ))?;
    pg.run(&format!(
        "CREATE TABLE {DIM} (k int8 PRIMARY KEY, bucket int8 NOT NULL, name text NOT NULL)"
    ))
}

/// Fills both tables, answering with how many statements it took.
///
/// Batched because a row at a time is not a load, it is a measurement of `commit_group`
/// (`docs/bench/columnar-learner.md`, "The ingestion number is not a speedup").
pub(crate) fn load(pg: &mut Pg, shape: Shape) -> Result<u64, String> {
    let mut rng = Pcg32::from_seed(shape.seed);
    let mut statements = 0;
    let mut sql = String::with_capacity(1 << 20);
    let mut id = 1;
    while id <= shape.rows {
        let last = (id + shape.batch - 1).min(shape.rows);
        sql.clear();
        let _ = write!(sql, "INSERT INTO {FACT} VALUES ");
        for row in id..=last {
            if row > id {
                sql.push(',');
            }
            // `amount` is drawn once per row from the seeded generator, so two runs at the same
            // seed hold the same bytes and the sums are comparable across them.
            let amount = rng.range_inclusive(0, 999_999);
            let _ = write!(
                sql,
                "({row},{},{},{},{amount},'label-{}','{}')",
                row % DAYS,
                row % GROUPS_LOW,
                row % shape.groups_high,
                row % 8,
                payload(row)
            );
        }
        pg.run(&sql)?;
        statements += 1;
        id = last + 1;
    }

    let dim_rows = shape.dim_rows();
    let mut key = 0;
    while key < dim_rows {
        let last = (key + shape.batch).min(dim_rows);
        sql.clear();
        let _ = write!(sql, "INSERT INTO {DIM} VALUES ");
        for row in key..last {
            if row > key {
                sql.push(',');
            }
            let _ = write!(sql, "({row},{},'dim-{row}')", row % DIM_BUCKETS);
        }
        pg.run(&sql)?;
        statements += 1;
        key = last;
    }
    Ok(statements)
}

/// Loads `shape.rows` more rows, numbered from `after`.
///
/// The same generator as [`load`], continuing the key space rather than restarting it, so the
/// table grows past another split threshold instead of colliding with itself.
pub(crate) fn load_more(pg: &mut Pg, shape: Shape, after: u64) -> Result<(), String> {
    let mut rng = Pcg32::from_seed(shape.seed ^ after);
    let mut sql = String::with_capacity(1 << 20);
    let mut id = after + 1;
    let last_row = after + shape.rows;
    while id <= last_row {
        let last = (id + shape.batch - 1).min(last_row);
        sql.clear();
        let _ = write!(sql, "INSERT INTO {FACT} VALUES ");
        for row in id..=last {
            if row > id {
                sql.push(',');
            }
            let amount = rng.range_inclusive(0, 999_999);
            let _ = write!(
                sql,
                "({row},{},{},{},{amount},'label-{}','{}')",
                row % DAYS,
                row % GROUPS_LOW,
                row % shape.groups_high,
                row % 8,
                payload(row)
            );
        }
        pg.run(&sql)?;
        id = last + 1;
    }
    Ok(())
}

/// A 32-byte payload from a small dictionary.
///
/// Compressible, like real text: an incompressible column would put a floor under every ratio and
/// measure the generator rather than the format (`docs/bench/columnar-m2.md`, "Compression").
fn payload(row: u64) -> String {
    const WORDS: [&str; 8] = [
        "north", "south", "east", "west", "inbound", "outbound", "settled", "pending",
    ];
    let mut text = String::with_capacity(32);
    for step in 0..4 {
        if step > 0 {
            text.push('-');
        }
        let index = usize::try_from((row / (step + 1)) % 8).unwrap_or(0);
        text.push_str(WORDS[index]);
    }
    text
}

/// What a query is for, which is what decides how its number is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A full scan with a filter and one output row. **The control**: no exchange can make it
    /// faster, because there is nothing to shuffle. If its arms move together with the others',
    /// the run measured the machine and not the query.
    Control,
    /// A `GROUP BY` whose answer is a handful of rows.
    LowCardinality,
    /// A `GROUP BY` whose answer is comparable in size to the scan itself.
    HighCardinality,
    /// A join of two large tables on a key neither is partitioned by.
    Join,
}

/// One measured query.
#[derive(Debug, Clone)]
pub(crate) struct Query {
    /// Its name in the report.
    pub(crate) name: &'static str,
    /// What it is for.
    pub(crate) kind: Kind,
    /// The statement.
    pub(crate) sql: String,
    /// How many rows it must answer with, asserted on every run.
    ///
    /// A query whose answer changed between arms is a query that measured two different things,
    /// and a routed plan that quietly returned a different number of groups is the exact failure
    /// ADR 0022's differential exists to catch.
    pub(crate) expected_rows: u64,
    /// Whether this shape can reach the columnar path at all.
    ///
    /// **Derived, never declared** — for the aggregates it is a constant `true`, and for the join
    /// it is computed from [`join_inner_keys`] against
    /// `esker_columnar::fragment::MAX_IN_VALUES`.
    ///
    /// It was a hand-written `false` for the join, on the grounds that
    /// [ADR 0040](../../../../docs/adr/0040-the-engine-a-query-runs-on.md) Decision 4 substitutes
    /// exactly one plan shape, `Aggregate { [Filter] { SeqScan } }`, so a join had no fragment to
    /// be pushed into. True when written, and falsified by the semi-join push-down.
    ///
    /// The stale `false` was worse than a wrong comment, because this field does not *permit* an
    /// engine, it **requires** one: `mod.rs` asserts `must_be_columnar == (engine == "columnar")`.
    /// So the join arm ran on rows, the assertion demanded rows, and a comparison of the row engine
    /// against itself was published as a join measurement — green
    /// (`docs/bench/mpp-baseline.md` §11d).
    ///
    /// Flipping it to a hand-written `true` only moved the trap: every run at the default
    /// `--groups-high` then aborted, because `dim` holds one row per high-cardinality group and one
    /// bucket of 100,000 is 12,500 keys, far past the 4,096 a fragment carries. Both spellings are
    /// right for one configuration and wrong for the other, which is what makes a constant the
    /// wrong shape here. The bound decides, and the bound is imported rather than copied.
    pub(crate) columnar_is_possible: bool,
}

/// The four queries, in the order the record reports them.
pub(crate) fn queries(shape: Shape) -> Vec<Query> {
    vec![
        Query {
            name: "control-scan",
            kind: Kind::Control,
            sql: format!("SELECT count(*), sum(amount) FROM {FACT} WHERE day < {CONTROL_DAY}"),
            expected_rows: 1,
            columnar_is_possible: true,
        },
        Query {
            name: "group-low",
            kind: Kind::LowCardinality,
            sql: format!("SELECT g32, count(*), sum(amount) FROM {FACT} GROUP BY g32"),
            expected_rows: GROUPS_LOW.min(shape.rows),
            columnar_is_possible: true,
        },
        Query {
            name: "group-high",
            kind: Kind::HighCardinality,
            sql: format!("SELECT ghigh, count(*), sum(amount) FROM {FACT} GROUP BY ghigh"),
            expected_rows: shape.groups_high.min(shape.rows),
            columnar_is_possible: true,
        },
        Query {
            name: "join",
            kind: Kind::Join,
            sql: format!(
                "SELECT count(*) FROM {FACT} JOIN {DIM} ON {FACT}.ghigh = {DIM}.k \
                 WHERE {DIM}.bucket = {JOIN_BUCKET}"
            ),
            expected_rows: 1,
            // The whole point of the field: at `--groups-high 4000` this is 500 keys and the join
            // routes; at the default 100,000 it is 12,500 and the planner refuses. Both are
            // correct, and the run asserts whichever one this configuration earns.
            columnar_is_possible: join_inner_keys(shape)
                <= esker_columnar::fragment::MAX_IN_VALUES as u64,
        },
    ]
}

/// The statements the correctness half runs, each naming what it is evidence about.
///
/// Deliberately wider than [`queries`]: the timed set is four shapes chosen to answer one
/// question, and this set exists to say *what the path does* across a region boundary — so it
/// includes the shapes that are not routed at all, because a boundary breaks a row scan and a
/// point read as readily as it breaks a fragment.
pub(crate) fn diagnostics(shape: Shape) -> Vec<(&'static str, String)> {
    let mid = shape.rows / 2;
    let mut out = vec![
        (
            "count(*), whole table",
            format!("SELECT count(*) FROM {FACT}"),
        ),
        (
            "aggregate with a filter",
            format!("SELECT count(*), sum(amount) FROM {FACT} WHERE day < {CONTROL_DAY}"),
        ),
        (
            "GROUP BY, low cardinality",
            format!("SELECT g32, count(*) FROM {FACT} GROUP BY g32 ORDER BY g32"),
        ),
        (
            "GROUP BY, high cardinality",
            format!("SELECT ghigh, count(*) FROM {FACT} GROUP BY ghigh ORDER BY ghigh LIMIT 5"),
        ),
        (
            "join, semi-join shape",
            format!(
                "SELECT count(*) FROM {FACT} JOIN {DIM} ON {FACT}.ghigh = {DIM}.k \
                 WHERE {DIM}.bucket = {JOIN_BUCKET}"
            ),
        ),
        // A point read and a bounded range are never routed (ADR 0022 rule 1); they are here
        // because they cross the boundary through the *row* path, which is the half a fragment
        // never exercises.
        (
            "point read, low key",
            format!("SELECT id, amount FROM {FACT} WHERE id = 1"),
        ),
        (
            "point read, past the split",
            format!("SELECT id, amount FROM {FACT} WHERE id = {mid}"),
        ),
        (
            "range scan across the boundary",
            format!("SELECT count(*) FROM {FACT} WHERE id BETWEEN 1 AND {mid}"),
        ),
        (
            "full row scan, ordered",
            format!("SELECT id FROM {FACT} ORDER BY id LIMIT 3"),
        ),
        // A write after the split: the region cache is a hint repaired by the refusals it causes,
        // and a write is the operation that meets a stale one hardest.
        (
            "insert past the split",
            format!(
                "INSERT INTO {FACT} VALUES ({}, 1, 1, 1, 1, 'label-0', 'x')",
                shape.rows + 1
            ),
        ),
        (
            "count(*) after that insert",
            format!("SELECT count(*) FROM {FACT}"),
        ),
    ];
    out.retain(|(_, sql)| !sql.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::{GROUPS_LOW, Kind, Shape, join_inner_keys, payload, queries};

    fn shape() -> Shape {
        Shape {
            rows: 1_000,
            groups_high: 100,
            batch: 10,
            seed: 7,
        }
    }

    /// The join expects whichever engine its inner side earns, in **both** directions.
    ///
    /// This is the test the field's own history asks for. A hand-written `false` made the join arm
    /// assert the row engine after the push-down had landed, so a row-versus-row comparison was
    /// published as a join measurement and stayed green (`docs/bench/mpp-baseline.md` §11d);
    /// flipping it to a hand-written `true` only moved the trap, aborting every run at the default
    /// `--groups-high`. A constant is right for one configuration and wrong for the other, so the
    /// assertion has to check both — a test that only ran the small case would have passed against
    /// the `true` that broke the default.
    #[test]
    fn the_join_expects_whichever_engine_its_inner_side_earns() {
        let expects_columnar = |groups_high| {
            let shape = Shape {
                groups_high,
                ..shape()
            };
            let join = queries(shape)
                .into_iter()
                .find(|query| query.kind == Kind::Join)
                .expect("the workload has a join");
            (join_inner_keys(shape), join.columnar_is_possible)
        };

        // 4,000 groups is 500 keys in one bucket of eight, under the 4,096 a fragment carries.
        assert_eq!(expects_columnar(4_000), (500, true));
        // The shipped default is 12,500, past it — the planner refuses and the run must expect
        // rows rather than call the refusal a fault.
        assert_eq!(expects_columnar(100_000), (12_500, false));
        // The boundary itself, from both sides: a fragment carries exactly MAX_IN_VALUES.
        assert_eq!(
            expects_columnar(32_768).1,
            true,
            "8 * 4,096 keys is the cap exactly"
        );
        assert_eq!(expects_columnar(32_776).1, false, "one key past it");
    }

    #[test]
    fn every_measured_query_projects_at_most_half_the_columns() {
        // The fact table has seven columns and the ratio is a half
        // (`esker_sql::plan::routing::RATIO`), so three is the most a routed query may read.
        // Asserted by counting the column names each statement mentions, because a later edit
        // that adds a column to a projection would silently move every query onto the rows and
        // the run would still be green.
        for query in queries(shape()) {
            if !query.columnar_is_possible {
                continue;
            }
            let read = ["id", "day", "g32", "ghigh", "amount", "label", "payload"]
                .into_iter()
                .filter(|column| query.sql.contains(column))
                .count();
            assert!(
                read * 2 <= 7,
                "{} projects {read} of 7 columns, which the ratio sends to the rows",
                query.name
            );
        }
    }

    #[test]
    fn the_control_is_the_only_query_that_cannot_gain_from_an_exchange() {
        let kinds: Vec<Kind> = queries(shape()).into_iter().map(|q| q.kind).collect();
        assert_eq!(kinds.iter().filter(|k| **k == Kind::Control).count(), 1);
        assert!(kinds.contains(&Kind::HighCardinality));
        assert!(kinds.contains(&Kind::Join));
    }

    #[test]
    fn a_group_count_never_exceeds_the_rows_it_groups() {
        let tiny = Shape {
            rows: 4,
            groups_high: 1_000,
            ..shape()
        };
        for query in queries(tiny) {
            assert!(
                query.expected_rows <= tiny.rows.max(1),
                "{} expects {} rows from {} rows of data",
                query.name,
                query.expected_rows,
                tiny.rows
            );
        }
        assert_eq!(queries(shape())[1].expected_rows, GROUPS_LOW);
    }

    #[test]
    fn a_payload_is_the_width_the_ratio_needs_and_is_not_random() {
        assert_eq!(payload(7), payload(7));
        assert!(payload(7).len() >= 16);
    }
}
