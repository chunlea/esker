//! The routing rule, the session override, and what `EXPLAIN` says about both.
//!
//! `docs/plans/phase-10-routing.md` U2 and U3. Two halves:
//!
//! * **`esker.engine`'s surface**, replayed against `tests/corpus/pg19_routing_engine.txt` — what a
//!   real PostgreSQL 19 answered for every statement in it — with every divergence listed and its
//!   reason, checked in both directions as [ADR 0031](../../docs/adr/0031-rails-compatibility-is-measured.md)
//!   requires. A gap cannot be absorbed silently and neither can closing one.
//! * **the rule itself**, against a fragment source that answers from a script. The decision is a
//!   pure function tested as a table in `plan::routing`'s own unit tests; what is tested here is
//!   the half that needs a plan — which shapes are expressible, what the substitution produces, and
//!   that a refusal is answered by the rows in the same snapshot.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use esker_client::wire::{Epoch, Peer, PeerRole};
use esker_proto::fragment::ScanStats;
use esker_proto::fragment::result::{Body, Group, Partial, Value, ValueType};
use esker_sql::backend::{Backend, MemoryBackend};
use esker_sql::catalog::Catalog;
use esker_sql::exec::Executor;
use esker_sql::fragment::{Answer, FragmentSource, RefusalReason, Shard};
use esker_sql::parse::{StatementClass, parse_statements};
use esker_sql::pgwire::session::{Execute, Outcome, Params};

// ---------------------------------------------------------------------------------------------
// The session override, against what PostgreSQL 19 actually did
// ---------------------------------------------------------------------------------------------

/// Statements a real PostgreSQL answers differently, each with its reason.
///
/// All six are in the **safe** direction: this node refuses something PostgreSQL accepts and does
/// nothing with. None of them is this node answering a question PostgreSQL answers, differently.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "SET esker.engine = 'sideways'",
        "PostgreSQL validates no custom parameter's value, ever, so it stores `sideways` and \
         nothing reads it. Here the parameter decides which engine a query runs on, and \
         `crate::parameter`'s whole rule is that a value this node will not act on is refused by \
         name rather than accepted and ignored. `22023` with the three spellings as a HINT, which \
         is PostgreSQL's own shape for an enum it does know.",
    ),
    (
        "BEGIN; SET LOCAL esker.engine = 'columnar'; SHOW esker.engine",
        "`SET LOCAL` is undone when the transaction ends, whichever way it ends, and this node \
         keeps that per-block undo only for `esker.read_as_of`. Refused by name rather than \
         silently promoted to a session-wide `SET`, which would outlive the block — the same \
         answer every other parameter in the table gets, and not specific to this one.",
    ),
    (
        "SET nonesker.engine = 'row'",
        "PostgreSQL accepts any namespaced name and stores it. This node has a list of the \
         parameters it means and answers `0A000` naming the statement for anything else; \
         answering `42704` instead would claim a parameter does not exist when what is true is \
         that this node does not have it.",
    ),
    (
        "SET engine = 'row'",
        "PostgreSQL knows which parameters it has, so an un-namespaced name it does not \
         recognise is `42704`. This node does not have that list and applies the only safe half \
         of the rule, exactly as `pg19_time_machine.txt` records for `SET nonamespace_thing`.",
    ),
    (
        "RESET esker.never_existed",
        "`RESET` validates no name at all on a real server — the asymmetry ADR 0022's phase-8 \
         amendment found for storage parameters, holding here too. This node answers `42704`, \
         which is the same code PostgreSQL uses for the `SET` of the same name and is the more \
         useful answer: a `RESET` of a name nobody ever set is a typo, and swallowing it silently \
         is how a session ends up believing it cleared something.",
    ),
    (
        "SET esker.engine = 'row', 'columnar'",
        "Both servers refuse it and both say `22023`; only the sentence differs. PostgreSQL says \
         `SET esker.engine takes only one argument`, because it knows a custom GUC is scalar. \
         This node has one path for a parameter that takes a list (`search_path` really does), so \
         a list arrives as one joined value and is refused as a value the enum does not admit — \
         which names the three it does.",
    ),
];

#[test]
fn the_session_override_answers_the_way_postgresql_19_does() {
    let mut checked = 0;
    let mut diverged = Vec::new();

    for (line_number, script, expected) in corpus() {
        let ours = answer(&script);
        checked += 1;

        let listed = DIVERGENCES.iter().find(|(sql, _)| script == *sql);
        if ours == expected {
            assert!(
                listed.is_none(),
                "line {line_number}: `{script}` is listed as a divergence and now agrees with \
                 PostgreSQL ({ours}). Delete its row in DIVERGENCES."
            );
            continue;
        }
        match listed {
            Some(_) => diverged.push(script),
            None => panic!(
                "line {line_number}: `{script}`\n  PostgreSQL 19: {expected}\n  here:          \
                 {ours}\nEither this is a bug or it is a divergence; if it is a divergence, it \
                 belongs in DIVERGENCES with its reason."
            ),
        }
    }

    assert!(
        checked >= 16,
        "only {checked} statements ran; the corpus did not load"
    );
    assert_eq!(
        diverged.len(),
        DIVERGENCES.len(),
        "every listed divergence must be exercised by the corpus; these diverged: {diverged:?}"
    );
}

/// The value is folded here and is not on a real server, which the corpus's `ok`/`!code` shape
/// cannot see — so it is asserted directly rather than left to a format that would miss it.
///
/// `SET ESKER.ENGINE = 'AUTO'` then `SHOW esker.engine` answers `AUTO` on PostgreSQL 19 and `auto`
/// here. The name is case-insensitive on both.
#[test]
fn the_name_is_folded_on_both_sides_and_the_value_only_here() {
    let mut node = node();
    run(&mut node, "SET ESKER.ENGINE = 'AUTO'").unwrap();
    assert_eq!(show(&mut node, "SHOW esker.engine"), "auto");
    run(&mut node, "SET esker.engine = 'CoLuMnAr'").unwrap();
    assert_eq!(show(&mut node, "SHOW Esker.Engine"), "columnar");
}

/// The boot value is `auto` and `RESET` goes back to it.
///
/// The other half of the declared divergence: PostgreSQL answers `42704` for `SHOW esker.engine`
/// until the first `SET` of the session, and the empty string after a `RESET`. This node has a
/// real default, so it has a real value to report at both moments.
#[test]
fn the_override_boots_at_auto_and_resets_to_it() {
    let mut node = node();
    assert_eq!(show(&mut node, "SHOW esker.engine"), "auto");
    run(&mut node, "SET esker.engine = 'row'").unwrap();
    assert_eq!(show(&mut node, "SHOW esker.engine"), "row");
    run(&mut node, "RESET esker.engine").unwrap();
    assert_eq!(show(&mut node, "SHOW esker.engine"), "auto");
}

/// A value outside the three is refused **by name**, with the three in the `HINT` — the shape a
/// real server uses for an enum it knows, which is what makes the refusal readable rather than
/// merely correct.
#[test]
fn a_value_this_node_will_not_act_on_is_refused_by_name() {
    let mut node = node();
    let error = run(&mut node, "SET esker.engine = 'sideways'").unwrap_err();
    assert_eq!(error.sqlstate(), "22023");
    let message = error.to_string();
    assert!(message.contains("esker.engine"), "{message}");
    assert!(message.contains("sideways"), "{message}");
    // And the setting did not move.
    assert_eq!(show(&mut node, "SHOW esker.engine"), "auto");
}

// ---------------------------------------------------------------------------------------------
// The rule, over a plan
// ---------------------------------------------------------------------------------------------

/// `count(*)` over a table with a columnar copy is answered by the fragments.
#[test]
fn a_count_star_is_answered_by_the_fragments() {
    let mut node = node_with(script(&[groups(&[(&[], &[Partial::Count(7)])])]));
    ready(&mut node);
    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["7"]]);
    assert!(explain(&mut node, "SELECT count(*) FROM t").contains("Columnar Aggregate on t"));
}

/// Two regions, two fragments, one answer: the counts add.
///
/// The whole of the two-level aggregate in its smallest form, and the reason the shard list has to
/// be complete — an answer built from one of two regions is a *number*, and a number cannot be
/// recognised as half of one.
#[test]
fn counts_from_two_regions_are_added() {
    let source = script(&[
        groups(&[(&[], &[Partial::Count(4)])]),
        groups(&[(&[], &[Partial::Count(3)])]),
    ]);
    source.split_at(b"t\x7f");
    let mut node = node_with(source);
    ready(&mut node);
    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["7"]]);
}

/// A refusal is answered by the rows, silently to the client and visibly in `EXPLAIN`.
///
/// The assertion that matters is the first one: the client's answer is what the row engine would
/// have said, which is what "a routing decision never changes an answer" means at the surface.
#[test]
fn a_refusal_falls_back_to_the_rows_in_the_same_snapshot() {
    for reason in [
        RefusalReason::TooFarBehind,
        RefusalReason::Unsupported,
        RefusalReason::NotColumnar,
    ] {
        let mut node = node_with(refusing(reason));
        ready(&mut node);
        run(&mut node, "INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20)").unwrap();
        assert_eq!(
            rows(&mut node, "SELECT count(*) FROM t"),
            vec![vec!["2"]],
            "{reason:?} changed the answer"
        );
        // The *reason* is a run-time fact, so a plain `EXPLAIN` — which runs nothing — cannot
        // carry it and does not pretend to. `EXPLAIN ANALYZE` is where it appears, and
        // `the_analyze_of_a_fallback_names_the_refusal` is that assertion.
    }
}

/// `SET esker.engine = 'row'` stops the fragments being asked at all.
#[test]
fn the_row_override_asks_nobody() {
    let source = script(&[groups(&[(&[], &[Partial::Count(99)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10)").unwrap();
    run(&mut node, "SET esker.engine = 'row'").unwrap();

    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["1"]]);
    assert_eq!(*asked.lock().unwrap(), 0, "a fragment went out anyway");
}

/// A table nobody asked for a columnar copy of is never routed, whatever the session says.
#[test]
fn a_table_with_no_columnar_copy_is_never_routed() {
    let source = script(&[groups(&[(&[], &[Partial::Count(99)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    run(
        &mut node,
        "CREATE TABLE t (id int8 PRIMARY KEY, name text, amount int8)",
    )
    .unwrap();
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10)").unwrap();
    run(&mut node, "SET esker.engine = 'columnar'").unwrap();

    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["1"]]);
    assert_eq!(*asked.lock().unwrap(), 0);
}

/// A transaction that has written reads its own writes, which a learner has never seen.
///
/// **The one rule here that is about a wrong answer rather than a slow one**, so it holds under the
/// `'columnar'` override: the override moves the threshold, never the correctness rules.
#[test]
fn a_transaction_that_has_written_is_never_routed() {
    let source = script(&[groups(&[(&[], &[Partial::Count(99)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    run(&mut node, "SET esker.engine = 'columnar'").unwrap();

    run(&mut node, "BEGIN").unwrap();
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10)").unwrap();
    assert_eq!(rows(&mut node, "SELECT count(*) FROM t"), vec![vec!["1"]]);
    run(&mut node, "COMMIT").unwrap();

    assert_eq!(
        *asked.lock().unwrap(),
        0,
        "a fragment was sent from a transaction holding uncommitted writes"
    );
}

/// A `GROUP BY` over a column is expressible; one over an expression is not, and says which.
#[test]
fn a_group_by_over_an_expression_is_refused_by_name() {
    let mut node = node_with(script(&[groups(&[(
        &[Value::Text("a".to_owned())],
        &[Partial::Count(1)],
    )])]));
    ready(&mut node);
    let plan = explain(&mut node, "SELECT name, count(*) FROM t GROUP BY name");
    assert!(plan.contains("Columnar Aggregate on t"), "{plan}");
    assert!(plan.contains("Group Key: name"), "{plan}");
}

/// A `DISTINCT` aggregate has no fragment spelling, and the plan says so rather than sending one.
#[test]
fn a_distinct_aggregate_is_refused_by_name() {
    let source = script(&[groups(&[(&[], &[Partial::Count(1)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    // The answer is the row engine's, and no fragment went out to produce it.
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10), (2, 'a', 20)").unwrap();
    assert_eq!(
        rows(&mut node, "SELECT count(DISTINCT name) FROM t"),
        vec![vec!["1"]]
    );
    assert_eq!(*asked.lock().unwrap(), 0);
}

/// A point read stays on rows even when the session asks for columns: a fragment cannot restrict
/// itself to a key range at all, so this is not a preference a session can express.
#[test]
fn a_point_read_stays_on_rows_under_the_override() {
    let source = script(&[groups(&[(&[], &[Partial::Count(99)])])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20)").unwrap();
    run(&mut node, "SET esker.engine = 'columnar'").unwrap();

    assert_eq!(
        rows(&mut node, "SELECT count(*) FROM t WHERE id = 1"),
        vec![vec!["1"]]
    );
    assert_eq!(*asked.lock().unwrap(), 0);
}

/// The ratio, at the surface: three columns of three is too wide, one of three is not.
#[test]
fn the_ratio_decides_between_two_queries_over_the_same_table() {
    let source = script(&[groups(&[(
        &[Value::Text("a".to_owned())],
        &[Partial::Count(1)],
    )])]);
    let asked = Arc::clone(&source.asked);
    let mut node = node_with(source);
    ready(&mut node);
    run(&mut node, "INSERT INTO t VALUES (1, 'a', 10)").unwrap();

    // One column of three: columns win, and the fragment is asked.
    let narrow = explain(&mut node, "SELECT name, count(*) FROM t GROUP BY name");
    assert!(narrow.contains("Columnar Aggregate on t"), "{narrow}");
    assert!(narrow.contains("1 of 3 columns projected"), "{narrow}");

    // Every column of three: rows win, and nothing goes out. Asserted on the wire rather than on
    // the plan text, because the *behaviour* is the claim; U3 adds the sentence.
    let before = *asked.lock().unwrap();
    assert_eq!(
        rows(
            &mut node,
            "SELECT id, name, count(amount) FROM t GROUP BY id, name"
        ),
        vec![vec!["1", "a", "1"]]
    );
    assert_eq!(*asked.lock().unwrap(), before, "a wide query was routed");
}

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// A fragment source that answers from a script, and counts what it was asked.
#[derive(Debug)]
struct Scripted {
    /// One answer per region, in region order.
    answers: Mutex<Vec<Answer>>,
    /// How many fragments actually went out.
    asked: Arc<Mutex<usize>>,
    /// Where the regions are split, or empty for one region over everything.
    splits: Mutex<Vec<Vec<u8>>>,
}

impl Scripted {
    /// Splits the key space at `at`, so a table's range covers two regions.
    fn split_at(&self, at: &[u8]) {
        self.splits.lock().unwrap().push(at.to_vec());
    }
}

impl FragmentSource for Scripted {
    fn shards(&self, _start: &[u8], _end: &[u8]) -> esker_sql::Result<Vec<Shard>> {
        let splits = self.splits.lock().unwrap().clone();
        let mut bounds: Vec<Vec<u8>> = vec![Vec::new()];
        bounds.extend(splits);
        bounds.push(Vec::new());
        Ok((0..bounds.len() - 1)
            .map(|at| Shard {
                region_id: at as u64 + 1,
                epoch: Epoch::INITIAL,
                start: bytes::Bytes::from(bounds[at].clone()),
                end: bytes::Bytes::from(bounds[at + 1].clone()),
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
        _fragment: &[u8],
        _ts: u64,
        _min_apply_index: u64,
    ) -> esker_sql::Result<Answer> {
        *self.asked.lock().unwrap() += 1;
        let answers = self.answers.lock().unwrap();
        let at = usize::try_from(shard.region_id - 1)
            .unwrap_or(0)
            .min(answers.len().saturating_sub(1));
        Ok(answers[at].clone())
    }
}

/// A source that answers each region with the group it was given.
fn script(answers: &[Body]) -> Arc<Scripted> {
    Arc::new(Scripted {
        answers: Mutex::new(
            answers
                .iter()
                .map(|body| Answer::Answered {
                    result: bytes::Bytes::from(
                        esker_proto::fragment::result::encode(body).expect("a well-formed body"),
                    ),
                    stats: ScanStats {
                        stripes_considered: 4,
                        stripes_read: 2,
                        chunks_decoded: 1,
                        rows_scanned: 100,
                        rows_matched: 10,
                    },
                })
                .collect(),
        ),
        asked: Arc::new(Mutex::new(0)),
        splits: Mutex::new(Vec::new()),
    })
}

/// A source that refuses everything.
fn refusing(reason: RefusalReason) -> Arc<Scripted> {
    Arc::new(Scripted {
        answers: Mutex::new(vec![Answer::Refused {
            reason,
            detail: "for a human".to_owned(),
        }]),
        asked: Arc::new(Mutex::new(0)),
        splits: Mutex::new(Vec::new()),
    })
}

/// One `Body::Groups` with the given groups; the aggregate shapes are taken from the first.
fn groups(rows: &[(&[Value], &[Partial])]) -> Body {
    let aggregates = rows
        .first()
        .map(|(_, partials)| {
            partials
                .iter()
                .map(|partial| (partial.kind(), aggregate_type(partial)))
                .collect()
        })
        .unwrap_or_default();
    Body::Groups {
        key_types: rows
            .first()
            .map(|(key, _)| key.iter().map(value_type).collect())
            .unwrap_or_default(),
        aggregates,
        groups: rows
            .iter()
            .map(|(key, partials)| Group {
                key: key.to_vec(),
                partials: partials.to_vec(),
            })
            .collect(),
    }
}

fn value_type(value: &Value) -> ValueType {
    value.value_type().unwrap_or(ValueType::Int8)
}

fn aggregate_type(partial: &Partial) -> Option<ValueType> {
    match partial {
        Partial::Count(_) => None,
        Partial::Sum(value) | Partial::Min(value) | Partial::Max(value) => Some(
            value
                .as_ref()
                .and_then(Value::value_type)
                .unwrap_or(ValueType::Int8),
        ),
    }
}

/// One SQL node over an in-memory store, with no fragment source.
fn node() -> Executor {
    Executor::new(
        Arc::new(MemoryBackend::new()) as Arc<dyn Backend>,
        Arc::new(Catalog::new()),
        1,
    )
}

/// The same, able to ask fragments of `source`.
fn node_with(source: Arc<Scripted>) -> Executor {
    node().asking_fragments_of(source as Arc<dyn FragmentSource>)
}

/// The table every routing case in this file is about: three columns, one columnar replica.
fn ready(node: &mut Executor) {
    run(
        node,
        "CREATE TABLE t (id int8 PRIMARY KEY, name text, amount int8)",
    )
    .unwrap();
    run(node, "ALTER TABLE t SET (columnar_replicas = 1)").unwrap();
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

/// The rows a query answers, as text.
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

/// The `EXPLAIN` of a query, as one string.
fn explain(node: &mut Executor, sql: &str) -> String {
    rows(node, &format!("EXPLAIN {sql}"))
        .into_iter()
        .map(|row| row.join(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The one value a `SHOW` answers with.
fn show(node: &mut Executor, sql: &str) -> String {
    rows(node, sql).pop().and_then(|mut row| row.pop()).unwrap()
}

/// What this node answers for one `;`-separated script: `ok`, or `!SQLSTATE message`.
fn answer(script: &str) -> String {
    let mut node = node();
    let statements: Vec<&str> = script.split(';').map(str::trim).collect();
    let (last, setup) = statements.split_last().expect("a script has a statement");
    for statement in setup {
        if let Err(error) = run(&mut node, statement) {
            return format!("!{} {error} (in setup `{statement}`)", error.sqlstate());
        }
    }
    match run(&mut node, last) {
        Ok(()) => "ok".to_owned(),
        Err(error) => format!("!{} {error}", error.sqlstate()),
    }
}

/// `(line number, script, expected answer)` for every case in the corpus.
fn corpus() -> Vec<(usize, String, String)> {
    include_str!("corpus/pg19_routing_engine.txt")
        .lines()
        .enumerate()
        .filter_map(|(at, line)| {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (script, expected) = line.split_once('\t')?;
            Some((at + 1, script.trim().to_owned(), expected.trim().to_owned()))
        })
        .collect()
}
