//! What a fragment dispatch does when the region it was planned against moved under it.
//!
//! `docs/plans/phase-16-mpp.md` §J12. The planner builds one fragment per region and sends each
//! with the epoch it saw, so a region that split between planning and dispatch is refused by the
//! store rather than answered about the half that is left — which is what makes "one fragment per
//! region" cover the whole table or nothing. Until this unit the refusal ended the columnar
//! attempt: every fragment fell back to the row plan, correct and slow, and on a cluster that is
//! actively splitting *every* query paid it.
//!
//! # The one rule these tests are about
//!
//! A replacement set is followed only when it **tiles the refused range exactly**. A fragment is
//! not a cursor: `esker-client`'s scan paths repair a route by believing the bounds the store
//! named and continuing from where they stopped, which is safe because a resumed walk covers each
//! key once. A fragment is an aggregate over the whole of one region's columnar copy, so what a
//! re-dispatch changes is not where a walk resumes but **which rows are counted** — and a
//! replacement covering one byte more than the shard it replaces counts that byte twice.
//!
//! So: a split is followed, a merge is not, a gap is not, and a region that keeps moving runs out
//! of budget. Three of the four assert the **fallback**, not the answer, and that is deliberate —
//! the row engine is right, so agreeing with it proves nothing about which rule produced the
//! agreement (`docs/plans/phase-16-mpp.md`, "Agreement is not correctness").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use esker_client::wire::{Epoch, Peer, PeerRole};
use esker_proto::fragment::ScanStats;
use esker_proto::fragment::result::{Body, Group, Partial, ValueType};
use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::fragment::{Answer, FragmentSource, Shard};
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};

// ---------------------------------------------------------------------------------------------
// A cluster whose regions move
// ---------------------------------------------------------------------------------------------

/// A fragment source with a routing table that changes the first time a fragment fails.
///
/// The shape is the real one: the planner asks for the shards covering a table, sends a fragment
/// to each, and one of them fails because that region no longer exists as the planner saw it. From
/// that moment the driver answers with the tiling that replaced it — which is what
/// [`FragmentSource::shards`] being asked a *second* time, over the refused shard's range alone,
/// is for.
#[derive(Debug)]
struct Moving {
    /// The tiling of the whole key space before the move.
    before: Vec<Shard>,
    /// The tiling after it. Whether it is a legal replacement is what each test varies.
    after: Vec<Shard>,
    /// Set by the first fragment that fails; from then on [`Self::shards`] answers from `after`.
    moved: Mutex<bool>,
    /// Region ids whose fragment fails as a transport error, once.
    stale: Vec<u64>,
    /// Every region a fragment was actually sent to, in order. The assertion of this file.
    asked: Mutex<Vec<u64>>,
    /// What each region answers, by region id.
    answers: BTreeMap<u64, u64>,
}

impl Moving {
    fn new(before: Vec<Shard>, after: Vec<Shard>, stale: &[u64], answers: &[(u64, u64)]) -> Self {
        Self {
            before,
            after,
            moved: Mutex::new(false),
            stale: stale.to_vec(),
            asked: Mutex::new(Vec::new()),
            answers: answers.iter().copied().collect(),
        }
    }
}

impl FragmentSource for Moving {
    /// `true`: this source really does scope a fragment to its shard, which is what makes the
    /// counts below add up to one right answer rather than a multiple of one.
    fn runs_are_region_scoped(&self) -> bool {
        true
    }

    /// **The range is ignored, as it is in `tests/routing.rs`'s source**, and the reason is worth
    /// stating: a real table's rows live under a `'t'`-prefixed key range whose exact bounds this
    /// file would have to reconstruct to place a synthetic split point inside them, and a split
    /// point that landed outside would silently return one region and make every test below pass
    /// for the wrong reason. What is under test is the *tiling rule*, which reads the shards'
    /// own bounds — so the bounds have to be real relative to each other, and need not be real
    /// relative to the table.
    ///
    /// The two calls are therefore answered in order: the planner's enumeration gets `before`, and
    /// the one repair `re_routed` makes gets `after`, which is the replacement for the region that
    /// failed.
    fn shards(&self, _start: &[u8], _end: &[u8]) -> esker_sql::Result<Vec<Shard>> {
        Ok(if *self.moved.lock().unwrap() {
            self.after.clone()
        } else {
            self.before.clone()
        })
    }

    fn evaluate(
        &self,
        shard: &Shard,
        _fragment: &[u8],
        _ts: u64,
        _min_apply_index: u64,
    ) -> esker_sql::Result<Answer> {
        self.asked.lock().unwrap().push(shard.region_id);
        let mut moved = self.moved.lock().unwrap();
        if self.stale.contains(&shard.region_id) && !*moved {
            *moved = true;
            return Err(esker_sql::SqlError::StoreUnavailable(format!(
                "region {} is not what it was",
                shard.region_id
            )));
        }
        drop(moved);
        Ok(counted(
            self.answers.get(&shard.region_id).copied().unwrap_or(0),
        ))
    }
}

/// A source whose region is replaced on **every** attempt, one for one.
///
/// Nothing about it is illegal — each replacement tiles the refused range exactly — so what stops
/// it is the budget alone, which is the only thing this variant is here to prove.
#[derive(Debug, Default)]
struct AlwaysMoving {
    /// Region ids handed out so far, which is also the number of fragments sent.
    asked: Mutex<Vec<u64>>,
    next: Mutex<u64>,
}

impl FragmentSource for AlwaysMoving {
    fn runs_are_region_scoped(&self) -> bool {
        true
    }

    fn shards(&self, start: &[u8], end: &[u8]) -> esker_sql::Result<Vec<Shard>> {
        let mut next = self.next.lock().unwrap();
        *next += 1;
        Ok(vec![region(*next, start, end)])
    }

    fn evaluate(
        &self,
        shard: &Shard,
        _fragment: &[u8],
        _ts: u64,
        _min_apply_index: u64,
    ) -> esker_sql::Result<Answer> {
        self.asked.lock().unwrap().push(shard.region_id);
        Err(esker_sql::SqlError::StoreUnavailable(
            "this region has moved again".to_owned(),
        ))
    }
}

/// One region of the key space, with a columnar learner on it.
fn region(id: u64, start: &[u8], end: &[u8]) -> Shard {
    Shard {
        region_id: id,
        epoch: Epoch::INITIAL,
        start: bytes::Bytes::copy_from_slice(start),
        end: bytes::Bytes::copy_from_slice(end),
        columnar: Some(Peer {
            store_id: 9,
            peer_id: 9,
            role: PeerRole::ColumnarLearner,
        }),
    }
}

/// One region's answer to `count(*)`.
fn counted(rows: u64) -> Answer {
    let body = Body::Groups {
        key_types: Vec::new(),
        aggregates: vec![(Partial::Count(0).kind(), None::<ValueType>)],
        groups: vec![Group {
            key: Vec::new(),
            partials: vec![Partial::Count(rows)],
        }],
    };
    Answer::Answered {
        result: bytes::Bytes::from(
            esker_proto::fragment::result::encode(&body).expect("a well-formed body"),
        ),
        stats: ScanStats::default(),
    }
}

// ---------------------------------------------------------------------------------------------
// The node
// ---------------------------------------------------------------------------------------------

fn node_with(source: Arc<dyn FragmentSource>) -> Executor {
    Executor::new(
        Arc::new(MemoryBackend::new()) as Arc<dyn Backend>,
        Arc::new(Catalog::new()),
        1,
        esker_sql::session::register(),
    )
    .asking_fragments_of(source)
}

/// A table with a columnar copy, and **two** rows in it.
///
/// Two, and the scripted regions below add to twelve, so a routed answer and a fallback answer are
/// different numbers. A test that asserted a number both paths produce would pass against a
/// dispatch that never routed at all.
fn ready(node: &mut Executor) {
    run(node, "CREATE TABLE t (id int8 PRIMARY KEY, amount int8)").unwrap();
    run(node, "ALTER TABLE t SET (columnar_replicas = 1)").unwrap();
    run(node, "INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
}

fn run(node: &mut Executor, sql: &str) -> esker_sql::Result<()> {
    for parsed in parse_statements(sql)? {
        match parsed.class() {
            StatementClass::Begin => node.begin(parsed.begins_read_only())?,
            StatementClass::Commit => node.commit()?,
            StatementClass::Rollback => node.rollback()?,
            _ => {
                let _: Outcome = node.execute(&parsed, &Params::NONE)?;
            }
        }
    }
    Ok(())
}

fn rows(node: &mut Executor, sql: &str) -> Vec<Vec<String>> {
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

/// The `EXPLAIN ANALYZE` of a query, as one string. Runs it, which is where a refusal appears.
fn explain_analyze(node: &mut Executor, sql: &str) -> String {
    rows(node, &format!("EXPLAIN ANALYZE {sql}"))
        .into_iter()
        .map(|row| row.join(""))
        .collect::<Vec<_>>()
        .join("\n")
}

const COUNT: &str = "SELECT count(*) FROM t";

// ---------------------------------------------------------------------------------------------
// A split is followed
// ---------------------------------------------------------------------------------------------

/// **The unit.** Region 1 split into 3 and 4 while the query was in flight; the fragment sent to 1
/// fails, and the dispatch asks 3 and 4 instead of giving up on the columnar path.
///
/// The three assertions are three different claims and none of them implies the others: the
/// **answer** is the fold of every region (12, which the row engine cannot produce — it holds 2);
/// the **requests** went to the regions that replaced the stale one, in its place in the walk and
/// before the region after it; and `EXPLAIN` still names the columnar engine, because a repaired
/// route is not a fallback.
#[test]
fn a_region_that_split_under_a_query_is_asked_again_as_its_halves() {
    let source = Arc::new(Moving::new(
        vec![region(1, b"", b"m"), region(2, b"m", b"")],
        vec![region(3, b"", b"f"), region(4, b"f", b"m")],
        &[1],
        &[(3, 4), (4, 3), (2, 5)],
    ));
    let mut node = node_with(Arc::clone(&source) as Arc<dyn FragmentSource>);
    ready(&mut node);

    assert_eq!(
        rows(&mut node, COUNT),
        vec![vec!["12"]],
        "the split region's rows were dropped, or the query fell back to the two rows on disk"
    );
    assert_eq!(
        *source.asked.lock().unwrap(),
        vec![1, 3, 4, 2],
        "the replacements were not walked in the place of the shard they replaced"
    );
}

/// The same, when the region that moved is the **last** one — whose end is the end of the key
/// space.
///
/// Its own test because an empty `end` is a sentinel, and the two ways to get this wrong are
/// symmetrical: compare it as bytes and the last region tiles nothing, treat every empty end as
/// equal and a region ending at `+∞` matches one ending at `m`. `cross_region_scan.rs` found the
/// row path's version of exactly this — "the last row, in the last region" — which is the reason
/// this case is written out rather than trusted.
#[test]
fn the_last_region_splitting_is_followed_too() {
    let source = Arc::new(Moving::new(
        vec![region(1, b"", b"m"), region(2, b"m", b"")],
        vec![region(3, b"m", b"t"), region(4, b"t", b"")],
        &[2],
        &[(1, 4), (3, 3), (4, 5)],
    ));
    let mut node = node_with(Arc::clone(&source) as Arc<dyn FragmentSource>);
    ready(&mut node);

    assert_eq!(rows(&mut node, COUNT), vec![vec!["12"]]);
    assert_eq!(*source.asked.lock().unwrap(), vec![1, 2, 3, 4]);
}

// ---------------------------------------------------------------------------------------------
// Everything that is not an exact re-tiling falls back
// ---------------------------------------------------------------------------------------------

/// **A merge is not followed.** The regions that now cover the refused range reach past its end,
/// so following them would count `[m, z)` twice — once here and once for region 2, which still
/// holds it.
///
/// It asserts the fallback and the requests, not the answer: the row engine is correct, so the
/// number alone cannot tell this rule from any other reason to fall back.
#[test]
fn a_replacement_that_reaches_past_the_refused_range_falls_back_to_the_rows() {
    let source = Arc::new(Moving::new(
        vec![region(1, b"", b"m"), region(2, b"m", b"")],
        vec![
            region(3, b"", b"f"),
            region(5, b"f", b"z"),
            region(6, b"z", b""),
        ],
        &[1],
        &[(3, 4), (5, 3), (2, 5), (6, 1)],
    ));
    let mut node = node_with(Arc::clone(&source) as Arc<dyn FragmentSource>);
    ready(&mut node);

    let ran = explain_analyze(&mut node, COUNT);
    assert!(ran.contains("Engine: rows"), "{ran}");
    assert!(ran.contains("columnar refused"), "{ran}");
    assert_eq!(
        *source.asked.lock().unwrap(),
        vec![1],
        "a widened replacement was dispatched to anyway"
    );
}

/// **A gap is not followed.** The replacements leave `[c, f)` covered by nobody, and an aggregate
/// built from part of a table is a number that cannot be recognised as part of one — the reason
/// the shard list is required to be complete before a query is routed at all.
#[test]
fn a_replacement_with_a_hole_in_it_falls_back_to_the_rows() {
    let source = Arc::new(Moving::new(
        vec![region(1, b"", b"m"), region(2, b"m", b"")],
        vec![
            region(3, b"", b"c"),
            region(4, b"f", b"m"),
            region(2, b"m", b""),
        ],
        &[1],
        &[(3, 4), (4, 3), (2, 5)],
    ));
    let mut node = node_with(Arc::clone(&source) as Arc<dyn FragmentSource>);
    ready(&mut node);

    let ran = explain_analyze(&mut node, COUNT);
    assert!(ran.contains("Engine: rows"), "{ran}");
    assert_eq!(*source.asked.lock().unwrap(), vec![1]);
}

/// **A region that keeps moving runs out of budget rather than out of patience.**
///
/// Every replacement here is legal — one region, tiling the refused range exactly — and every one
/// of them fails too. Nothing in the tiling rule stops this; only the budget does, and without one
/// a splitting cluster turns a single statement into an unbounded walk.
///
/// The assertion is the **count of fragments sent**, because that is the thing that would grow
/// without bound: one original attempt plus `MAX_ROUTE_REPAIRS` of them.
#[test]
fn a_region_that_keeps_moving_stops_after_a_bounded_number_of_repairs() {
    let source = Arc::new(AlwaysMoving::default());
    let mut node = node_with(Arc::clone(&source) as Arc<dyn FragmentSource>);
    ready(&mut node);

    let ran = explain_analyze(&mut node, COUNT);
    assert!(ran.contains("Engine: rows"), "{ran}");
    // Read the count out and drop the guard: the assertion below runs another query, and this
    // source locks `asked` inside `evaluate`.
    let sent = source.asked.lock().unwrap().len();
    assert_eq!(
        sent, 5,
        "one attempt and four repairs is the budget; this query sent {sent} fragments"
    );
    assert_eq!(
        rows(&mut node, COUNT),
        vec![vec!["2"]],
        "the fallback did not answer what the rows say"
    );
}

/// **A store that is down is not a region that moved.** The routing table answers with the same
/// region, so there is nothing to re-dispatch to and the budget is not spent pretending otherwise.
///
/// Without this the commonest failure in the cluster — one store unreachable — would cost five
/// round trips per shard instead of one before reaching the same row plan.
#[test]
fn an_unreachable_store_is_asked_once_and_not_five_times() {
    let source = Arc::new(Moving::new(
        vec![region(1, b"", b"")],
        vec![region(1, b"", b"")],
        &[1],
        &[(1, 7)],
    ));
    let mut node = node_with(Arc::clone(&source) as Arc<dyn FragmentSource>);
    ready(&mut node);

    let ran = explain_analyze(&mut node, COUNT);
    assert!(ran.contains("Engine: rows"), "{ran}");
    assert_eq!(
        *source.asked.lock().unwrap(),
        vec![1],
        "an unchanged routing answer was treated as a repair"
    );
}
